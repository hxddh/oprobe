//! Building the measured operator from an [`OperatorSpec`]: the service (through oprobe's
//! instrumented `HttpFetch`) and the spec's layer stack, innermost first
//! ([`super::diff::LAYER_ORDER`]).

use std::time::Duration;

use anyhow::{Context, Result};
use opendal::Operator;
use opendal::layers::{
    ConcurrentLimitLayer, FoyerLayer, RetryLayer, TailCutLayer, ThrottleLayer, TimeoutLayer,
};

use super::diff::UNLIMITED_OPERATIONS;
use super::*;
use crate::config::TargetSpec;
use crate::trace::OpScope;

impl OperatorSpec {
    /// The service target of this spec (URL and options; not yet normalized).
    pub fn target_spec(&self) -> TargetSpec {
        TargetSpec {
            url: Some(self.url.clone()),
            scheme: String::new(),
            options: self.options.clone(),
            insecure: self.http.insecure,
            ca_cert: self.http.ca_cert.clone(),
            proxy: self.http.proxy.clone(),
        }
    }

    /// The service target, normalized (the URL's options merged, required options checked).
    pub fn target(&self) -> Result<TargetSpec> {
        self.target_spec().normalized()
    }
}

/// `TailCutLayer` with the spec's percentile, its deadline capped by the operation timeout.
pub fn tail_cut_layer(t: &TailCut, op_timeout: Duration) -> TailCutLayer {
    let p = crate::engine::stack::TailCutParams::for_timeout(op_timeout);
    TailCutLayer::builder()
        .percentile(t.percentile)
        .safety_factor(p.safety_factor)
        .min_samples(p.min_samples)
        .min_deadline(p.min_deadline)
        .max_deadline(p.max_deadline)
        .window(p.window)
        .build()
}

/// `RetryLayer` with the spec's parameters.
pub fn retry_layer(r: &Retry) -> RetryLayer {
    let l = RetryLayer::default()
        .with_factor(r.factor)
        .with_min_delay(r.min_delay.get())
        .with_max_delay(r.max_delay.get())
        .with_max_times(r.max_times);
    if r.jitter { l.with_jitter() } else { l }
}

/// Wrap `op` (the service, already traced) in the spec's layers. Retries are reported to the
/// operation's [`OpScope`] (backoff, throttling) when `observe` is set.
pub async fn apply_layers(
    mut op: Operator,
    spec: &OperatorSpec,
    observe: bool,
) -> Result<Operator> {
    let l = &spec.layers;
    if let Some(t) = &l.throttle {
        op = op.layer(ThrottleLayer::new(
            t.bandwidth.0.min(u64::from(u32::MAX)) as u32,
            t.burst.0.min(u64::from(u32::MAX)) as u32,
        ));
    }
    if let Some(c) = &l.concurrent_limit {
        let mut layer = ConcurrentLimitLayer::new(c.operations.unwrap_or(UNLIMITED_OPERATIONS));
        if let Some(h) = c.http {
            layer = layer.with_http_concurrent_limit(h);
        }
        op = op.layer(layer);
    }
    if let Some(t) = &l.timeout {
        op = op.layer(
            TimeoutLayer::new()
                .with_timeout(t.op.get())
                .with_io_timeout(t.io.get()),
        );
    }
    if let Some(t) = &l.tail_cut {
        op = op.layer(tail_cut_layer(t, spec.op_timeout()));
    }
    if let Some(r) = &l.retry {
        let layer = retry_layer(r);
        op = if observe {
            op.layer(layer.with_notify(|ev: opendal::layers::RetryEvent<'_>| {
                if let Some(s) = OpScope::current() {
                    // 429/503 responses mark the scope throttled when they are recorded
                    // (`OpScope::push_request`); OpenDAL's kind covers the rest.
                    s.record_retry(
                        ev.retry_after,
                        ev.err.kind() == opendal::ErrorKind::RateLimited,
                    );
                }
            }))
        } else {
            op.layer(layer)
        };
    }
    if let Some(c) = &l.cache {
        let cache = foyer::HybridCacheBuilder::new()
            .memory(c.memory.as_usize())
            .storage()
            .build()
            .await
            .context("cannot build the in-memory foyer cache")?;
        op = op.layer(FoyerLayer::new(cache));
    }
    Ok(op)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_layer_builds_and_serves() {
        let spec = OperatorSpec::parse(
            "url = \"memory://\"\n[layers]\nconcurrent_limit = { http = 4, operations = 8 }\ntail_cut = true\nthrottle = { bandwidth = \"64MiB\", burst = \"8MiB\" }\ncache = { memory = \"8MiB\" }\n",
        )
        .unwrap();
        let base = Operator::new(opendal::services::Memory::default()).unwrap();
        let op = apply_layers(base, &spec, true).await.unwrap();
        op.write("a", vec![1u8; 1024]).await.unwrap();
        assert_eq!(op.read("a").await.unwrap().len(), 1024);
        // Read twice: the second read can come from the cache.
        assert_eq!(op.read("a").await.unwrap().len(), 1024);
        let t = spec.target().unwrap();
        assert_eq!(t.scheme, "memory");
    }
}
