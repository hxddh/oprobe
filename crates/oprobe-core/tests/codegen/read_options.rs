use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::{Operator, Reader};

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    opendal::init_default_registry();
    let op = Operator::from_uri("memory://")?
        // Innermost first: each layer wraps the ones before it.
        .layer(TimeoutLayer::new())
        .layer(RetryLayer::new());
    Ok(op)
}

/// A reader of `path` with the measured `[read]` options.
pub async fn reader(op: &Operator, path: &str) -> opendal::Result<Reader> {
    op.reader_with(path).gap(1024 * 1024).await
}
