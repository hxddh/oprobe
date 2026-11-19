//! Production-fidelity transport: OpenDAL's own HTTP client (reqwest 0.13 + rustls/aws-lc-rs,
//! exactly as `opendal-http-transport-reqwest` builds it), observed only through reqwest's
//! public extension points:
//!
//! - a timed DNS resolver (`ClientBuilder::dns_resolver`, same `getaddrinfo` path as reqwest's
//!   default resolver),
//! - a connector layer (`ClientBuilder::connector_layer`) that times every dial and learns the
//!   connection's local/remote address,
//! - a delegating TLS session store that timestamps the ClientHello, splitting TCP from TLS,
//! - the response's `HttpInfo` (local address = connection identity), which marks the first
//!   request on every connection as cold and attributes that connection's dial to it.
//!
//! What this mode cannot see (use `--microscope`): time waiting inside reqwest's pool for an
//! idle connection or HTTP/2 stream is part of TTFB, and the TLS version/cipher are not exposed.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use await_tree::InstrumentAwait;
use bytes::Bytes;
use futures::Stream;
use http::{Method, Request, Response};
use hyper_util::client::legacy::connect::{Connection, HttpInfo};
use opendal_core::raw::{parse_content_encoding, parse_content_length};
use opendal_core::{Buffer, Error, ErrorKind, HttpBody, HttpRedirect, HttpTransport, Result};
use rustls::client::{ClientSessionMemoryCache, ClientSessionStore, Resumption};
use rustls::{ClientConfig, NamedGroup};
use rustls_pki_types::ServerName;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::tls::{crypto_provider, InsecureCertVerifier};
use super::{HttpVersion, TransportConfig};
use crate::trace::model::{classify_http, Protocol, RequestRecord, RequestTiming};
use crate::trace::recorder::DialRecord;
use crate::trace::{OpScope, Recorder};

// ---------------------------------------------------------------------------------------------
// Dial timing (task-local while reqwest's connector future runs)

#[derive(Default, Debug, Clone, Copy)]
struct DialMarks {
    dns_start: Option<Instant>,
    dns_end: Option<Instant>,
    client_hello: Option<Instant>,
    tls_version: Option<&'static str>,
}

tokio::task_local! {
    static DIAL: Arc<Mutex<DialMarks>>;
}

fn mark(f: impl FnOnce(&mut DialMarks)) {
    let _ = DIAL.try_with(|m| {
        if let Ok(mut g) = m.lock() {
            f(&mut g)
        }
    });
}

/// A finished dial waiting for the first request that uses its connection.
#[derive(Clone, Copy, Debug)]
struct Dialed {
    dns_us: u64,
    tcp_us: u64,
    proxy_us: u64,
    tls_us: u64,
    end: Instant,
}

#[derive(Default)]
struct Conns {
    seq: u64,
    /// Connection identity (local address) -> (id, dial awaiting its first request).
    by_local: HashMap<SocketAddr, (u64, Option<Dialed>)>,
}

struct Shared {
    recorder: Option<Arc<Recorder>>,
    conns: Mutex<Conns>,
    /// Configured egress proxy (display form) and its addresses: a dial whose peer is one of
    /// them went through the proxy (reqwest applies NO_PROXY itself).
    proxy: Option<String>,
    proxy_addrs: Vec<SocketAddr>,
}

// ---------------------------------------------------------------------------------------------
// DNS: timed getaddrinfo, like reqwest's default GaiResolver.

struct TimedResolver;

impl reqwest::dns::Resolve for TimedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            mark(|m| m.dns_start = Some(Instant::now()));
            let res = tokio::net::lookup_host((host.as_str(), 0)).await;
            mark(|m| m.dns_end = Some(Instant::now()));
            let addrs: Vec<SocketAddr> = res?.collect();
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// TLS: delegate to rustls' default in-memory session cache, timestamp the ClientHello.

#[derive(Debug)]
struct HelloClock(Arc<ClientSessionMemoryCache>);

