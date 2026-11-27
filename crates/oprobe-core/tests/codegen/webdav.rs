use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    opendal::init_default_registry();
    let op = Operator::via_iter(
        "webdav",
        [
            ("endpoint".to_owned(), "http://127.0.0.1:8080".to_owned()),
            ("root".to_owned(), "/files".to_owned()),
        ],
    )?
    // Innermost first: each layer wraps the ones before it.
    .layer(TimeoutLayer::new())
    .layer(RetryLayer::new());
    Ok(op)
}
