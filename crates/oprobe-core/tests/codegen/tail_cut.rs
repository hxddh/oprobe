use std::time::Duration;

use opendal::layers::TailCutLayer;
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    let op = Operator::from_uri("memory://")?
        // Innermost first: each layer wraps the ones before it.
        .layer(TailCutLayer::builder().safety_factor(1.5).min_samples(32).min_deadline(Duration::from_millis(50)).max_deadline(Duration::from_secs(60)).build());
    Ok(op)
}
