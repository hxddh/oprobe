use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    opendal::init_default_registry();
    let op = Operator::via_iter(
        "fs",
        [
            ("root".to_owned(), "./local-dir".to_owned()),
        ],
    )?
    // Innermost first: each layer wraps the ones before it.
    .layer(TimeoutLayer::new())
    .layer(RetryLayer::new());
    Ok(op)
}
