use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> Result<Operator, Box<dyn std::error::Error + Send + Sync>> {
    opendal::init_default_registry();
    let op = Operator::from_uri(format!("s3://{}/data", std::env::var("OPROBE_BUCKET")?))?
        // Innermost first: each layer wraps the ones before it.
        .layer(TimeoutLayer::new())
        .layer(RetryLayer::new());
    Ok(op)
}
