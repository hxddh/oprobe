use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::{HttpTransporter, OperationContext, Operator};
use opendal_http_transport_reqwest::ReqwestTransport;

/// The operator as oprobe measured it.
pub fn build_operator() -> Result<Operator, Box<dyn std::error::Error + Send + Sync>> {
    opendal::init_default_registry();
    let op = Operator::via_iter(
        "s3",
        [
            ("bucket".to_owned(), "bucket".to_owned()),
            ("endpoint".to_owned(), "http://127.0.0.1:9000".to_owned()),
            ("root".to_owned(), "/prefix".to_owned()),
        ],
    )?
    // Innermost first: each layer wraps the ones before it.
    .layer(TimeoutLayer::new())
    .layer(RetryLayer::new());
    let client = reqwest::Client::builder().http2_prior_knowledge().build()?;
    let op = op.with_context(OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client))));
    Ok(op)
}
