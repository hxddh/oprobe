use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::{Operator, Writer};

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    let op = Operator::from_uri("memory://")?
        // Innermost first: each layer wraps the ones before it.
        .layer(TimeoutLayer::new())
        .layer(RetryLayer::new());
    Ok(op)
}

/// A writer to `path` with the measured `[write]` options.
pub async fn writer(op: &Operator, path: &str) -> opendal::Result<Writer> {
    op.writer_with(path).concurrent(2).await
}
