//! Building OpenDAL operators: the instrumented one under test and a plain admin one
//! (dataset seeding, verification, cleanup) that never pollutes measurements.

use std::sync::{Arc, Once};
use std::time::Duration;

use anyhow::{Context, Result};
use opendal::Operator;
use opendal::layers::{RetryLayer, TimeoutLayer};

use super::stack::ClientStack;
use crate::config::{Knobs, TargetSpec};
use crate::errors::{ErrorCode, OprobeError};
use crate::net::{FidelityTransport, TlsClientOptions, TransportConfig};
use crate::trace::layer::{InProcessObserveLayer, TraceContextLayer};
use crate::trace::{OpScope, Recorder};

static REGISTRY: Once = Once::new();

fn ensure_registry() {
    REGISTRY.call_once(opendal::init_default_registry);
}

/// The raw backend shared by every operator of a session (essential for `memory://`).
/// The cargo feature that compiles the OpenDAL service behind `scheme`, when this build
/// lacks it (`None`: compiled in, or not an optional service).
pub fn missing_service_feature(scheme: &str) -> Option<&'static str> {
    let (on, feature) = match scheme {
        "s3" => (cfg!(feature = "services-s3"), "services-s3"),
        "gcs" => (cfg!(feature = "services-gcs"), "services-gcs"),
        "azblob" => (cfg!(feature = "services-azblob"), "services-azblob"),
        "oss" => (cfg!(feature = "services-oss"), "services-oss"),
        "cos" => (cfg!(feature = "services-cos"), "services-cos"),
        "obs" => (cfg!(feature = "services-obs"), "services-obs"),
        "b2" => (cfg!(feature = "services-b2"), "services-b2"),
        "webdav" => (cfg!(feature = "services-webdav"), "services-webdav"),
        "azdls" => (cfg!(feature = "services-azdls"), "services-azdls"),
        "azfile" => (cfg!(feature = "services-azfile"), "services-azfile"),
        "swift" => (cfg!(feature = "services-swift"), "services-swift"),
        "tos" => (cfg!(feature = "services-tos"), "services-tos"),
        "webhdfs" => (cfg!(feature = "services-webhdfs"), "services-webhdfs"),
        "upyun" => (cfg!(feature = "services-upyun"), "services-upyun"),
        "http" => (cfg!(feature = "services-http"), "services-http"),
        _ => (true, ""),
    };
    (!on).then_some(feature)
}

pub fn base_operator(target: &TargetSpec) -> Result<Operator> {
    ensure_registry();
    if let Some(feature) = missing_service_feature(&target.scheme) {
        return Err(OprobeError::new(
            ErrorCode::StorageUnsupported,
            format!(
                "this oprobe was built without the `{}` service",
                target.scheme
            ),
        )
        .with_hint(format!(
            "install a build with it: `cargo install oprobe --features {feature}` (or `all-services`)"
        ))
        .into());
    }
    let mut opts = target.resolved_options()?;
    opts.remove("ca_cert_pem");
    Operator::via_iter(&target.scheme, opts.clone()).map_err(|e| {
        let code = crate::errors::opendal_code(&e).unwrap_or(ErrorCode::ConfigInvalid);
        let mut err = OprobeError::new(
            code,
            format!(
                "cannot configure the `{}` backend for {}: {e}",
                target.scheme,
                target.display()
            ),
        );
        if e.kind() == opendal::ErrorKind::ConfigInvalid {
            err = err.with_hint(backend_config_hint(&target.scheme, &opts));
        }
        anyhow::Error::new(err)
    })
}

/// What to set when OpenDAL refuses a backend configuration: the required options that are
/// really missing (the bucket or container usually comes from the URL), then where options go.
pub fn backend_config_hint(
    scheme: &str,
    opts: &std::collections::BTreeMap<String, String>,
) -> String {
    let has = |k: &str| opts.get(k).is_some_and(|v| !v.trim().is_empty());
    let mut missing: Vec<String> = crate::config::required_options(scheme)
        .iter()
        .filter(|k| !has(k))
        .map(|k| format!("`{k}`"))
        .collect();
    if scheme == "azblob"
        && !has("endpoint")
        && !has("account_name")
        && !missing.iter().any(|m| m == "`endpoint`")
    {
        missing.push("`endpoint` (https://ACCOUNT.blob.core.windows.net) or `account_name`".into());
    }
    let what = if missing.is_empty() {
        "check the backend options".to_owned()
    } else {
        format!("set {}", missing.join(" and "))
    };
    format!(
        "{what}: pass `--option KEY=VALUE` (or `?KEY=VALUE` in the URL), or add it under \
         [targets.NAME.options] in the config file (`oprobe config edit`)"
    )
}

