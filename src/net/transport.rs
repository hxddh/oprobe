//! `opendal_core::HttpTransport` built directly on hyper's low-level HTTP/1.1 and HTTP/2
//! connections with oprobe's own connection pool, so that every request is attributed
//! exactly: queue wait, fresh-dial phases, request send, TTFB, transfer and consumer stall.

use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use await_tree::InstrumentAwait;
use bytes::Bytes;
use futures::Stream;
use http::{HeaderValue, Method, Request, Response};
use http_body::{Body, Frame, SizeHint};
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use opendal_core::{Buffer, Error, ErrorKind, HttpBody, HttpRedirect, HttpTransport, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::dial::{ConnClock, DialTiming, Dialer};
use super::proxy::ProxyConfig;
use super::tls::{TlsClientOptions, build_tls_connector};
use crate::trace::{
    InflightGuard, OpScope, Protocol, Recorder, RequestRecord, RequestTiming, classify_http,
};

/// HTTP protocol preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HttpVersion {
    /// Negotiate via ALPN (h2 preferred for https), HTTP/1.1 for plain http.
    #[default]
    Auto,
    /// Force HTTP/1.1.
    H1,
    /// Require HTTP/2 (ALPN for https, prior knowledge for plain http).
    H2,
}

#[derive(Clone, Debug)]
pub struct TransportConfig {
    pub http: HttpVersion,
    pub keep_alive: bool,
    pub max_idle_per_host: usize,
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    /// Cap on concurrent in-flight HTTP requests (None = unlimited).
    pub max_inflight: Option<usize>,
    pub tls: TlsClientOptions,
    /// Explicit proxy; `None` = resolve from environment; `Some(None)` = disabled.
    pub proxy: Option<Option<String>>,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            http: HttpVersion::Auto,
            keep_alive: true,
            max_idle_per_host: 256,
            idle_timeout: Duration::from_secs(90),
            connect_timeout: Duration::from_secs(10),
            max_inflight: None,
            tls: TlsClientOptions::default(),
            proxy: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PoolKey {
    https: bool,
    host: String,
    port: u16,
}

struct IdleH1 {
    id: u64,
    sender: http1::SendRequest<TracedBody>,
    clock: Arc<ConnClock>,
    idle_since: Instant,
}

struct H2Conn {
    id: u64,
    sender: http2::SendRequest<TracedBody>,
    clock: Arc<ConnClock>,
}

#[derive(Default)]
struct HostPool {
    idle_h1: Vec<IdleH1>,
    h2: Vec<H2Conn>,
    /// Learned from the first dial: does this host speak h2?
    speaks_h2: Option<bool>,
    dial_lock: Option<Arc<tokio::sync::Mutex<()>>>,
}

struct Inner {
    cfg: TransportConfig,
    https_dialer: Dialer,
    http_dialer: Dialer,
    pool: Mutex<HashMap<PoolKey, HostPool>>,
    limiter: Option<Arc<Semaphore>>,
    recorder: Option<Arc<Recorder>>,
    conn_seq: AtomicU64,
}

/// White-box HTTP transport for OpenDAL.
#[derive(Clone)]
pub struct TracingTransport {
    inner: Arc<Inner>,
}

enum Checked {
    H1 {
        id: u64,
        sender: http1::SendRequest<TracedBody>,
        clock: Arc<ConnClock>,
    },
    H2 {
        id: u64,
        sender: http2::SendRequest<TracedBody>,
        clock: Arc<ConnClock>,
    },
}

impl TracingTransport {
    pub fn new(cfg: TransportConfig, recorder: Option<Arc<Recorder>>) -> anyhow::Result<Self> {
        let mut tls = cfg.tls.clone();
        tls.alpn_protocols = match cfg.http {
            HttpVersion::Auto => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            HttpVersion::H1 => vec![b"http/1.1".to_vec()],
            HttpVersion::H2 => vec![b"h2".to_vec()],
        };
        let connector = build_tls_connector(&tls)?;
        let proxy_for = |https: bool| -> anyhow::Result<Option<ProxyConfig>> {
            match &cfg.proxy {
                None => Ok(ProxyConfig::from_env(https)),
                Some(None) => Ok(None),
                // An explicitly configured proxy is always used (NO_PROXY only applies to env proxies).
                Some(Some(raw)) => Ok(Some(ProxyConfig::parse(raw, "")?)),
            }
        };
        let mk = |https: bool| -> anyhow::Result<Dialer> {
            Ok(Dialer::new(
                connector.clone(),
                proxy_for(https)?,
                cfg.connect_timeout,
                recorder.clone(),
            ))
        };
        let https_dialer = mk(true)?;
        let http_dialer = mk(false)?;
        Ok(Self {
            inner: Arc::new(Inner {
                limiter: cfg.max_inflight.map(|n| Arc::new(Semaphore::new(n.max(1)))),
                cfg,
                https_dialer,
                http_dialer,
                pool: Mutex::new(HashMap::new()),
                recorder,
                conn_seq: AtomicU64::new(1),
            }),
        })
    }

