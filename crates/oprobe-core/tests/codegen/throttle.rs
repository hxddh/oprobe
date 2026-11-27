use opendal::layers::ThrottleLayer;
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    opendal::init_default_registry();
    let op = Operator::from_uri("memory://")?
        // Innermost first: each layer wraps the ones before it.
        .layer(ThrottleLayer::new(100 * 1024 * 1024, 16 * 1024 * 1024));
    Ok(op)
}
