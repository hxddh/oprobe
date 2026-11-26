use std::time::Duration;

use opendal::layers::RetryLayer;
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    let op = Operator::from_uri("memory://")?
        // Innermost first: each layer wraps the ones before it.
        .layer(RetryLayer::new().with_jitter().with_max_times(5).with_min_delay(Duration::from_millis(50)).with_max_delay(Duration::from_secs(5)).with_factor(1.5));
    Ok(op)
}
