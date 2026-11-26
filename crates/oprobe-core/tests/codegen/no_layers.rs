use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> opendal::Result<Operator> {
    let op = Operator::from_uri("memory://")?;
    Ok(op)
}