impl ClientSessionStore for HelloClock {
    fn set_kx_hint(&self, server_name: ServerName<'static>, group: NamedGroup) {
        self.0.set_kx_hint(server_name, group)
    }
    fn kx_hint(&self, server_name: &ServerName<'_>) -> Option<NamedGroup> {
        // Called while building the ClientHello, i.e. right after TCP (and proxy CONNECT).
        mark(|m| {
            m.client_hello.get_or_insert_with(Instant::now);
        });
        self.0.kx_hint(server_name)
    }
    fn set_tls12_session(
        &self,
        server_name: ServerName<'static>,
        value: rustls::client::Tls12ClientSessionValue,
    ) {
        self.0.set_tls12_session(server_name, value)
    }
    fn tls12_session(
        &self,
        server_name: &ServerName<'_>,
    ) -> Option<rustls::client::Tls12ClientSessionValue> {
        self.0.tls12_session(server_name)
    }
    fn remove_tls12_session(&self, server_name: &ServerName<'static>) {
        self.0.remove_tls12_session(server_name)
    }
    fn insert_tls13_ticket(
        &self,
        server_name: ServerName<'static>,
        value: rustls::client::Tls13ClientSessionValue,
    ) {
        self.0.insert_tls13_ticket(server_name, value)
    }
    fn take_tls13_ticket(
        &self,
        server_name: &ServerName<'static>,
    ) -> Option<rustls::client::Tls13ClientSessionValue> {
        self.0.take_tls13_ticket(server_name)
    }
}

/// Learns the negotiated TLS version from the key-schedule labels. Never receives a TLS 1.3
/// secret (`will_log` returns false); TLS 1.2's single `log` call is ignored.
#[derive(Debug)]
struct VersionProbe;

impl rustls::KeyLog for VersionProbe {
    fn log(&self, label: &str, _client_random: &[u8], _secret: &[u8]) {
        if label == "CLIENT_RANDOM" {
            mark(|m| m.tls_version = Some("TLSv1.2"));
        }
    }
    fn will_log(&self, label: &str) -> bool {
        if label.starts_with("CLIENT_HANDSHAKE_TRAFFIC_SECRET") {
            mark(|m| m.tls_version = Some("TLSv1.3"));
        }
        false
    }
}

/// The rustls config reqwest 0.13 builds by default (aws-lc-rs, TLS 1.2 + 1.3, platform
/// verifier, extra roots merged), plus the ClientHello clock.
fn rustls_config(cfg: &TransportConfig, alpn: Vec<Vec<u8>>) -> anyhow::Result<ClientConfig> {
    let provider = crypto_provider();
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(rustls::ALL_VERSIONS)?;
    let mut tls = if cfg.tls.insecure_skip_verify {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(InsecureCertVerifier { provider }))
            .with_no_client_auth()
    } else {
        let extra: Vec<rustls_pki_types::CertificateDer<'static>> = match &cfg.tls.extra_ca_pem {
            Some(pem) => {
                use rustls_pki_types::pem::PemObject;
                rustls_pki_types::CertificateDer::pem_slice_iter(pem.as_bytes())
                    .collect::<std::result::Result<_, _>>()?
            }
            None => Vec::new(),
        };
        let verifier = if extra.is_empty() {
            rustls_platform_verifier::Verifier::new(provider)?
        } else {
            #[cfg(any(all(unix, not(target_os = "android")), target_os = "windows"))]
            {
                rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider)?
            }
            #[cfg(not(any(all(unix, not(target_os = "android")), target_os = "windows")))]
            {
                anyhow::bail!("extra CA certificates are not supported on this platform")
            }
        };
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth()
    };
    tls.alpn_protocols = alpn;
    tls.key_log = Arc::new(VersionProbe);
    tls.resumption = Resumption::store(Arc::new(HelloClock(Arc::new(
        ClientSessionMemoryCache::new(256),
    ))));
    Ok(tls)
}

// ---------------------------------------------------------------------------------------------
// Connector layer: time each dial, key it by the connection's local address.

#[derive(Clone)]
struct DialLayer(Arc<Shared>);

#[derive(Clone)]
struct DialService<S> {
    inner: S,
    shared: Arc<Shared>,
}

