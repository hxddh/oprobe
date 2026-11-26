use foyer::HybridCacheBuilder;
use opendal::layers::{FoyerKey, FoyerLayer, FoyerValue};
use opendal::Operator;

/// The operator as oprobe measured it.
pub async fn build_operator() -> Result<Operator, Box<dyn std::error::Error + Send + Sync>> {
    let op = Operator::from_uri("memory://")?
        // Innermost first: each layer wraps the ones before it.
        .layer(FoyerLayer::new(HybridCacheBuilder::new().memory(1024 * 1024).with_shards(1).with_weighter(|_: &FoyerKey, v: &FoyerValue| v.len()).with_filter(|_: &FoyerKey, v: &FoyerValue| v.len() <= 1024 * 1024).storage().build().await?).with_size_limit(..=1024 * 1024));
    Ok(op)
}
