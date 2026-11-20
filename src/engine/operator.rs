//! Building OpenDAL operators: the instrumented one under test and a plain admin one
//! (dataset seeding, verification, cleanup) that never pollutes measurements.

use std::sync::{Arc, Once};
use std::time::Duration;

use anyhow::{Context, Result};
use opendal::Operator;
use opendal::layers::{RetryLayer, TimeoutLayer};

use super::stack::ClientStack;
use crate::config::{Knobs, TargetSpec, TransportMode};
use crate::layers::{InProcessObserveLayer, TraceContextLayer, WhiteboxCacheLayer};
use crate::net::{FidelityTransport, TlsClientOptions, TracingTransport, TransportConfig};
use crate::trace::{OpScope, Recorder};

static REGISTRY: Once = Once::new();

fn ensure_registry() {
    REGISTRY.call_once(opendal::init_default_registry);
}

/// The raw backend shared by every operator of a session (essential for `memory://`).
pub fn base_operator(target: &TargetSpec) -> Result<Operator> {
    ensure_registry();
    let mut opts = target.resolved_options()?;
    opts.remove("ca_cert_pem");
    Operator::via_iter(&target.scheme, opts).with_context(|| {
        format!(
            "cannot configure the `{}` backend for {}",
            target.scheme,
            target.display()
        )
    })
}

pub fn tls_options(target: &TargetSpec) -> Result<TlsClientOptions> {
    let extra_ca_pem = match &target.ca_cert {
        Some(p) => Some(
            std::fs::read_to_string(p)
                .with_context(|| format!("cannot read CA certificate `{}`", p.display()))?,
        ),
        None => target.options.get("ca_cert_pem").cloned(),
    };
    Ok(TlsClientOptions {
        alpn_protocols: Vec::new(),
        extra_ca_pem,
        insecure_skip_verify: target.insecure,
    })
}

pub fn proxy_setting(target: &TargetSpec) -> Option<Option<String>> {
    match target.proxy.as_deref() {
        None => None,
        Some(p) if p.eq_ignore_ascii_case("none") || p.is_empty() => Some(None),
        Some(p) => Some(Some(p.to_owned())),
    }
}

pub fn transport_config(target: &TargetSpec, knobs: &Knobs) -> Result<TransportConfig> {
    Ok(TransportConfig {
        http: knobs.http_version,
        keep_alive: knobs.keep_alive,
        max_inflight: ClientStack::from_knobs(knobs).http_concurrent_limit,
        tls: tls_options(target)?,
        proxy: proxy_setting(target),
        ..Default::default()
    })
}

/// oprobe's instrumented connection pool (`--microscope`).
pub fn transport(
    target: &TargetSpec,
    knobs: &Knobs,
    recorder: Option<Arc<Recorder>>,
) -> Result<TracingTransport> {
    TracingTransport::new(transport_config(target, knobs)?, recorder)
}

/// The tracing layer for the configured transport mode.
pub fn trace_layer(
    target: &TargetSpec,
    knobs: &Knobs,
    recorder: Option<Arc<Recorder>>,
) -> Result<TraceContextLayer> {
    Ok(match knobs.transport {
        TransportMode::Microscope => TraceContextLayer::new(transport(target, knobs, recorder)?),
        TransportMode::Opendal => {
            let https = target.endpoint().is_none_or(|e| e.starts_with("https"));
            TraceContextLayer::new(FidelityTransport::new(
                transport_config(target, knobs)?,
                https,
                recorder,
            )?)
        }
    })
}

/// Instrumented operator: every request is traced into `recorder`.
pub fn measured_operator(
    base: &Operator,
    target: &TargetSpec,
    knobs: &Knobs,
    recorder: &Arc<Recorder>,
    tree_registry: Option<await_tree::Registry>,
) -> Result<Operator> {
    let mut op = base.clone();
    if !target.is_network() {
        op = op.layer(InProcessObserveLayer::new(recorder.clone()));
    }
    op = op.layer(trace_layer(target, knobs, Some(recorder.clone()))?);
    let stack = ClientStack::from_knobs(knobs);
    op = op.layer(stack.timeout_layer());
    if let Some(l) = stack.tail_cut_layer() {
        op = op.layer(l);
    }
    if let Some(l) = stack.retry_layer() {
        op = op.layer(l.with_notify(|ev: opendal::layers::RetryEvent<'_>| {
            if let Some(s) = OpScope::current() {
                // 429/503 responses mark the scope throttled when they are recorded
                // (`OpScope::push_request`); OpenDAL's kind covers the rest. The error text is
                // never matched: object names and ports contain digits.
                s.record_retry(
                    ev.retry_after,
                    ev.err.kind() == opendal::ErrorKind::RateLimited,
                );
            }
        }));
    }
    // Async call stacks: operations are registered as await-tree roots by the runner, OpenDAL
    // sub-tasks by `ScopePropagatingExecutor`, and the transports add zero-allocation spans per
    // request. OpenDAL's `AwaitTreeLayer` is not used: it wraps every call and read chunk and
    // cost ~14% throughput on small requests (examples/overhead.rs).
    let _ = tree_registry;
    if knobs.cache {
        op = op.layer(WhiteboxCacheLayer::new(knobs.cache_size.as_u64()));
    }
    recorder.set_cache_enabled(knobs.cache);
    Ok(op)
}

/// Fail fast when the endpoint cannot be reached at all (connection refused, unknown host):
/// the admin operator would otherwise retry every dataset upload for minutes. One `stat` on a
/// key that does not exist, without retries; any HTTP answer (404, 403, ...) passes.
pub async fn check_reachable(base: &Operator, target: &TargetSpec) -> Result<()> {
    let probe = base
        .clone()
        .layer(trace_layer(target, &Knobs::default(), None)?)
        .layer(TimeoutLayer::new().with_timeout(Duration::from_secs(20)));
    let Err(e) = probe.stat(".oprobe-reachability-check").await else {
        return Ok(());
    };
    let mut chain = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        chain.push_str(": ");
        chain.push_str(&s.to_string());
        src = s.source();
    }
    let l = chain.to_ascii_lowercase();
    // Wording differs by OS: "Connection refused (os error 111)" on Linux, "(os error 61)" on
    // macOS, "No connection could be made because the target machine actively refused it.
    // (os error 10061)" on Windows.
    let unreachable = [
        "connection refused",
        "actively refused",
        "os error 10061",
        "connect: ",
        "dns error",
        "failed to lookup address",
        "name or service not known",
        "no addresses to connect to",
        "nodename nor servname",
    ]
    .iter()
    .any(|n| l.contains(n));
    if unreachable {
        anyhow::bail!(
            "cannot reach {} ({}): {chain}",
            target.endpoint().unwrap_or_else(|| target.display()),
            target.display()
        );
    }
    Ok(())
}

/// Untraced operator for dataset seeding, verification and cleanup.
pub fn admin_operator(base: &Operator, target: &TargetSpec) -> Result<Operator> {
    let knobs = Knobs::default();
    Ok(base
        .clone()
        .layer(trace_layer(target, &knobs, None)?)
        .layer(
            TimeoutLayer::new()
                .with_timeout(Duration::from_secs(300))
                .with_io_timeout(Duration::from_secs(60)),
        )
        .layer(
            RetryLayer::default()
                .with_jitter()
                .with_min_delay(Duration::from_millis(100))
                .with_max_times(10)
                .with_max_delay(Duration::from_secs(10)),
        ))
}
