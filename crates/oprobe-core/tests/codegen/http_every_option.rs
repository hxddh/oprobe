use opendal::layers::{ConcurrentLimitLayer, RetryLayer, TimeoutLayer};
use opendal::{HttpTransporter, OperationContext, Operator, Reader};
use opendal_http_transport_reqwest::ReqwestTransport;

/// The operator as oprobe measured it.
pub fn build_operator() -> Result<Operator, Box<dyn std::error::Error + Send + Sync>> {
    opendal::init_default_registry();
    let op = Operator::from_uri("s3://bucket/prefix")?
        // Innermost first: each layer wraps the ones before it.
        // The HTTP permits are taken inside the service: waiting for one counts against
        // the timeout and tail-cut layers.
        .layer(TimeoutLayer::new())
        .layer(ConcurrentLimitLayer::new(1 << 20).with_http_concurrent_limit(16))
        .layer(RetryLayer::new());
    let client = reqwest::Client::builder().http1_only().pool_max_idle_per_host(0).danger_accept_invalid_certs(true).add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read("/etc/ssl/certs/extra.pem")?)?).proxy(reqwest::Proxy::all("http://127.0.0.1:8888")?).build()?;
    let op = op.with_context(OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client))));
    Ok(op)
}

/// A reader of `path` with the measured `[read]` options.
pub async fn reader(op: &Operator, path: &str) -> opendal::Result<Reader> {
    op.reader_with(path).concurrent(4).await
}
