use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    opendal::init_default_registry();
    let op = Operator::via_iter(
        "s3",
        [
            ("bucket".to_owned(), "lab".to_owned()),
            ("disable_config_load".to_owned(), "true".to_owned()),
            ("endpoint".to_owned(), "http://127.0.0.1:9000".to_owned()),
            ("region".to_owned(), "us-east-1".to_owned()),
        ],
    )?
    // Innermost first: each layer wraps the ones before it.
    .layer(TimeoutLayer::new())
    .layer(RetryLayer::new());
    Ok(op)
}
