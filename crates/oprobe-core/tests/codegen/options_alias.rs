use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    opendal::init_default_registry();
    let op = Operator::via_iter(
        "gcs",
        [
            ("bucket".to_owned(), "my-bucket".to_owned()),
            ("root".to_owned(), "/data".to_owned()),
            ("service_account".to_owned(), "sa@proj.iam".to_owned()),
        ],
    )?
    // Innermost first: each layer wraps the ones before it.
    .layer(TimeoutLayer::new())
    .layer(RetryLayer::new());
    Ok(op)
}
