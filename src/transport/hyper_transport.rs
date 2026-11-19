//! `opendal_core::HttpTransport` implementation backed directly by `hyper_util::client::legacy::Client`
//! and [`ProbedHttpConnector`], providing zero-estimation cold/warm connection attribution,
//! exact post-write gateway TTFB, leak-free RAII inflight tracking, and body streaming telemetry.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use await_tree::InstrumentAwait;
use bytes::Bytes;
use futures::Stream;
use http::{HeaderName, HeaderValue, Method, Request, Response};
use http_body::{Body, Frame, SizeHint};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use opendal_core::raw::{parse_content_encoding, parse_content_length};
use opendal_core::{
    Buffer, Error, ErrorKind, HttpBody, HttpRedirect, HttpTransport, HttpTransporter, Result,
};

use super::connector::{ConnectionTelemetry, ProbedConnection, ProbedHttpConnector};
use crate::telemetry::{OpTraceScope, TelemetryRegistry};
use crate::tls::TlsClientOptions;

static DEFAULT_FALLBACK_TRANSPORT: LazyLock<HyperWhiteboxTransport> = LazyLock::new(|| {
    HyperWhiteboxTransport::try_new(HyperTransportOptions::default(), None)
        .expect("default pure-Rust HyperWhiteboxTransport must initialize")
});

/// Install the pure-Rust `HyperWhiteboxTransport` as OpenDAL's global default HTTP transporter.
pub fn install_default_hyper_transport() {
    struct GlobalDefaultTransport;
    impl HttpTransport for GlobalDefaultTransport {
        async fn fetch(&self, req: Request<Buffer>) -> Result<Response<HttpBody>> {
            DEFAULT_FALLBACK_TRANSPORT.fetch(req).await
        }
    }
    HttpTransporter::install_default(GlobalDefaultTransport);
}

/// Configuration options for [`HyperWhiteboxTransport`].
#[derive(Clone, Debug)]
pub struct HyperTransportOptions {
    pub pool_max_idle_per_host: usize,
    pub keep_alive: bool,
    pub connect_timeout: Duration,
    pub tls: TlsClientOptions,
}

impl Default for HyperTransportOptions {
    fn default() -> Self {
        Self {
            pool_max_idle_per_host: 32,
            keep_alive: true,
            connect_timeout: Duration::from_secs(10),
            tls: TlsClientOptions::default(),
        }
    }
}

#[derive(Clone)]
struct BufferRequestBody {
    buf: Buffer,
}

impl Body for BufferRequestBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        match self.buf.next() {
            Some(chunk) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
            None => Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.buf.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.buf.len() as u64)
    }
}

/// Leak-free RAII guard tracking active HTTP inflight concurrency and body stream completion time.
struct InflightStreamGuard {
    telemetry: Option<Arc<TelemetryRegistry>>,
    body_started_at: Option<Instant>,
}

impl InflightStreamGuard {
    fn enter(telemetry: Option<Arc<TelemetryRegistry>>) -> Self {
        if let Some(ref tel) = telemetry {
            tel.record_inflight_enter();
        }
        Self {
            telemetry,
            body_started_at: None,
        }
    }

    fn start_body_phase(&mut self) {
        self.body_started_at = Some(Instant::now());
    }
}

impl Drop for InflightStreamGuard {
    fn drop(&mut self) {
        if let Some(ref tel) = self.telemetry {
            if let Some(t_body) = self.body_started_at {
                tel.record_body_stream(t_body.elapsed().as_micros() as u64);
            }
            tel.record_inflight_exit();
        }
    }
}

struct ProbedIncomingStream {
    incoming: Incoming,
    uri_label: String,
    is_object_read: bool,
    telemetry: Option<Arc<TelemetryRegistry>>,
    _guard: InflightStreamGuard,
}

impl Stream for ProbedIncomingStream {
    type Item = Result<Buffer>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match Pin::new(&mut this.incoming).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        if data.is_empty() {
                            continue;
                        }
                        if this.is_object_read {
                            if let Some(ref tel) = this.telemetry {
                                tel.record_storage_read_bytes(data.len() as u64);
                            }
                        }
                        return Poll::Ready(Some(Ok(Buffer::from(data))));
                    }
                    // Ignore HTTP trailers and poll next frame
                }
                Poll::Ready(Some(Err(err))) => {
                    if let Some(ref tel) = this.telemetry {
                        if tel.is_measuring() {
                            tel.network_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    return Poll::Ready(Some(Err(Error::new(
                        ErrorKind::Unexpected,
                        "read data from http response body",
                    )
                    .with_operation("hyper::poll_frame")
                    .with_context("url", this.uri_label.clone())
                    .set_temporary()
                    .set_source(err))));
                }
            }
        }
    }
}

/// White-box `HttpTransport` powered by Hyper 1.x and [`ProbedHttpConnector`].
#[derive(Clone)]
pub struct HyperWhiteboxTransport {
    client: Client<ProbedHttpConnector, BufferRequestBody>,
    telemetry: Option<Arc<TelemetryRegistry>>,
    keep_alive: bool,
}