/// Where the region of an S3 target comes from.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegionSource {
    /// `region` option (URL, flag, target).
    Configured,
    /// `AWS_REGION` / `AWS_DEFAULT_REGION`.
    Environment,
    /// Asked the service (`x-amz-bucket-region` of a HeadBucket, or the endpoint's name).
    Detected,
    /// Nothing known: OpenDAL's default (`us-east-1`).
    Default,
}

/// The region of an S3 target and where it comes from; detects it (HeadBucket, ≤ 3 s) when
/// none is configured. `None` for other services.
pub async fn s3_region(target: &TargetSpec) -> Option<(String, RegionSource)> {
    if target.scheme != "s3" {
        return None;
    }
    if let Some(r) = target.options.get("region") {
        return Some((r.clone(), RegionSource::Configured));
    }
    for k in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Ok(r) = std::env::var(k)
            && !r.is_empty()
        {
            return Some((r, RegionSource::Environment));
        }
    }
    match detect_s3_region(target).await {
        Some(r) => Some((r, RegionSource::Detected)),
        None => Some(("us-east-1".into(), RegionSource::Default)),
    }
}

/// The region the service reports for an S3 bucket (`None`: not determinable).
pub async fn detect_s3_region(target: &TargetSpec) -> Option<String> {
    let bucket = target.options.get("bucket")?.clone();
    let endpoint = target
        .options
        .get("endpoint")
        .and_then(|e| crate::config::expand_env(e).ok());
    let budget = Duration::from_secs(3);
    match endpoint.filter(|e| !e.contains("amazonaws.com")) {
        // Other S3-compatible services: OpenDAL's detection (endpoint name, then HeadBucket).
        #[cfg(feature = "services-s3")]
        Some(ep) => {
            tokio::time::timeout(budget, opendal::services::S3::detect_region(&ep, &bucket))
                .await
                .ok()
                .flatten()
        }
        #[cfg(not(feature = "services-s3"))]
        Some(_) => None,
        // AWS answers HeadBucket on the global endpoint with the bucket's region in a header,
        // whatever the status (301 / 403 included): a PermanentRedirect avoided up front.
        None => {
            let mut b = reqwest::Client::builder().timeout(budget);
            if proxy_setting(target) == Some(None) {
                b = b.no_proxy();
            }
            let resp = b
                .build()
                .ok()?
                .head(format!("https://{bucket}.s3.amazonaws.com/"))
                .send()
                .await
                .ok()?;
            resp.headers()
                .get("x-amz-bucket-region")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        }
    }
}

/// Prepare a target before its operator is built: apply the provider knobs (addressing style,
/// checksum, SSE) and detect the region of an S3 bucket that has none configured.
pub async fn prepare_target(target: &mut TargetSpec, knobs: &Knobs) -> Result<()> {
    for (k, v) in knobs.provider.options(&target.scheme)? {
        target.options.insert(k.to_owned(), v);
    }
    if let Some((region, RegionSource::Detected)) = s3_region(target).await {
        target.options.insert("region".into(), region);
    }
    Ok(())
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
        // `OPROBE_TCP_INFO=0` turns the per-request kernel statistics off.
        tcp_info: std::env::var("OPROBE_TCP_INFO").map_or(true, |v| v != "0"),
    })
}

/// The tracing layer: OpenDAL's own HTTP client, instrumented (the fidelity transport).
pub fn trace_layer(
    target: &TargetSpec,
    knobs: &Knobs,
    recorder: Option<Arc<Recorder>>,
) -> Result<TraceContextLayer> {
    let https = target.endpoint().is_none_or(|e| e.starts_with("https"));
    Ok(TraceContextLayer::new(FidelityTransport::new(
        transport_config(target, knobs)?,
        https,
        recorder,
    )?))
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
    recorder.set_cache_enabled(false);
    Ok(op)
}

/// Fail fast when the endpoint cannot be reached at all (connection refused, unknown host):
/// the admin operator would otherwise retry every dataset upload for minutes. One `stat` on a
/// key that does not exist, without retries; any HTTP answer (404, 403, ...) passes.
pub async fn check_reachable(base: &Operator, target: &TargetSpec) -> Result<()> {
    check_reachable_with(base, target, &Knobs::default()).await
}

/// [`check_reachable`] with the run's transport and HTTP version (a server that only speaks
/// HTTP/2 is reachable for an HTTP/2 client).
pub async fn check_reachable_with(
    base: &Operator,
    target: &TargetSpec,
    knobs: &Knobs,
) -> Result<()> {
    let k = Knobs {
        http_version: knobs.http_version,
        ..Knobs::default()
    };
    let probe = base
        .clone()
        .layer(trace_layer(target, &k, None)?)
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
    let unreachable = super::ops::is_connect_failure(&e)
        || [
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
        return Err(OprobeError::new(
            ErrorCode::NetUnreachable,
            format!(
                "cannot reach {} ({}): {chain}",
                target.endpoint().unwrap_or_else(|| target.display()),
                target.display()
            ),
        )
        .with_hint("check the endpoint, the network path and the proxy ")
        .into());
    }
    Ok(())
}

