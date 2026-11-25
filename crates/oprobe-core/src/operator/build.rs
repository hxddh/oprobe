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

/// A layer parameter's value, as passed to the OpenDAL builder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arg {
    Count(usize),
    F32(f32),
    F64(f64),
    /// A builder switch without an argument (`with_jitter()`).
    Switch(bool),
    Time(Duration),
    Bytes(u64),
}

/// OpenDAL's own value for a parameter when its builder method is not called.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OpendalDefault {
    /// A constructor argument: always spelled out.
    Ctor,
    /// Not applied unless the method is called.
    Unset,
    /// This value.
    Value(Arg),
}

/// One applied layer parameter: the builder method that sets it, the value oprobe applies and
/// OpenDAL's default for it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Param {
    pub method: &'static str,
    pub value: Arg,
    pub opendal_default: OpendalDefault,
}

impl Param {
    fn ctor(method: &'static str, value: Arg) -> Self {
        Self {
            method,
            value,
            opendal_default: OpendalDefault::Ctor,
        }
    }

    fn with(method: &'static str, value: Arg, default: Arg) -> Self {
        Self {
            method,
            value,
            opendal_default: OpendalDefault::Value(default),
        }
    }

    /// Whether generated code must call the method for the layer to behave as measured.
    pub fn spelled(&self) -> bool {
        match self.opendal_default {
            OpendalDefault::Ctor | OpendalDefault::Unset => true,
            OpendalDefault::Value(d) => d != self.value,
        }
    }
}

/// Every parameter oprobe applies to layer `name` of `s` (`None`: not in the stack). Both the
/// measured operator ([`apply_layers`]) and the generated code ([`super::diff`]) are made from
/// this list, so what is measured is what is emitted.
///
/// The OpenDAL defaults are those of OpenDAL 0.59.5.
pub fn layer_params(s: &OperatorSpec, name: &str) -> Option<Vec<Param>> {
    use Arg::*;
    let l = &s.layers;
    let secs = |n| Time(Duration::from_secs(n));
    Some(match name {
        "throttle" => {
            let t = l.throttle.as_ref()?;
            vec![
                Param::ctor("bandwidth", Bytes(t.bandwidth.0.min(u64::from(u32::MAX)))),
                Param::ctor("burst", Bytes(t.burst.0.min(u64::from(u32::MAX)))),
            ]
        }
        "concurrent_limit" => {
            let c = l.concurrent_limit.as_ref()?;
            let mut v = vec![Param::ctor(
                "permits",
                Count(c.operations.unwrap_or(UNLIMITED_OPERATIONS)),
            )];
            if let Some(h) = c.http {
                v.push(Param {
                    method: "with_http_concurrent_limit",
                    value: Count(h),
                    opendal_default: OpendalDefault::Unset,
                });
            }
            v
        }
        "timeout" => {
            let t = l.timeout.as_ref()?;
            vec![
                Param::with("with_timeout", Time(t.op.get()), secs(60)),
                Param::with("with_io_timeout", Time(t.io.get()), secs(10)),
            ]
        }
        "tail_cut" => {
            let p = l.tail_cut.as_ref()?.params(s.op_timeout());
            vec![
                Param::with("percentile", Count(usize::from(p.percentile)), Count(95)),
                Param::with("safety_factor", F64(p.safety_factor), F64(1.3)),
                Param::with("min_samples", Count(p.min_samples), Count(200)),
                Param::with(
                    "min_deadline",
                    Time(p.min_deadline),
                    Time(Duration::from_millis(500)),
                ),
                Param::with("max_deadline", Time(p.max_deadline), secs(30)),
                Param::with("window", Time(p.window), secs(60)),
            ]
        }
        "retry" => {
            let r = l.retry.as_ref()?;
            vec![
                Param::with("with_jitter", Switch(r.jitter), Switch(false)),
                Param::with("with_max_times", Count(r.max_times), Count(3)),
                Param::with("with_min_delay", Time(r.min_delay.get()), secs(1)),
                Param::with("with_max_delay", Time(r.max_delay.get()), secs(60)),
                Param::with("with_factor", F32(r.factor), F32(2.0)),
            ]
        }
        "cache" => {
            let c = l.cache.as_ref()?;
            vec![Param::ctor("memory", Bytes(c.memory.0))]
        }
        _ => return None,
    })
}

fn count(p: &[Param], method: &str) -> Option<usize> {
    p.iter()
        .find(|x| x.method == method)
        .and_then(|x| match x.value {
            Arg::Count(n) => Some(n),
            _ => None,
        })
}

fn bytes(p: &[Param], method: &str) -> u64 {
    p.iter()
        .find(|x| x.method == method)
        .and_then(|x| match x.value {
            Arg::Bytes(n) => Some(n),
            _ => None,
        })
        .unwrap_or_default()
}

