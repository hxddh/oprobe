use opendal::layers::ConcurrentLimitLayer;
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    opendal::init_default_registry();
    let op = Operator::from_uri("memory://")?
        // Innermost first: each layer wraps the ones before it.
        .layer(ConcurrentLimitLayer::new(8));
    Ok(op)
}
