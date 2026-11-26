//! An in-process HTTP server for the transport's tests: HTTP/1.1 and HTTP/2 (prior knowledge)
//! on loopback, answering with whatever the test's handler returns.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;

/// A running server; it stops when dropped.
pub(crate) struct TestServer {
    pub(crate) addr: SocketAddr,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// A response with `status` and `body`.
pub(crate) fn respond(status: u16, body: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .expect("a valid response")
}

/// Serve every request with `handler`.
pub(crate) async fn serve<H, F>(handler: H) -> TestServer
where
    H: Fn(Request<Incoming>) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = Response<Full<Bytes>>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local address");
    let accept = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let answer = handler(req);
                    async move { Ok::<_, Infallible>(answer.await) }
                });
                // A client hanging up is how these connections end.
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    TestServer { addr, accept }
}