/// With `--http h2`, check the target speaks HTTP/2 before anything else (design v5 §3, M6):
/// an endpoint without h2c (plain HTTP) or without `h2` in ALPN (TLS) would otherwise fail
/// every request of the run.
pub async fn check_protocol(base: &Operator, target: &TargetSpec, knobs: &Knobs) -> Result<()> {
    if knobs.http_version != crate::net::HttpVersion::H2 || !target.is_network() {
        return Ok(());
    }
    let probe = base
        .clone()
        .layer(trace_layer(target, knobs, None)?)
        .layer(TimeoutLayer::new().with_timeout(Duration::from_secs(10)));
    let Err(e) = probe.stat(".oprobe-protocol-check").await else {
        return Ok(());
    };
    if e.kind() == opendal::ErrorKind::NotFound || e.kind() == opendal::ErrorKind::PermissionDenied
    {
        // The server answered over HTTP/2.
        return Ok(());
    }
    let mut chain = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        chain.push_str(": ");
        chain.push_str(&s.to_string());
        src = s.source();
    }
    let l = chain.to_ascii_lowercase();
    let protocol = [
        "http2",
        "h2",
        "protocol",
        "preface",
        "frame",
        "alpn",
        "connection closed",
        "connection reset",
        "broken pipe",
        "unexpected eof",
        "error sending request",
    ]
    .iter()
    .any(|n| l.contains(n));
    if !protocol {
        return Ok(());
    }
    let endpoint = target.endpoint().unwrap_or_else(|| target.display());
    let plain = endpoint.starts_with("http://");
    Err(OprobeError::new(
        ErrorCode::NetHttp,
        format!(
            "{endpoint} does not speak HTTP/2{} (`--http h2`): {chain}",
            if plain { " with prior knowledge (h2c)" } else { " (no `h2` in ALPN)" }
        ),
    )
    .with_hint(
        "use --http auto or --http h1; lab targets speak HTTP/2 as lab://NAME?proto=h2c (plain), h2 or https (TLS)",
    )
    .into())
}

/// Untraced operator for dataset seeding, verification and cleanup.
/// Untraced operator that fails fast (few retries, short timeouts): the preflight check.
pub fn preflight_operator(base: &Operator, target: &TargetSpec) -> Result<Operator> {
    preflight_operator_with(base, target, None)
}

/// [`preflight_operator`] whose requests are recorded into `recorder` (budgets: a recorder
/// with a guard accounts every request).
pub fn preflight_operator_with(
    base: &Operator,
    target: &TargetSpec,
    recorder: Option<Arc<crate::trace::Recorder>>,
) -> Result<Operator> {
    let knobs = Knobs::default();
    let mut op = base.clone();
    if let Some(r) = &recorder
        && !target.is_network()
    {
        op = op.layer(InProcessObserveLayer::new(r.clone()));
    }
    Ok(op
        .layer(trace_layer(target, &knobs, recorder)?)
        .layer(
            TimeoutLayer::new()
                .with_timeout(Duration::from_secs(30))
                .with_io_timeout(Duration::from_secs(15)),
        )
        // A few quick retries of temporary errors (throttling); persistent ones fail at once.
        .layer(
            RetryLayer::default()
                .with_jitter()
                .with_min_delay(Duration::from_millis(200))
                .with_max_times(4)
                .with_max_delay(Duration::from_secs(2)),
        ))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_config_hints_name_flags_and_only_missing_options() {
        let mut o = std::collections::BTreeMap::new();
        o.insert("container".to_owned(), "c".to_owned());
        let h = backend_config_hint("azblob", &o);
        assert!(!h.contains("`container`"), "{h}");
        assert!(h.contains("`endpoint`"), "{h}");
        assert!(h.contains("--option KEY=VALUE"), "{h}");
        assert!(h.contains("[targets.NAME.options]"), "{h}");
        assert!(!h.contains("-o KEY"), "{h}");
        let h = backend_config_hint("s3", &Default::default());
        assert!(h.contains("set `bucket`"), "{h}");
        let t = TargetSpec::from_url("azblob://c/p").unwrap();
        let e = base_operator(&t).unwrap_err();
        let b = crate::errors::classify(&e, "azblob");
        assert_eq!(b.code, ErrorCode::ConfigInvalid, "{b:?}");
        let hint = b.hint.unwrap_or_default();
        assert!(!hint.contains("`container`"), "{hint}");
        assert!(hint.contains("--option"), "{hint}");
    }
}
