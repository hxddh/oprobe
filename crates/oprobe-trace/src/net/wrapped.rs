//! The host's own HTTP transport, observed from outside (`Config::keep_http_transport`).
//!
//! [`WrappedTransport`] forwards every request to the transport that was installed on the
//! operator before the trace layer was applied: the application's own client with its proxy,
//! TLS roots, timeouts and pool, and whatever wraps it beneath the trace layer (an HTTP
//! concurrency limit). Nothing of it is replaced or reconfigured.
//!
//! What can be seen from outside is recorded: queue time, the time to the response headers,
//! the body transfer and its consumer's stalls, status, bytes, the server's signals and
//! failures. What happens inside the client cannot: DNS, TCP, proxy and TLS time are part of
//! the time to first byte, and connections (reuse, dials, kernel TCP statistics) are not
//! observed.

use std::sync::Arc;
use std::time::Instant;

use await_tree::InstrumentAwait;
use http::{Method, Request, Response};
use opendal_core::raw::{parse_content_encoding, parse_content_length};
use opendal_core::{Buffer, HttpBody, HttpTransport, HttpTransporter, Result};
use tokio::sync::Semaphore;

use super::fidelity::{Entered, Finisher, TimedResponseBody, answered};
use crate::{InflightGuard, Recorder};

/// The transport installed on the operator, with every request through it recorded.
#[derive(Clone)]
pub struct WrappedTransport {
    inner: HttpTransporter,
    recorder: Arc<Recorder>,
    limiter: Option<Arc<Semaphore>>,
}

impl WrappedTransport {
    /// Observe `inner`; `limiter` caps the requests in flight through it (`Config::max_inflight`).
    pub fn new(
        inner: HttpTransporter,
        recorder: Arc<Recorder>,
        limiter: Option<Arc<Semaphore>>,
    ) -> Self {
        Self {
            inner,
            recorder,
            limiter,
        }
    }
}

impl HttpTransport for WrappedTransport {
    async fn fetch(&self, req: Request<Buffer>) -> Result<Response<HttpBody>> {
        let now = Instant::now();
        let recorder = Some(self.recorder.clone());
        let is_head = req.method() == Method::HEAD;
        let bytes_up = req.body().len() as u64;
        let (parts, body) = req.into_parts();
        let uri = &parts.uri;
        let server = uri.host().map(|host| {
            let default_port = if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            };
            (Arc::from(host), uri.port_u16().unwrap_or(default_port))
        });
        let Entered {
            t_start,
            scope,
            mut record,
            mut cancel,
            range_bytes,
        } = Entered::new(
            now,
            &parts,
            uri.path(),
            uri.query(),
            server,
            bytes_up,
            &recorder,
        );
        let span = record.service_op.unwrap_or_else(|| record.api.label());

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
        let inflight = InflightGuard::start(recorder.clone());
        let t_ready = Instant::now();
        cancel.set_ready(t_ready);
        record.timing.queue_us = t_ready.saturating_duration_since(t_start).as_micros() as u64;

        let sent = self
            .inner
            .fetch(Request::from_parts(parts, body))
            .instrument_await(span)
            .await;
        let t_headers = Instant::now();
        cancel.disarm();
        // Sending is not separable from waiting for the answer from outside the client.
        record.timing.ttfb_us = t_headers.saturating_duration_since(t_ready).as_micros() as u64;
        let resp = match sent {
            Ok(resp) => resp,
            Err(err) => {
                record.error = Some(format!("send http request: {err}"));
                record.reused = false;
                record.timing.total_us =
                    t_headers.saturating_duration_since(t_start).as_micros() as u64;
                Finisher::emit(record, &scope, &recorder);
                return Err(err);
            }
        };
        let sniff = answered(
            &mut record,
            range_bytes,
            resp.status(),
            resp.version(),
            resp.headers(),
        );
        // The declared length tells a body read to its end from one dropped part-way; a
        // header that does not parse is the service's to report.
        let expected = if is_head || !matches!(parse_content_encoding(resp.headers()), Ok(None)) {
            None
        } else {
            parse_content_length(resp.headers()).ok().flatten()
        };
        let fin = Finisher {
            record,
            // The connection is the host client's own.
            conn: None,
            t_start,
            t_headers,
            scope,
            recorder,
            _inflight: inflight,
            _permit: permit,
            sniff,
        };
        let (parts, body) = resp.into_parts();
        let body =
            body.map_inner(|inner| Box::new(TimedResponseBody::new(inner, fin, expected)));
        Ok(Response::from_parts(parts, body))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::stream;
    use opendal_core::{Error, ErrorKind};

    use super::*;

    /// A host transport that answers without a network: 200 with a five-byte body, or the
    /// error a client reports for a refused connection, quoting the URL as HTTP clients do.
    struct Host {
        calls: Arc<AtomicUsize>,
    }

    impl HttpTransport for Host {
        async fn fetch(&self, req: Request<Buffer>) -> Result<Response<HttpBody>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if req.uri().path().ends_with("/refused") {
                return Err(Error::new(ErrorKind::Unexpected, "send http request")
                    .with_context("url", req.uri().to_string())
                    .set_temporary());
            }
            let body = stream::iter([Ok(Buffer::from("he")), Ok(Buffer::from("llo"))]);
            Ok(Response::builder()
                .status(200)
                .header("content-length", "5")
                .header("x-amz-request-id", "REQ1")
                .body(HttpBody::new(body, Some(5)))
                .unwrap())
        }
    }

    /// Requests go through the host's transport, and what is visible from outside it is
    /// recorded: status, bytes, the server's request id, failures (never with a signature).
    #[tokio::test]
    async fn the_host_transport_carries_every_request_and_is_observed() {
        let recorder = Recorder::always_on(64);
        let calls = Arc::new(AtomicUsize::new(0));
        let host = HttpTransporter::new(Host {
            calls: calls.clone(),
        });
        let t = WrappedTransport::new(host, recorder.clone(), None);
        let get = |path: &str| {
            Request::get(format!("https://bucket.example{path}"))
                .body(Buffer::new())
                .unwrap()
        };
        let resp = t.fetch(get("/k")).await.unwrap();
        let (_, mut body) = resp.into_parts();
        assert_eq!(body.to_buffer().await.unwrap().to_vec(), b"hello");
        drop(body);
        assert!(
            t.fetch(get("/refused?X-Amz-Signature=SECRET"))
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        let rows = recorder.take_trace().reqs;
        assert_eq!(rows.len(), 2, "{rows:?}");
        let ok = rows.iter().find(|r| r.status == 200).expect("the answer");
        assert_eq!((ok.bytes_down, ok.error.as_deref()), (5, None));
        assert_eq!(ok.request_id.as_deref(), Some("REQ1"));
        // Connections are the host client's own: none is claimed dialed or observed.
        assert_eq!((ok.conn_id, ok.connect_us(), ok.tcp.is_none()), (0, 0, true));
        let failed = rows.iter().find(|r| r.status == 0).expect("the failure");
        let error = failed.error.as_deref().unwrap();
        assert!(error.starts_with("send http request"), "{error}");
        assert!(error.contains("bucket.example/refused"), "{error}");
        assert!(!error.contains("SECRET"), "{error}");
    }
}