    /// Proxy that will be used for https targets, if any (for display).
    pub fn proxy_display(&self) -> Option<String> {
        self.inner.https_dialer.proxy.as_ref().map(|p| p.display())
    }

    /// Number of currently pooled connections (idle h1 + live h2).
    pub fn pooled_connections(&self) -> usize {
        self.inner
            .pool
            .lock()
            .map(|p| p.values().map(|h| h.idle_h1.len() + h.h2.len()).sum())
            .unwrap_or(0)
    }
}

impl Inner {
    fn lock_pool(&self) -> std::sync::MutexGuard<'_, HashMap<PoolKey, HostPool>> {
        self.pool.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take a pooled connection if one is usable.
    async fn checkout_pooled(&self, key: &PoolKey) -> Option<Checked> {
        if !self.cfg.keep_alive {
            return None;
        }
        loop {
            let candidate = {
                let mut pool = self.lock_pool();
                let hp = pool.entry(key.clone()).or_default();
                hp.h2.retain(|c| !c.sender.is_closed());
                if let Some(c) = hp.h2.first() {
                    return Some(Checked::H2 {
                        id: c.id,
                        sender: c.sender.clone(),
                        clock: c.clock.clone(),
                    });
                }
                let timeout = self.cfg.idle_timeout;
                hp.idle_h1
                    .retain(|c| !c.sender.is_closed() && c.idle_since.elapsed() < timeout);
                hp.idle_h1.pop()
            };
            let mut c = candidate?;
            match tokio::time::timeout(Duration::from_secs(1), c.sender.ready()).await {
                Ok(Ok(())) => {
                    return Some(Checked::H1 {
                        id: c.id,
                        sender: c.sender,
                        clock: c.clock,
                    });
                }
                _ => continue,
            }
        }
    }

    async fn dial_new(&self, key: &PoolKey) -> std::result::Result<(Checked, DialTiming), Error> {
        let dialer = if key.https {
            &self.https_dialer
        } else {
            &self.http_dialer
        };
        let d = dialer
            .dial(key.https, &key.host, key.port)
            .await
            .map_err(|e| {
                Error::new(ErrorKind::Unexpected, format!("connect: {e}"))
                    .with_operation("oprobe::dial")
                    .with_context("target", format!("{}:{}", key.host, key.port))
                    .set_temporary()
            })?;
        let id = self.conn_seq.fetch_add(1, Ordering::Relaxed);
        let use_h2 = match d.alpn.as_deref() {
            Some("h2") => true,
            Some(_) => false,
            None => self.cfg.http == HttpVersion::H2,
        };
        let hs_err = |e: hyper::Error| {
            Error::new(ErrorKind::Unexpected, format!("http handshake: {e}"))
                .with_operation("oprobe::handshake")
                .set_temporary()
        };
        let io = TokioIo::new(d.stream);
        let checked = if use_h2 {
            let (sender, conn) = http2::Builder::new(TokioExecutor::new())
                .adaptive_window(true)
                .handshake(io)
                .await
                .map_err(hs_err)?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            if self.cfg.keep_alive {
                let mut pool = self.lock_pool();
                let hp = pool.entry(key.clone()).or_default();
                hp.speaks_h2 = Some(true);
                hp.h2.push(H2Conn {
                    id,
                    sender: sender.clone(),
                    clock: d.clock.clone(),
                });
            }
            Checked::H2 {
                id,
                sender,
                clock: d.clock,
            }
        } else {
            let (sender, conn) = http1::Builder::new().handshake(io).await.map_err(hs_err)?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            self.lock_pool().entry(key.clone()).or_default().speaks_h2 = Some(false);
            Checked::H1 {
                id,
                sender,
                clock: d.clock,
            }
        };
        Ok((checked, d.timing))
    }