impl HyperWhiteboxTransport {
    pub fn try_new(
        opts: HyperTransportOptions,
        telemetry: Option<Arc<TelemetryRegistry>>,
    ) -> anyhow::Result<Self> {
        let connector =
            ProbedHttpConnector::try_new(&opts.tls, telemetry.clone(), opts.connect_timeout)?;
        let idle_cap = if opts.keep_alive {
            opts.pool_max_idle_per_host.max(8)
        } else {
            0
        };

        let mut builder = Client::builder(TokioExecutor::new());
        builder
            .http2_adaptive_window(true)
            .pool_max_idle_per_host(idle_cap)
            .pool_timer(TokioTimer::new())
            .retry_canceled_requests(opts.keep_alive);

        if opts.keep_alive {
            builder.pool_idle_timeout(Duration::from_secs(90));
        } else {
            builder.pool_idle_timeout(Duration::ZERO);
        }

        let client: Client<ProbedHttpConnector, BufferRequestBody> = builder.build(connector);
        Ok(Self {
            client,
            telemetry,
            keep_alive: opts.keep_alive,
        })
    }
}

fn classify_rpc_and_multipart(method: &Method, query: Option<&str>) -> (bool, bool, bool, bool) {
    let q = query.unwrap_or_default();
    let is_list = method == Method::GET
        && (q.contains("list-type=") || q.contains("prefix=") || q.contains("delimiter="));
    let is_class_a = matches!(
        *method,
        Method::PUT | Method::POST | Method::DELETE | Method::PATCH
    ) || is_list;

    let is_mpu_init = method == Method::POST
        && (q == "uploads" || q.starts_with("uploads&") || q.contains("&uploads"));
    let is_mpu_part = method == Method::PUT && q.contains("partNumber=") && q.contains("uploadId=");
    let is_mpu_complete =
        method == Method::POST && q.contains("uploadId=") && !q.contains("uploads");

    (is_class_a, is_mpu_init, is_mpu_part, is_mpu_complete)
}