/// `TailCutLayer` from its applied parameters.
pub fn tail_cut_layer(params: &[Param]) -> TailCutLayer {
    let mut b = TailCutLayer::builder();
    for p in params {
        b = match (p.method, p.value) {
            ("percentile", Arg::Count(n)) => b.percentile(n as u8),
            ("safety_factor", Arg::F64(f)) => b.safety_factor(f),
            ("min_samples", Arg::Count(n)) => b.min_samples(n),
            ("min_deadline", Arg::Time(d)) => b.min_deadline(d),
            ("max_deadline", Arg::Time(d)) => b.max_deadline(d),
            ("window", Arg::Time(d)) => b.window(d),
            other => unreachable!("tail_cut parameter {other:?}"),
        };
    }
    b.build()
}

/// `RetryLayer` from its applied parameters.
fn retry_layer(params: &[Param]) -> RetryLayer {
    let mut l = RetryLayer::default();
    for p in params {
        l = match (p.method, p.value) {
            ("with_jitter", Arg::Switch(true)) => l.with_jitter(),
            ("with_jitter", Arg::Switch(false)) => l,
            ("with_max_times", Arg::Count(n)) => l.with_max_times(n),
            ("with_min_delay", Arg::Time(d)) => l.with_min_delay(d),
            ("with_max_delay", Arg::Time(d)) => l.with_max_delay(d),
            ("with_factor", Arg::F32(f)) => l.with_factor(f),
            other => unreachable!("retry parameter {other:?}"),
        };
    }
    l
}

/// `TimeoutLayer` from its applied parameters.
fn timeout_layer(params: &[Param]) -> TimeoutLayer {
    let mut l = TimeoutLayer::new();
    for p in params {
        l = match (p.method, p.value) {
            ("with_timeout", Arg::Time(d)) => l.with_timeout(d),
            ("with_io_timeout", Arg::Time(d)) => l.with_io_timeout(d),
            other => unreachable!("timeout parameter {other:?}"),
        };
    }
    l
}

/// Wrap `op` (the service, already traced) in the spec's layers. Retries are reported to the
/// operation's [`OpScope`] (backoff, throttling) when `observe` is set.
pub async fn apply_layers(op: Operator, spec: &OperatorSpec, observe: bool) -> Result<Operator> {
    apply_layers_with_tail_cut(op, spec, observe, None).await
}

/// [`apply_layers`], reusing an already-warmed [`TailCutLayer`] when given.
pub async fn apply_layers_with_tail_cut(
    op: Operator,
    spec: &OperatorSpec,
    observe: bool,
    shared_tail_cut: Option<TailCutLayer>,
) -> Result<Operator> {
    apply_layers_with_recorder(op, spec, observe, shared_tail_cut, None).await
}

/// [`apply_layers_with_tail_cut`], also observing in-memory cache lookups into `recorder`
/// when `layers.cache` is enabled.
pub async fn apply_layers_with_recorder(
    mut op: Operator,
    spec: &OperatorSpec,
    observe: bool,
    shared_tail_cut: Option<TailCutLayer>,
    recorder: Option<std::sync::Arc<crate::trace::Recorder>>,
) -> Result<Operator> {
    let params = |name| layer_params(spec, name);
    if let Some(p) = params("throttle") {
        op = op.layer(ThrottleLayer::new(
            bytes(&p, "bandwidth") as u32,
            bytes(&p, "burst") as u32,
        ));
    }
    if let Some(p) = params("concurrent_limit") {
        let mut layer = ConcurrentLimitLayer::new(count(&p, "permits").unwrap_or(1));
        let has_http = count(&p, "with_http_concurrent_limit");
        if let Some(h) = has_http {
            layer = layer.with_http_concurrent_limit(h);
        }
        op = op.layer(layer);
        if observe && has_http.is_some() {
            op = op.layer(oprobe_trace::QueueStampLayer);
        }
    }
    if let Some(p) = params("timeout") {
        op = op.layer(timeout_layer(&p));
    }
    if let Some(p) = params("tail_cut") {
        op = op.layer(shared_tail_cut.unwrap_or_else(|| tail_cut_layer(&p)));
    }
    if let Some(p) = params("retry") {
        let layer = retry_layer(&p);
        op = if observe {
            op.layer(layer.with_notify(|ev: opendal::layers::RetryEvent<'_>| {
                if let Some(s) = OpScope::current() {
                    let msg = ev.err.message();
                    let cut = matches!(msg, "cancelled by tail cut" | "io cancelled by tail cut")
                        || msg.contains("tail cut");
                    // 429/503 responses mark the scope throttled when they are recorded
                    // (`OpScope::push_request`); OpenDAL's kind covers the rest.
                    s.record_retry_with_cut(
                        ev.retry_after,
                        ev.err.kind() == opendal::ErrorKind::RateLimited,
                        cut,
                    );
                }
            }))
        } else {
            op.layer(layer)
        };
    }
    if let Some(p) = params("cache") {
        let cache = foyer::HybridCacheBuilder::new()
            .memory(bytes(&p, "memory") as usize)
            .storage()
            .build()
            .await
            .context("cannot build the in-memory foyer cache")?;
        op = op.layer(FoyerLayer::new(cache));
        if observe && let Some(rec) = recorder {
            op = op.layer(crate::trace::CacheObserveLayer::new(rec));
        }
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