impl<S> tower_layer::Layer<S> for DialLayer {
    type Service = DialService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        DialService {
            inner,
            shared: self.0.clone(),
        }
    }
}

impl<S, R> tower_service::Service<R> for DialService<S>
where
    S: tower_service::Service<R>,
    S::Response: Connection,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let shared = self.shared.clone();
        let marks = Arc::new(Mutex::new(DialMarks::default()));
        let fut = DIAL
            .scope(marks.clone(), self.inner.call(req))
            .instrument_await("http::dial");
        Box::pin(async move {
            let start = Instant::now();
            let res = fut.await;
            let end = Instant::now();
            let m = marks.lock().map(|g| *g).unwrap_or_default();
            let us = |a: Instant, b: Instant| b.saturating_duration_since(a).as_micros() as u64;
            let dns_us = match (m.dns_start, m.dns_end) {
                (Some(a), Some(b)) => us(a, b),
                _ => 0,
            };
            let tcp_from = m.dns_end.unwrap_or(start);
            let (mut tcp_us, tls_us) = match m.client_hello {
                Some(h) => (us(tcp_from, h), us(h, end)),
                None => (us(tcp_from, end), 0),
            };
            match &res {
                Ok(conn) => {
                    let connected = conn.connected();
                    let mut ext = http::Extensions::new();
                    connected.get_extras(&mut ext);
                    let info = ext.get::<HttpInfo>().cloned();
                    let proxied = info
                        .as_ref()
                        .is_some_and(|i| shared.proxy_addrs.contains(&i.remote_addr()));
                    // Through a proxy, TCP to the proxy and the CONNECT exchange are not
                    // separable from outside reqwest: report them together as proxy time.
                    let proxy_us = if proxied && tls_us > 0 {
                        std::mem::take(&mut tcp_us)
                    } else {
                        0
                    };
                    if let Some(r) = &shared.recorder {
                        r.record_dial(DialRecord {
                            ok: true,
                            proxy: shared.proxy.clone().filter(|_| proxied),
                            dns_us,
                            tcp_us,
                            proxy_us,
                            tls_us,
                            remote: info
                                .as_ref()
                                .map(|i| i.remote_addr().to_string())
                                .unwrap_or_default(),
                            alpn: if connected.is_negotiated_h2() {
                                "h2".into()
                            } else if tls_us > 0 {
                                "http/1.1".into()
                            } else {
                                String::new()
                            },
                            tls_version: m.tls_version.unwrap_or_default().to_owned(),
                            cipher: String::new(),
                        });
                    }
                    if let (Some(i), Ok(mut c)) = (info, shared.conns.lock()) {
                        c.seq += 1;
                        let id = c.seq;
                        c.by_local.insert(
                            i.local_addr(),
                            (
                                id,
                                Some(Dialed {
                                    dns_us,
                                    tcp_us,
                                    proxy_us,
                                    tls_us,
                                    end,
                                }),
                            ),
                        );
                    }
                }
                Err(_) => {
                    if let Some(r) = &shared.recorder {
                        r.record_dial(DialRecord {
                            ok: false,
                            proxy: None,
                            dns_us,
                            tcp_us,
                            proxy_us: 0,
                            tls_us,
                            remote: String::new(),
                            alpn: String::new(),
                            tls_version: String::new(),
                            cipher: String::new(),
                        });
                    }
                }
            }
            res
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The transport

/// OpenDAL's default reqwest client, observed request by request.
#[derive(Clone)]
pub struct FidelityTransport {
    client: reqwest::Client,
    shared: Arc<Shared>,
    limiter: Option<Arc<Semaphore>>,
}

impl FidelityTransport {
    pub fn new(
        cfg: TransportConfig,
        https: bool,
        recorder: Option<Arc<Recorder>>,
    ) -> anyhow::Result<Self> {
        let proxy = match &cfg.proxy {
            Some(Some(p)) => Some(super::ProxyConfig::parse(p, "")?),
            Some(None) => None,
            None => super::ProxyConfig::from_env(https),
        };
        let proxy_addrs = proxy
            .as_ref()
            .and_then(|p| {
                use std::net::ToSocketAddrs;
                (p.host.as_str(), p.port).to_socket_addrs().ok()
            })
            .map(|a| a.collect())
            .unwrap_or_default();
        let shared = Arc::new(Shared {
            recorder,
            conns: Mutex::new(Conns::default()),
            proxy: proxy.map(|p| p.display()),
            proxy_addrs,
        });
        // OpenDAL builds reqwest without the `http2` feature, so its default is HTTP/1.1.
        let alpn: Vec<Vec<u8>> = match cfg.http {
            HttpVersion::Auto | HttpVersion::H1 => vec![b"http/1.1".to_vec()],
            HttpVersion::H2 => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        };
        let mut b = reqwest::Client::builder()
            .tls_backend_preconfigured(rustls_config(&cfg, alpn)?)
            .dns_resolver(Arc::new(TimedResolver))
            .connector_layer(DialLayer(shared.clone()))
            .connect_timeout(cfg.connect_timeout)
            .pool_idle_timeout(cfg.idle_timeout)
            .pool_max_idle_per_host(if cfg.keep_alive {
                cfg.max_idle_per_host
            } else {
                0
            });
        b = match cfg.http {
            HttpVersion::Auto | HttpVersion::H1 => b.http1_only(),
            HttpVersion::H2 if !https => b.http2_prior_knowledge(),
            HttpVersion::H2 => b,
        };
        b = match &cfg.proxy {
            None => b,
            Some(None) => b.no_proxy(),
            Some(Some(p)) => b.proxy(reqwest::Proxy::all(p.as_str())?),
        };
        Ok(Self {
            client: b.build()?,
            shared,
            limiter: cfg.max_inflight.map(|n| Arc::new(Semaphore::new(n.max(1)))),
        })
    }
}

impl FidelityTransport {
    /// The underlying reqwest client (for benchmarks comparing against an untraced client).
    #[doc(hidden)]
    pub fn reqwest_client(&self) -> reqwest::Client {
        self.client.clone()
    }
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

/// OpenDAL's classification of reqwest errors (opendal-http-transport-reqwest).
fn is_temporary_error(err: &reqwest::Error) -> bool {
    err.is_request() || err.is_body() || err.is_decode()
}

/// Request body that notes when its last byte was handed to the connection.
struct TimedBody {
    buf: Buffer,
    start: Instant,
    end_us: Arc<AtomicU64>,
}

impl http_body::Body for TimedBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<http_body::Frame<Bytes>, Self::Error>>> {
        let next = self.buf.next();
        if self.buf.is_empty() {
            let us = (self.start.elapsed().as_micros() as u64).max(1);
            self.end_us.store(us, Ordering::Release);
        }
        Poll::Ready(next.map(|b| Ok(http_body::Frame::data(b))))
    }

    fn is_end_stream(&self) -> bool {
        self.buf.is_empty()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.buf.len() as u64)
    }
}

struct Finisher {
    record: RequestRecord,
    t_start: Instant,
    t_headers: Instant,
    stall: Duration,
    last_yield: Option<Instant>,
    scope: Option<Arc<OpScope>>,
    recorder: Option<Arc<Recorder>>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl Finisher {
    fn emit(record: RequestRecord, scope: &Option<Arc<OpScope>>, rec: &Option<Arc<Recorder>>) {
        if let Some(r) = rec {
            r.request_finished();
        }
        match scope {
            Some(s) => s.push_request(record),
            None => {
                if let Some(r) = rec {
                    r.record_unscoped_request(&record);
                }
            }
        }
    }

    fn finish(mut self, error: Option<String>) {
        let now = Instant::now();
        if let Some(t) = self.last_yield.take() {
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
        Self::emit(self.record, &self.scope, &self.recorder);
    }
}

struct TimedResponseBody<S> {
    inner: Pin<Box<S>>,
    fin: Option<Finisher>,
    uri: String,
}

impl<S: Stream<Item = reqwest::Result<Bytes>>> Stream for TimedResponseBody<S> {
    type Item = Result<Buffer>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(f) = this.fin.as_mut() {
            if let Some(t) = f.last_yield.take() {
                f.stall += t.elapsed();
            }
        }
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    if let Some(f) = this.fin.take() {
                        f.finish(None);
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Ok(b))) => {
                    if b.is_empty() {
                        continue;
                    }
                    if let Some(f) = this.fin.as_mut() {
                        f.record.bytes_down += b.len() as u64;
                        f.last_yield = Some(Instant::now());
                    }
                    return Poll::Ready(Some(Ok(Buffer::from(b))));
                }
                Poll::Ready(Some(Err(err))) => {
                    let msg = format!("response body: {err}");
                    if let Some(f) = this.fin.take() {
                        f.finish(Some(msg));
                    }
                    let temporary = is_temporary_error(&err);
                    return Poll::Ready(Some(Err(Error::new(
                        ErrorKind::Unexpected,
                        "read data from http response",
                    )
                    .with_operation("reqwest::fetch")
                    .with_context("url", this.uri.clone())
                    .with_temporary(temporary)
                    .set_source(err.without_url()))));
                }
            }
        }
    }
}