impl HttpTransport for HyperWhiteboxTransport {
    async fn fetch(&self, req: Request<Buffer>) -> Result<Response<HttpBody>> {
        let original_uri = req.uri().clone();
        let method = req.method().clone();
        let is_head = method == Method::HEAD;

        let (mut parts, body_buf) = req.into_parts();

        let effective_uri: http::Uri = if let Some(redirect) = parts.extensions.get::<HttpRedirect>()
        {
            let target_str = redirect.uri().original_uri();
            let parsed_target: http::Uri = target_str.parse().map_err(|err| {
                Error::new(ErrorKind::Unexpected, "redirect target uri is invalid")
                    .with_operation("hyper::fetch")
                    .with_context("url", original_uri.to_string())
                    .set_source(err)
            })?;
            if original_uri.authority() != parsed_target.authority()
                || original_uri.scheme() != parsed_target.scheme()
            {
                for sensitive in [
                    "authorization",
                    "proxy-authorization",
                    "cookie",
                    "cookie2",
                    "www-authenticate",
                ] {
                    parts.headers.insert(
                        HeaderName::from_static(sensitive),
                        HeaderValue::from_static(""),
                    );
                }
            }
            parsed_target
        } else {
            original_uri.clone()
        };

        let query = effective_uri.query();
        let (is_class_a, is_mpu_init, is_mpu_part, is_mpu_complete) =
            classify_rpc_and_multipart(&method, query);
        let is_object_read = method == Method::GET && !is_class_a;

        if let Some(ref tel) = self.telemetry {
            if tel.is_measuring() {
                tel.total_http_requests.fetch_add(1, Ordering::Relaxed);
                tel.record_backend_rpc(is_class_a);
                if is_mpu_init {
                    tel.multipart_initiations.fetch_add(1, Ordering::Relaxed);
                }
                if is_mpu_part {
                    tel.multipart_parts_uploaded.fetch_add(1, Ordering::Relaxed);
                }
                if is_mpu_complete {
                    tel.multipart_completions.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let mut guard = InflightStreamGuard::enter(self.telemetry.clone());

        if !self.keep_alive {
            parts.headers.insert(
                http::header::CONNECTION,
                HeaderValue::from_static("close"),
            );
        }

        let mut hyper_req = Request::builder()
            .method(method.clone())
            .uri(effective_uri.clone())
            .version(parts.version);

        if let Some(hdrs) = hyper_req.headers_mut() {
            *hdrs = parts.headers;
        }

        let hyper_req = hyper_req
            .body(BufferRequestBody { buf: body_buf })
            .map_err(|err| {
                Error::new(ErrorKind::Unexpected, "build hyper request")
                    .with_operation("hyper::fetch")
                    .set_source(err)
            })?;

        let path_short = effective_uri.path();
        let span_label = format!("http::await_ttfb {method} {path_short}");
        let t_fetch_start = Instant::now();

        let resp = match self
            .client
            .request(hyper_req)
            .instrument_await(span_label)
            .await
        {
            Ok(r) => r,
            Err(err) => {
                if let Some(ref tel) = self.telemetry {
                    if tel.is_measuring() {
                        tel.network_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let is_temp = err.is_connect()
                    || format!("{err:?}").contains("IncompleteMessage")
                    || format!("{err:?}").contains("Canceled")
                    || format!("{err:?}").contains("TimedOut");
                return Err(Error::new(ErrorKind::Unexpected, "send http request failed")
                    .with_operation("hyper::request")
                    .with_context("url", original_uri.to_string())
                    .with_temporary(is_temp)
                    .set_source(err));
            }
        };

        let t_headers_ready = Instant::now();

        // Extract exact per-connection socket telemetry attached by ProbedHttpConnector
        if let Some(ref tel) = self.telemetry {
            if let Some(conn_meta) = resp.extensions().get::<ConnectionTelemetry>() {
                let seq = conn_meta.dispatch_seq.fetch_add(1, Ordering::AcqRel);
                tel.update_negotiated_protocol(
                    &conn_meta.alpn,
                    &conn_meta.tls_version,
                    &conn_meta.cipher_suite,
                );
                let write_done = conn_meta.last_write_instant();
                if seq == 0 {
                    tel.record_cold_handshake(conn_meta.dns_us, conn_meta.tcp_us, conn_meta.tls_us);
                    let ttfb_us = t_headers_ready
                        .saturating_duration_since(conn_meta.dial_completed_at)
                        .as_micros() as u64;
                    tel.record_gateway_ttfb(ttfb_us);
                } else {
                    // Guard against post-response TLS close_notify / H2 WINDOW_UPDATE writes
                    let valid_write = if write_done >= t_fetch_start && write_done <= t_headers_ready {
                        let gap_to_hdr = t_headers_ready.saturating_duration_since(write_done);
                        if gap_to_hdr > Duration::from_micros(50) {
                            write_done
                        } else {
                            t_fetch_start
                        }
                    } else {
                        t_fetch_start
                    };
                    let checkout_us = valid_write
                        .saturating_duration_since(t_fetch_start)
                        .as_micros() as u64;
                    tel.record_warm_reuse(checkout_us);
                    let ttfb_us = t_headers_ready
                        .saturating_duration_since(valid_write)
                        .as_micros() as u64;
                    tel.record_gateway_ttfb(ttfb_us);
                }
            } else {
                // Fallback only if custom extension absent
                let total_hdr_us = t_headers_ready
                    .saturating_duration_since(t_fetch_start)
                    .as_micros() as u64;
                tel.record_gateway_ttfb(total_hdr_us);
            }
        }

        let status = resp.status();
        if let Some(ref tel) = self.telemetry {
            if tel.is_measuring() {
                match status.as_u16() {
                    200 | 201 | 204 => {
                        tel.status_200.fetch_add(1, Ordering::Relaxed);
                    }
                    206 => {
                        tel.status_206.fetch_add(1, Ordering::Relaxed);
                    }
                    401 | 403 => {
                        tel.status_403.fetch_add(1, Ordering::Relaxed);
                    }
                    404 => {
                        tel.status_404.fetch_add(1, Ordering::Relaxed);
                    }
                    429 => {
                        tel.status_429.fetch_add(1, Ordering::Relaxed);
                        let _ = OpTraceScope::with_current(|s| {
                            s.throttled_429_503.store(true, Ordering::Relaxed);
                        });
                    }
                    500 | 502 | 504 => {
                        tel.status_500.fetch_add(1, Ordering::Relaxed);
                    }
                    503 => {
                        tel.status_503.fetch_add(1, Ordering::Relaxed);
                        let _ = OpTraceScope::with_current(|s| {
                            s.throttled_429_503.store(true, Ordering::Relaxed);
                        });
                    }
                    c if c >= 400 => {
                        tel.status_other_err.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }

                for hdr_name in [
                    "x-amz-request-id",
                    "cf-ray",
                    "x-oss-request-id",
                    "x-goog-generation",
                    "x-ms-request-id",
                ] {
                    if let Some(val) = resp.headers().get(hdr_name).and_then(|v| v.to_str().ok()) {
                        tel.push_request_id(format!(
                            "{} [{}] {hdr_name}={val}",
                            status.as_u16(),
                            method
                        ));
                        break;
                    }
                }
            }
        }

        let content_length = if is_head || parse_content_encoding(resp.headers())?.is_some() {
            None
        } else {
            parse_content_length(resp.headers())?
        };

        let (resp_parts, incoming) = resp.into_parts();
        let mut hr = Response::builder()
            .status(resp_parts.status)
            .version(resp_parts.version)
            .extension(original_uri.clone());

        if let Some(hdrs) = hr.headers_mut() {
            *hdrs = resp_parts.headers;
        }

        guard.start_body_phase();
        let body_stream = ProbedIncomingStream {
            incoming,
            uri_label: original_uri.to_string(),
            is_object_read: is_object_read && status.is_success(),
            telemetry: self.telemetry.clone(),
            _guard: guard,
        };

        let http_body = HttpBody::new(body_stream, content_length);
        Ok(hr.body(http_body).expect("valid HTTP response builder"))
    }
}

#[allow(dead_code)]
fn _assert_connection_send_sync()
where
    ProbedConnection: Send + Unpin + 'static,
{
}