    async fn checkout(
        &self,
        key: &PoolKey,
    ) -> std::result::Result<(Checked, Option<DialTiming>), Error> {
        if let Some(c) = self.checkout_pooled(key).await {
            return Ok((c, None));
        }
        // Coalesce dials for hosts known to speak h2: one connection carries many streams.
        let lock = {
            let mut pool = self.lock_pool();
            let hp = pool.entry(key.clone()).or_default();
            if hp.speaks_h2 == Some(true) && self.cfg.keep_alive {
                Some(
                    hp.dial_lock
                        .get_or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                        .clone(),
                )
            } else {
                None
            }
        };
        if let Some(lock) = lock {
            let _g = lock.lock().await;
            if let Some(c) = self.checkout_pooled(key).await {
                return Ok((c, None));
            }
            let (c, t) = self.dial_new(key).await?;
            return Ok((c, Some(t)));
        }
        let (c, t) = self.dial_new(key).await?;
        Ok((c, Some(t)))
    }

    fn return_h1(
        &self,
        key: PoolKey,
        id: u64,
        sender: http1::SendRequest<TracedBody>,
        clock: Arc<ConnClock>,
    ) {
        if !self.cfg.keep_alive || sender.is_closed() {
            return;
        }
        let mut pool = self.lock_pool();
        let hp = pool.entry(key).or_default();
        if hp.idle_h1.len() < self.cfg.max_idle_per_host {
            hp.idle_h1.push(IdleH1 {
                id,
                sender,
                clock,
                idle_since: Instant::now(),
            });
        }
    }
}

/// Request body that records when its last byte was handed to the connection.
pub struct TracedBody {
    buf: Buffer,
    start: Instant,
    end_us: Arc<AtomicU64>,
}

impl Body for TracedBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, Infallible>>> {
        match self.buf.next() {
            Some(chunk) => {
                if self.buf.is_empty() {
                    // hyper stops polling once `is_end_stream()` is true: last chunk handed over.
                    let us = self.start.elapsed().as_micros() as u64;
                    self.end_us.store(us.max(1), Ordering::Release);
                }
                Poll::Ready(Some(Ok(Frame::data(chunk))))
            }
            None => {
                let us = self.start.elapsed().as_micros() as u64;
                self.end_us.store(us.max(1), Ordering::Release);
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.buf.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.buf.len() as u64)
    }
}

/// HTTP/1 connection to hand back to the pool once its response body is drained.
type H1Return = (
    Arc<Inner>,
    PoolKey,
    u64,
    http1::SendRequest<TracedBody>,
    Arc<ConnClock>,
);

/// Everything needed to finalize a request record when its response body completes.
struct Pending {
    record: RequestRecord,
    t_start: Instant,
    t_headers: Instant,
    stall: Duration,
    last_yield: Option<Instant>,
    scope: Option<Arc<OpScope>>,
    recorder: Option<Arc<Recorder>>,
    _permit: Option<OwnedSemaphorePermit>,
    _inflight: InflightGuard,
    h1_return: Option<H1Return>,
}

impl Pending {
    fn finish(mut self, complete: bool, error: Option<String>) {
        let now = Instant::now();
        if let Some(t) = self.last_yield.take() {
            // Data delivered but the consumer never came back for more: consumer time.
            self.stall += now.saturating_duration_since(t);
        }
        let body_total = now.saturating_duration_since(self.t_headers);
        let stall = self.stall.min(body_total);
        let t = &mut self.record.timing;
        t.stall_us = stall.as_micros() as u64;
        t.body_us = (body_total - stall).as_micros() as u64;
        t.total_us = now.saturating_duration_since(self.t_start).as_micros() as u64;
        if error.is_some() && self.record.error.is_none() {
            self.record.error = error;
        }
        if complete && let Some((inner, key, id, sender, clock)) = self.h1_return.take() {
            inner.return_h1(key, id, sender, clock);
        }
        match &self.scope {
            Some(s) => s.push_request(self.record),
            None => {
                if let Some(r) = &self.recorder {
                    r.record_unscoped_request(&self.record);
                }
            }
        }
    }
}

struct TracedResponseBody {
    incoming: Incoming,
    pending: Option<Pending>,
    uri: String,
    /// Declared body length, to tell a body read to its end from one dropped part-way.
    expected: Option<u64>,
}

impl Stream for TracedResponseBody {
    type Item = Result<Buffer>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(p) = this.pending.as_mut()
            && let Some(t) = p.last_yield.take()
        {
            p.stall += t.elapsed();
        }
        loop {
            match Pin::new(&mut this.incoming).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    if let Some(p) = this.pending.take() {
                        p.finish(true, None);
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        if data.is_empty() {
                            continue;
                        }
                        if let Some(p) = this.pending.as_mut() {
                            p.record.bytes_down += data.len() as u64;
                            p.last_yield = Some(Instant::now());
                        }
                        return Poll::Ready(Some(Ok(Buffer::from(data))));
                    }
                }
                Poll::Ready(Some(Err(err))) => {
                    let msg = format!("response body: {err}");
                    if let Some(p) = this.pending.take() {
                        p.finish(false, Some(msg.clone()));
                    }
                    return Poll::Ready(Some(Err(Error::new(ErrorKind::Unexpected, msg)
                        .with_operation("oprobe::read_body")
                        .with_context("url", this.uri.clone())
                        .set_temporary()
                        .set_source(err))));
                }
            }
        }
    }
}

