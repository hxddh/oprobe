use std::time::Duration;

use foyer::HybridCacheBuilder;
use opendal::layers::{ConcurrentLimitLayer, FoyerKey, FoyerLayer, FoyerValue, RetryLayer, TailCutLayer, ThrottleLayer, TimeoutLayer};
use opendal::{Operator, Reader, Writer};

/// The operator as oprobe measured it.
pub async fn build_operator() -> Result<Operator, Box<dyn std::error::Error + Send + Sync>> {
    let op = Operator::from_uri("memory://")?
        // Innermost first: each layer wraps the ones before it.
        // The HTTP permits are taken inside the service: waiting for one counts against
        // the timeout and tail-cut layers.
        .layer(TimeoutLayer::new().with_timeout(Duration::from_secs(30)).with_io_timeout(Duration::from_millis(2500)))
        .layer(TailCutLayer::builder().percentile(99).safety_factor(2.0).min_samples(10).min_deadline(Duration::from_millis(20)).max_deadline(Duration::from_secs(3)).window(Duration::from_secs(90)).build())
        .layer(ConcurrentLimitLayer::new(64).with_http_concurrent_limit(24))
        .layer(ThrottleLayer::new(64 * 1024 * 1024, 8 * 1024 * 1024))
        .layer(RetryLayer::new().with_jitter().with_max_times(5).with_min_delay(Duration::from_millis(50)).with_max_delay(Duration::from_secs(5)).with_factor(1.5))
        .layer(FoyerLayer::new(HybridCacheBuilder::new().memory(3000000).with_shards(1).with_weighter(|_: &FoyerKey, v: &FoyerValue| v.len()).with_filter(|_: &FoyerKey, v: &FoyerValue| v.len() <= 3000000).storage().build().await?).with_size_limit(..=3000000));
    Ok(op)
}

/// A reader of `path` with the measured `[read]` options.
pub async fn reader(op: &Operator, path: &str) -> opendal::Result<Reader> {
    op.reader_with(path).chunk(8 * 1024 * 1024).concurrent(4).gap(64 * 1024).await
}

/// A writer to `path` with the measured `[write]` options.
pub async fn writer(op: &Operator, path: &str) -> opendal::Result<Writer> {
    op.writer_with(path).chunk(8 * 1024 * 1024).concurrent(4).await
}
