use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::{HttpTransporter, OperationContext, Operator};
use opendal_http_transport_reqwest::ReqwestTransport;

/// The operator as oprobe measured it.
pub fn build_operator() -> Result<Operator, Box<dyn std::error::Error + Send + Sync>> {
    opendal::init_default_registry();
    let op = Operator::from_uri("s3://bucket/prefix")?
        // Innermost first: each layer wraps the ones before it.
        .layer(TimeoutLayer::new())
        .layer(RetryLayer::new());
    // reqwest's `http2` feature negotiates h2 through ALPN.
    let client = reqwest::Client::builder().build()?;
    let op = op.with_context(OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client))));
    Ok(op)
}