impl Drop for TracedResponseBody {
    fn drop(&mut self) {
        if let Some(p) = self.pending.take() {
            // Body dropped before EOF: connection cannot be reused safely.
            let complete = self.incoming.is_end_stream();
            let got = p.record.bytes_down;
            let short = self.expected.filter(|&n| got < n);
            p.finish(
                complete,
                short.map(|n| format!("response body dropped after {got} of {n} bytes")),
            );
        }
    }
}

fn header_str(resp: &Response<Incoming>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

const REQUEST_ID_HEADERS: [&str; 7] = [
    "x-amz-request-id",
    "x-oss-request-id",
    "x-cos-request-id",
    "x-ms-request-id",
    "x-guploader-uploadid",
    "x-bz-request-id",
    "cf-ray",
];

impl HttpTransport for TracingTransport {
    async fn fetch(&self, req: Request<Buffer>) -> Result<Response<HttpBody>> {
        let inner = self.inner.clone();
        let t_start = Instant::now();
        let scope = OpScope::current();
        let recorder = inner.recorder.clone();

        let original_uri = req.uri().clone();
        let (mut parts, body_buf) = req.into_parts();
        let uri: http::Uri = match parts.extensions.get::<HttpRedirect>() {
            Some(redirect) => {
                let target: http::Uri = redirect.uri().original_uri().parse().map_err(|e| {
                    Error::new(ErrorKind::Unexpected, "invalid redirect target").set_source(e)
                })?;
                if target.authority() != original_uri.authority() {
                    for h in ["authorization", "proxy-authorization", "cookie"] {
                        parts.headers.remove(h);
                    }
                }
                target
            }
            None => original_uri.clone(),
        };
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            other => {
                return Err(Error::new(
                    ErrorKind::ConfigInvalid,
                    format!("unsupported URI scheme {other:?} in {uri}"),
                ));
            }
        };
        let host = uri
            .host()
            .ok_or_else(|| Error::new(ErrorKind::ConfigInvalid, format!("URI has no host: {uri}")))?
            .trim_matches(['[', ']'])
            .to_owned();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let key = PoolKey { https, host, port };
        let method = parts.method.clone();
        let api = classify_http(&method, uri.path(), uri.query());
        let bytes_up = body_buf.len() as u64;

        let inflight = InflightGuard::start(recorder.clone());
        let start_offset_us = scope
            .as_ref()
            .map(|s| t_start.saturating_duration_since(s.started).as_micros() as u64)
            .unwrap_or(0);
        let mut record = RequestRecord {
            api,
            method: method.to_string(),
            status: 0,
            protocol: Protocol::Http1,
            reused: false,
            conn_id: 0,
            start_offset_us,
            timing: RequestTiming::default(),
            bytes_up,
            bytes_down: 0,
            request_id: None,
            retry_after_ms: None,
            error: None,
        };

        // A request dropped before its response (timeout layer, tail-cut) still gets a record.
        let mut cancel_guard = super::fidelity::CancelGuard::new(
            api,
            method.to_string(),
            bytes_up,
            start_offset_us,
            t_start,
            scope.clone(),
            recorder.clone(),
        );

        // Early failures still produce a request record so errors are attributed.
        let fail = |mut record: RequestRecord, msg: String, err: Error| -> Error {
            record.error = Some(msg);
            record.timing.total_us = t_start.elapsed().as_micros() as u64;
            record.timing.queue_us = record.timing.total_us;
            match &scope {
                Some(s) => s.push_request(record),
                None => {
                    if let Some(r) = &recorder {
                        r.record_unscoped_request(&record);
                    }
                }
            }
            err
        };

        // 1. Queue: in-flight permit.
        let permit = match &inner.limiter {
            Some(sem) => Some(
                sem.clone()
                    .acquire_owned()
                    .instrument_await("http::wait_inflight_permit")
                    .await
                    .expect("limiter never closed"),
            ),
            None => None,
        };

        // 2. Connection: pooled or freshly dialed (timed phase by phase).
        let (checked, dial) = match inner
            .checkout(&key)
            .instrument_await(await_tree::span!("http::checkout {}", key.host))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                cancel_guard.disarm();
                return Err(fail(record, e.to_string(), e));
            }
        };
        let t_ready = Instant::now();
        let dial_us = dial.map(|d| d.total_us()).unwrap_or(0);
        if let Some(d) = dial {
            record.timing.dns_us = d.dns_us;
            record.timing.tcp_us = d.tcp_us;
            record.timing.proxy_us = d.proxy_us;
            record.timing.tls_us = d.tls_us;
        }
        record.reused = dial.is_none();
        record.timing.queue_us =
            (t_ready.saturating_duration_since(t_start).as_micros() as u64).saturating_sub(dial_us);

        // 3. Build the request.
        let end_us = Arc::new(AtomicU64::new(0));
        let body = TracedBody {
            buf: body_buf,
            start: t_start,
            end_us: end_us.clone(),
        };
        let mut builder = Request::builder().method(method.clone());
        let (id, clock) = match &checked {
            Checked::H1 { id, clock, .. } | Checked::H2 { id, clock, .. } => (*id, clock.clone()),
        };
        record.conn_id = id;
        match &checked {
            Checked::H1 { .. } => {
                record.protocol = Protocol::Http1;
                // HTTP/1 wants origin-form plus Host header.
                let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
                builder = builder.uri(pq).version(http::Version::HTTP_11);
                if !parts.headers.contains_key(http::header::HOST)
                    && let Some(a) = uri.authority()
                    && let Ok(v) = HeaderValue::from_str(a.as_str())
                {
                    parts.headers.insert(http::header::HOST, v);
                }
                if !inner.cfg.keep_alive {
                    parts
                        .headers
                        .insert(http::header::CONNECTION, HeaderValue::from_static("close"));
                }
            }
            Checked::H2 { .. } => {
                record.protocol = Protocol::Http2;
                parts.headers.remove(http::header::HOST);
                parts.headers.remove(http::header::CONNECTION);
                builder = builder.uri(uri.clone()).version(http::Version::HTTP_2);
            }
        }
        let mut hreq = builder
            .body(body)
            .map_err(|e| Error::new(ErrorKind::Unexpected, "build http request").set_source(e))?;
        *hreq.headers_mut() = parts.headers;

        // 4. Send and await response headers.
        let t_dispatch = Instant::now();
        let span = format!("http::{} {} {}", api.label(), method, uri.path());
        let (resp, h1_sender) = match checked {
            Checked::H1 { mut sender, .. } => {
                let r = sender.send_request(hreq).instrument_await(span).await;
                (r, Some(sender))
            }
            Checked::H2 { mut sender, .. } => {
                let r = async {
                    sender.ready().await?;
                    sender.send_request(hreq).await
                }
                .instrument_await(span)
                .await;
                (r, None)
            }
        };
        let t_headers = Instant::now();
        cancel_guard.disarm();
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                // Anything but a malformed request/response is a transport failure (reset, abort,
                // early close while the body was still being sent — common after a 503 on
                // Windows) and safe for the retry layer to try again.
                let temporary = !(e.is_user() || e.is_parse());
                let msg = format!("send request: {e}");
                record.timing.send_us =
                    t_headers.saturating_duration_since(t_ready).as_micros() as u64;
                let err = Error::new(ErrorKind::Unexpected, msg.clone())
                    .with_operation("oprobe::send_request")
                    .with_context("url", original_uri.to_string())
                    .with_temporary(temporary)
                    .set_source(e);
                record.error = Some(msg.clone());
                record.timing.total_us = t_start.elapsed().as_micros() as u64;
                match &scope {
                    Some(s) => s.push_request(record),
                    None => {
                        if let Some(r) = &recorder {
                            r.record_unscoped_request(&record);
                        }
                    }
                }
                return Err(err);
            }
        };

        // Request fully sent: exact socket write time on HTTP/1 (one request per connection
        // at a time), body-completion time on HTTP/2 (multiplexed socket).
        let sent_at = if h1_sender.is_some() {
            let lw = clock.last_write();
            if lw >= t_dispatch && lw <= t_headers {
                lw
            } else {
                t_dispatch
            }
        } else {
            let end = end_us.load(Ordering::Acquire);
            let at = t_start + Duration::from_micros(end);
            if end > 0 && bytes_up > 0 && at >= t_dispatch && at <= t_headers {
                at
            } else {
                t_dispatch
            }
        };
        record.timing.send_us = sent_at.saturating_duration_since(t_ready).as_micros() as u64;
        record.timing.ttfb_us = t_headers.saturating_duration_since(sent_at).as_micros() as u64;
        record.status = resp.status().as_u16();
        record.request_id = REQUEST_ID_HEADERS.iter().find_map(|h| header_str(&resp, h));
        if record.is_throttle() {
            record.retry_after_ms = header_str(&resp, "retry-after")
                .as_deref()
                .and_then(super::parse_retry_after);
        }

        let content_length = if method == Method::HEAD
            || resp.headers().contains_key(http::header::CONTENT_ENCODING)
        {
            None
        } else {
            resp.headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
        };

        let (rparts, incoming) = resp.into_parts();
        let mut pending = Pending {
            record,
            t_start,
            t_headers,
            stall: Duration::ZERO,
            // Until the consumer first polls the body, the bytes wait on it, not the network.
            last_yield: Some(t_headers),
            scope,
            recorder,
            _permit: permit,
            _inflight: inflight,
            h1_return: h1_sender.map(|s| (inner.clone(), key, id, s, clock)),
        };
        let mut out = Response::builder()
            .status(rparts.status)
            .version(rparts.version)
            .extension(original_uri.clone());
        *out.headers_mut().expect("fresh builder") = rparts.headers;

        let pending = if incoming.is_end_stream() {
            pending.record.bytes_down = 0;
            pending.finish(true, None);
            None
        } else {
            Some(pending)
        };
        let stream = TracedResponseBody {
            incoming,
            pending,
            uri: original_uri.to_string(),
            expected: content_length,
        };
        Ok(out
            .body(HttpBody::new(stream, content_length))
            .expect("valid response"))
    }
}