impl<S> Drop for TimedResponseBody<S> {
    fn drop(&mut self) {
        if let Some(f) = self.fin.take() {
            f.finish(None);
        }
    }
}

impl HttpTransport for FidelityTransport {
    async fn fetch(&self, req: Request<Buffer>) -> Result<Response<HttpBody>> {
        let t_start = Instant::now();
        let scope = OpScope::current();
        let recorder = self.shared.recorder.clone();

        // --- identical to opendal-http-transport-reqwest ---------------------------------
        let uri = req.uri().clone();
        let is_head = req.method() == Method::HEAD;
        let (mut parts, body) = req.into_parts();
        let original = uri.to_string();
        let target = parts
            .extensions
            .get::<HttpRedirect>()
            .map(|redirect| redirect.uri().original_uri())
            .unwrap_or(&original);
        let url = reqwest::Url::parse(target).map_err(|err| {
            Error::new(ErrorKind::Unexpected, "request url is invalid")
                .with_operation("reqwest::fetch")
                .with_context("url", uri.to_string())
                .set_source(err)
        })?;
        if parts.extensions.get::<HttpRedirect>().is_some() {
            let original = reqwest::Url::parse(&uri.to_string()).map_err(|err| {
                Error::new(ErrorKind::Unexpected, "original request url is invalid").set_source(err)
            })?;
            if original.origin() != url.origin() {
                for name in [
                    "authorization",
                    "proxy-authorization",
                    "cookie",
                    "cookie2",
                    "www-authenticate",
                ] {
                    parts.headers.insert(
                        http::header::HeaderName::from_static(name),
                        http::HeaderValue::from_static(""),
                    );
                }
            }
        }
        // ---------------------------------------------------------------------------------

        let method = parts.method.clone();
        let api = classify_http(&method, url.path(), url.query());
        let bytes_up = body.len() as u64;
        if let Some(r) = &recorder {
            r.request_started();
        }
        let mut record = RequestRecord {
            api,
            method: method.to_string(),
            status: 0,
            protocol: Protocol::Http1,
            reused: true,
            conn_id: 0,
            start_offset_us: scope
                .as_ref()
                .map(|s| t_start.saturating_duration_since(s.started).as_micros() as u64)
                .unwrap_or(0),
            timing: RequestTiming::default(),
            bytes_up,
            bytes_down: 0,
            request_id: None,
            retry_after_ms: None,
            error: None,
        };

        let permit = match &self.limiter {
            Some(sem) => Some(
                sem.clone()
                    .acquire_owned()
                    .instrument_await("http::wait_inflight_permit")
                    .await
                    .expect("never closed"),
            ),
            None => None,
        };
        let t_ready = Instant::now();
        record.timing.queue_us = t_ready.saturating_duration_since(t_start).as_micros() as u64;

        let end_us = Arc::new(AtomicU64::new(0));
        let mut rb = self
            .client
            .request(parts.method, url)
            .headers(parts.headers)
            .version(parts.version);
        if !body.is_empty() {
            rb = rb.body(reqwest::Body::wrap(TimedBody {
                buf: body,
                start: t_start,
                end_us: end_us.clone(),
            }));
        }

        // Static span name: no allocation per request.
        let sent = rb.send().instrument_await(api.label()).await;
        let t_headers = Instant::now();
        let mut resp = match sent {
            Ok(r) => r,
            Err(err) => {
                let msg = format!("send http request: {err}");
                record.error = Some(msg);
                record.timing.ttfb_us =
                    t_headers.saturating_duration_since(t_ready).as_micros() as u64;
                record.timing.total_us =
                    t_headers.saturating_duration_since(t_start).as_micros() as u64;
                Finisher::emit(record, &scope, &recorder);
                return Err(Error::new(ErrorKind::Unexpected, "send http request")
                    .with_operation("reqwest::send")
                    .with_context("url", uri.to_string())
                    .with_temporary(is_temporary_error(&err))
                    .set_source(err.without_url()));
            }
        };

        // Connection identity: the first request seen on a local address is the cold one and
        // carries that connection's dial.
        let local = resp.extensions().get::<HttpInfo>().map(|i| i.local_addr());
        let mut dial = None;
        if let (Some(local), Ok(mut c)) = (local, self.shared.conns.lock()) {
            let c = &mut *c;
            let entry = c.by_local.entry(local).or_insert_with(|| {
                c.seq += 1;
                (c.seq, None)
            });
            record.conn_id = entry.0;
            dial = entry.1.take();
        }
        record.protocol = if resp.version() == http::Version::HTTP_2 {
            Protocol::Http2
        } else {
            Protocol::Http1
        };
        // Time on the wire starts when the connection was ready.
        let mut wire_from = t_ready;
        if let Some(d) = dial {
            record.reused = false;
            record.timing.dns_us = d.dns_us;
            record.timing.tcp_us = d.tcp_us;
            record.timing.proxy_us = d.proxy_us;
            record.timing.tls_us = d.tls_us;
            if d.end > t_ready && d.end <= t_headers {
                wire_from = d.end;
            }
        }
        let sent_at = {
            let end = end_us.load(Ordering::Acquire);
            let at = t_start + Duration::from_micros(end);
            if end > 0 && at >= wire_from && at <= t_headers {
                at
            } else {
                wire_from
            }
        };
        record.timing.send_us = sent_at.saturating_duration_since(wire_from).as_micros() as u64;
        record.timing.ttfb_us = t_headers.saturating_duration_since(sent_at).as_micros() as u64;
        record.status = resp.status().as_u16();
        let header = |n: &str| {
            resp.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        record.request_id = REQUEST_ID_HEADERS.iter().find_map(|h| header(h));
        if record.is_throttle() {
            record.retry_after_ms = header("retry-after")
                .and_then(|v| v.trim().parse::<f64>().ok())
                .map(|s| (s * 1000.0) as u64);
        }

        // --- identical to opendal-http-transport-reqwest ---------------------------------
        let content_length = if is_head || parse_content_encoding(resp.headers())?.is_some() {
            None
        } else {
            parse_content_length(resp.headers())?
        };
        let mut hr = Response::builder()
            .status(resp.status())
            .extension(uri.clone());
        if let Ok(target) = resp.url().as_str().parse::<http::Uri>() {
            if target != uri {
                let redirect = parts
                    .extensions
                    .get::<HttpRedirect>()
                    .filter(|redirect| redirect.uri().original_uri() == resp.url().as_str())
                    .cloned()
                    .unwrap_or_else(|| HttpRedirect::new(target));
                hr = hr.extension(redirect);
            }
        }
        hr = hr.version(resp.version());
        std::mem::swap(hr.headers_mut().expect("fresh builder"), resp.headers_mut());
        // ---------------------------------------------------------------------------------

        let fin = Finisher {
            record,
            t_start,
            t_headers,
            stall: Duration::ZERO,
            last_yield: None,
            scope,
            recorder,
            _permit: permit,
        };
        let stream = TimedResponseBody {
            inner: Box::pin(resp.bytes_stream()),
            fin: Some(fin),
            uri: uri.to_string(),
        };
        Ok(hr
            .body(HttpBody::new(stream, content_length))
            .expect("response must build"))
    }
}
