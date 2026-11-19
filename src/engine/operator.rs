//! Building OpenDAL operators: the instrumented one under test and a plain admin one
//! (dataset seeding, verification, cleanup) that never pollutes measurements.

use std::sync::{Arc, Once};
use std::time::Duration;

use anyhow::{Context, Result};
use opendal::layers::{AwaitTreeLayer, RetryLayer, TailCutLayer, TimeoutLayer};
use opendal::Operator;

use crate::config::{Knobs, TargetSpec};
use crate::layers::{InProcessObserveLayer, TraceContextLayer, WhiteboxCacheLayer};
use crate::net::{HttpVersion, TlsClientOptions, TracingTransport, TransportConfig};
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

pub fn transport(
    target: &TargetSpec,
    knobs: &Knobs,
    recorder: Option<Arc<Recorder>>,
) -> Result<TracingTransport> {
    TracingTransport::new(
        TransportConfig {
            http: knobs.http_version,
            keep_alive: knobs.keep_alive,
            max_inflight: (knobs.max_inflight_requests > 0).then_some(knobs.max_inflight_requests),
            tls: tls_options(target)?,
            proxy: proxy_setting(target),
            ..Default::default()
        },
        recorder,
    )
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
    op = op.layer(TraceContextLayer::new(transport(
        target,
        knobs,
        Some(recorder.clone()),
    )?));
    op = op.layer(
        TimeoutLayer::new()
            .with_timeout(knobs.timeout.get())
            .with_io_timeout(knobs.timeout.get().min(Duration::from_secs(30))),
    );
    if knobs.tail_cut {
        op = op.layer(
            TailCutLayer::builder()
                .percentile(95)
                .safety_factor(1.5)
                .min_samples(32)
                .min_deadline(Duration::from_millis(50))
                .max_deadline(knobs.timeout.get())
                .window(Duration::from_secs(60))
                .build(),
        );
    }
    if knobs.retries > 0 {
        op = op.layer(
            RetryLayer::default()
                .with_jitter()
                .with_min_delay(Duration::from_millis(50))
                .with_max_delay(Duration::from_secs(5))
                .with_max_times(knobs.retries)
                .with_notify(|ev: opendal::layers::RetryEvent<'_>| {
                    if let Some(s) = OpScope::current() {
                        let throttled = ev.err.kind() == opendal::ErrorKind::RateLimited || {
                            let m = ev.err.to_string();
                            m.contains("429") || m.contains("503") || m.contains("SlowDown")
                        };
                        s.with(|d| {
                            d.retries += 1;
                            d.retry_backoff_us += ev.retry_after.as_micros() as u64;
                            if throttled {
                                d.throttled = true;
                            }
                        });
                    }
                }),
        );
    }
    if tree_registry.is_some() {
        op = op.layer(AwaitTreeLayer::new());
    }
    if knobs.cache {
        op = op.layer(WhiteboxCacheLayer::new(knobs.cache_size.as_u64()));
    }
    recorder.set_cache_enabled(knobs.cache);
    Ok(op)
}

/// Untraced operator for dataset seeding, verification and cleanup.
pub fn admin_operator(base: &Operator, target: &TargetSpec) -> Result<Operator> {
    let knobs = Knobs {
        http_version: HttpVersion::Auto,
        ..Knobs::default()
    };
    Ok(base
        .clone()
        .layer(TraceContextLayer::new(transport(target, &knobs, None)?))
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
