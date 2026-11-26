use std::time::Duration;

use opendal::layers::TimeoutLayer;
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    let op = Operator::from_uri("memory://")?
        // Innermost first: each layer wraps the ones before it.
        .layer(TimeoutLayer::new().with_timeout(Duration::from_secs(30)).with_io_timeout(Duration::from_millis(2500)));
    Ok(op)
}
