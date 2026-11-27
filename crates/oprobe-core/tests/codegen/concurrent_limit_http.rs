use opendal::layers::ConcurrentLimitLayer;
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    opendal::init_default_registry();
    let op = Operator::from_uri("memory://")?
        // Innermost first: each layer wraps the ones before it.
        // The HTTP permits are taken inside the service: waiting for one counts against
        // the timeout and tail-cut layers.
        .layer(ConcurrentLimitLayer::new(1 << 20).with_http_concurrent_limit(24));
    Ok(op)
}
