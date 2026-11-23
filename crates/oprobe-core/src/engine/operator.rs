//! Building OpenDAL operators: the instrumented one under test and a plain admin one
//! (dataset seeding, verification, cleanup) that never pollutes measurements.

use std::sync::{Arc, Once};
use std::time::Duration;

use anyhow::{Context, Result};
use opendal::Operator;
use opendal::layers::{RetryLayer, TimeoutLayer};

use crate::config::TargetSpec;
use crate::errors::{ErrorCode, OprobeError};
use crate::net::{FidelityTransport, TlsClientOptions, TransportConfig};
use crate::operator::{HttpOptions, OperatorSpec};
use crate::trace::Recorder;
use crate::trace::layer::{InProcessObserveLayer, TraceContextLayer};

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
    if missing_service_feature(&target.scheme).is_some() {
        return Err(OprobeError::new(
            ErrorCode::StorageUnsupported,
            format!(
                "this oprobe was built without the `{}` service",
                target.scheme
            ),
        )
        .with_hint(
            "install a build with every service: `cargo install oprobe --features all-services`",
        )
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
        "{what}: pass `--set options.KEY=VALUE` (or `?KEY=VALUE` in the URL), or add it under \
         [operators.NAME.options] in the config file"
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

/// Prepare a target before its operator is built: detect the region of an S3 bucket that has
/// none configured.
pub async fn prepare_target(target: &mut TargetSpec) -> Result<()> {
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

/// The instrumented `HttpFetch`'s settings: the operator's HTTP options and the target's TLS
/// and proxy (no in-flight cap: see [`measured_operator`]).
pub fn transport_config(target: &TargetSpec, http: &HttpOptions) -> Result<TransportConfig> {
    Ok(TransportConfig {
        http: http.version,
        keep_alive: http.keep_alive,
        max_inflight: None,
        tls: tls_options(target)?,
        proxy: proxy_setting(target),
        // `OPROBE_TCP_INFO=0` turns the per-request kernel statistics off.
        tcp_info: std::env::var("OPROBE_TCP_INFO").map_or(true, |v| v != "0"),
    })
}

/// The tracing layer: OpenDAL's own HTTP client, instrumented (the fidelity transport).
pub fn trace_layer(
    target: &TargetSpec,
    http: &HttpOptions,
    recorder: Option<Arc<Recorder>>,
) -> Result<TraceContextLayer> {
    let https = target.endpoint().is_none_or(|e| e.starts_with("https"));
    Ok(TraceContextLayer::new(FidelityTransport::new(
        transport_config(target, http)?,
        https,
        recorder,
    )?))
}

/// The measured operator: the service through the instrumented `HttpFetch` (every request is
/// traced into `recorder`), wrapped in `spec`'s layers.
pub async fn measured_operator(
    base: &Operator,
    target: &TargetSpec,
    spec: &OperatorSpec,
    recorder: &Arc<Recorder>,
    tree_registry: Option<await_tree::Registry>,
) -> Result<Operator> {
    let mut op = base.clone();
    if !target.is_network() {
        op = op.layer(InProcessObserveLayer::new(recorder.clone()));
    }
    // `layers.concurrent_limit.http` is enforced by the instrumented transport itself, with the
    // semantics of `ConcurrentLimitLayer::with_http_concurrent_limit` (a permit per request,
    // held until its body is consumed), so the wait is measured as the request's queue stage;
    // the layer would wait outside the transport, where it cannot be attributed.
    let https = target.endpoint().is_none_or(|e| e.starts_with("https"));
    let mut cfg = transport_config(target, &spec.http)?;
    cfg.max_inflight = spec.http_limit();
    op = op.layer(TraceContextLayer::new(FidelityTransport::new(
        cfg,
        https,
        Some(recorder.clone()),
    )?));
    // Async call stacks: operations are registered as await-tree roots by the runner, OpenDAL
    // sub-tasks by `ScopePropagatingExecutor`, and the transports add zero-allocation spans per
    // request. OpenDAL's `AwaitTreeLayer` is not used: it wraps every call and read chunk and
    // cost ~14% throughput on small requests (examples/overhead.rs).
    let _ = tree_registry;
    recorder.set_cache_enabled(spec.layers.cache.is_some());
    let mut layers = spec.clone();
    if let Some(c) = &mut layers.layers.concurrent_limit {
        c.http = None;
        if c.operations.is_none() {
            layers.layers.concurrent_limit = None;
        }
    }
    crate::operator::build::apply_layers(op, &layers, true).await
}

/// Fail fast when the endpoint cannot be reached at all (connection refused, unknown host):
/// the admin operator would otherwise retry every dataset upload for minutes. One `stat` on a
/// key that does not exist, without retries; any HTTP answer (404, 403, ...) passes.
pub async fn check_reachable(base: &Operator, target: &TargetSpec) -> Result<()> {
    check_reachable_with(base, target, &HttpOptions::default()).await
}

/// [`check_reachable`] with the run's transport and HTTP version (a server that only speaks
/// HTTP/2 is reachable for an HTTP/2 client).
pub async fn check_reachable_with(
    base: &Operator,
    target: &TargetSpec,
    http: &HttpOptions,
) -> Result<()> {
    let h = HttpOptions {
        version: http.version,
        ..http.clone()
    };
    let probe = base
        .clone()
        .layer(trace_layer(target, &h, None)?)
        .layer(TimeoutLayer::new().with_timeout(Duration::from_secs(20)));
    let Err(e) = probe.stat(".oprobe-reachability-check").await else {
        return Ok(());
    };
    let chain = error_chain(&e);
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
        return Err(unreachable_error(target, &chain).into());
    }
    Ok(())
}

/// An OpenDAL error and its sources on one line, each text once (OpenDAL's own message already
/// names its first source).
fn error_chain(e: &opendal::Error) -> String {
    let mut chain = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        let t = s.to_string();
        if !chain.contains(&t) {
            chain.push_str(": ");
            chain.push_str(&t);
        }
        src = s.source();
    }
    chain
}

/// `host:port` of an endpoint URL (the scheme's default port when it names none).
fn host_port(endpoint: &str) -> Option<String> {
    let u = url::Url::parse(endpoint).ok()?;
    let host = u.host_str().filter(|h| !h.is_empty())?;
    Some(match u.port_or_known_default() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    })
}

/// The error of a target that cannot be reached: one plain cause (`connection refused by
/// 127.0.0.1:9999`), the operator, and the storage library's report (`chain`) as the detail.
pub fn unreachable_error(target: &TargetSpec, chain: &str) -> OprobeError {
    let endpoint = target.endpoint();
    let at = endpoint
        .as_deref()
        .and_then(host_port)
        .or(endpoint)
        .unwrap_or_else(|| target.display());
    let l = chain.to_ascii_lowercase();
    let dns = [
        "dns error",
        "failed to lookup address",
        "name or service not known",
        "nodename nor servname",
        "no such host",
    ]
    .iter()
    .any(|n| l.contains(n));
    let refused = ["connection refused", "actively refused", "os error 10061"]
        .iter()
        .any(|n| l.contains(n));
    let (code, cause, hint) = if dns {
        let host = at.rsplit_once(':').map_or(at.as_str(), |(h, _)| h);
        (
            ErrorCode::NetDns,
            format!("cannot resolve {host}"),
            "check the endpoint's host name (`--set options.endpoint=URL`), DNS and the proxy",
        )
    } else if refused {
        (
            ErrorCode::NetUnreachable,
            format!("connection refused by {at}"),
            "check that the service is running at that address and the endpoint is right \
             (`--set options.endpoint=URL`)",
        )
    } else {
        (
            ErrorCode::NetUnreachable,
            format!("cannot connect to {at}"),
            "check the endpoint (`--set options.endpoint=URL`), the network path and the proxy",
        )
    };
    OprobeError::new(code, format!("{cause} ({})", target.display()))
        .with_hint(hint)
        .with_detail(chain.trim())
}

/// What [`credential_source`] says of a target that sends unsigned requests.
pub const UNSIGNED: &str = "unsigned requests";

/// Where the credentials of `target` come from, as far as the configuration and the
/// environment tell (`AWS_ACCESS_KEY_ID`, `~/.aws/credentials`, an operator option); `None`
/// when nothing names them (the service's metadata endpoint, or no credentials at all).
pub fn credential_source(target: &TargetSpec) -> Option<String> {
    let opt = |k: &str| target.options.get(k).is_some_and(|v| !v.trim().is_empty());
    let env = |k: &str| std::env::var(k).is_ok_and(|v| !v.trim().is_empty());
    let on = |k: &str| target.options.get(k).is_some_and(|v| v.trim() == "true");
    if on("skip_signature") || on("allow_anonymous") {
        return Some(UNSIGNED.into());
    }
    let option = |k: &str| format!("the operator's `{k}` option");
    let home = || {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
    };
    let config_load = !on("disable_config_load");
    match target.scheme.as_str() {
        "s3" => {
            if opt("access_key_id") {
                return Some(option("access_key_id"));
            }
            if env("AWS_ACCESS_KEY_ID") {
                return Some("AWS_ACCESS_KEY_ID".into());
            }
            if !config_load {
                return None;
            }
            if env("AWS_WEB_IDENTITY_TOKEN_FILE") {
                return Some("AWS_WEB_IDENTITY_TOKEN_FILE".into());
            }
            let profile = std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".into());
            if let Ok(f) = std::env::var("AWS_SHARED_CREDENTIALS_FILE") {
                return Some(format!("{f} (profile {profile})"));
            }
            if home().is_some_and(|h| h.join(".aws/credentials").is_file()) {
                return Some(format!("~/.aws/credentials (profile {profile})"));
            }
            None
        }
        "gcs" => {
            for k in ["credential", "credential_path", "token"] {
                if opt(k) {
                    return Some(option(k));
                }
            }
            if env("GOOGLE_APPLICATION_CREDENTIALS") {
                return Some("GOOGLE_APPLICATION_CREDENTIALS".into());
            }
            if config_load
                && home().is_some_and(|h| {
                    h.join(".config/gcloud/application_default_credentials.json")
                        .is_file()
                })
            {
                return Some("gcloud's application default credentials".into());
            }
            None
        }
        "azblob" => {
            for k in ["account_key", "sas_token"] {
                if opt(k) {
                    return Some(option(k));
                }
            }
            [
                "AZURE_STORAGE_CONNECTION_STRING",
                "AZURE_STORAGE_ACCOUNT_KEY",
                "AZURE_STORAGE_SAS_TOKEN",
            ]
            .into_iter()
            .find(|k| env(k))
            .map(str::to_owned)
        }
        _ => [
            "access_key_id",
            "account_key",
            "sas_token",
            "credential",
            "password",
            "token",
        ]
        .into_iter()
        .find(|k| opt(k))
        .map(option),
    }
}

/// Where to put credentials for `scheme` (the hint of `auth.no_credentials`).
fn credentials_hint(scheme: &str) -> &'static str {
    match scheme {
        "s3" => {
            "set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (or AWS_PROFILE), or send unsigned \
             requests to a public bucket with --set options.skip_signature=true"
        }
        "gcs" => {
            "set GOOGLE_APPLICATION_CREDENTIALS to a service account key file, or send unsigned \
             requests to a public bucket with --set options.skip_signature=true"
        }
        "azblob" => {
            "set AZURE_STORAGE_ACCOUNT_KEY (or AZURE_STORAGE_SAS_TOKEN), or send unsigned \
             requests to a public container with --set options.skip_signature=true"
        }
        _ => {
            "set credentials for the operator (OpenDAL's credential chain for the service), or \
             send unsigned requests to a public bucket with --set options.skip_signature=true"
        }
    }
}

/// A preflight failure of `target`, stated plainly: what happened, at which endpoint, with
/// which credentials (`credentials rejected by s3.eu-west-1.amazonaws.com:443 (credentials from
/// AWS_ACCESS_KEY_ID): InvalidAccessKeyId: …`); the storage library's report becomes the
/// detail. Other errors pass through unchanged.
pub fn explain_preflight(err: anyhow::Error, target: &TargetSpec) -> anyhow::Error {
    let Some(e) = err.downcast_ref::<OprobeError>() else {
        return err;
    };
    let at = target
        .endpoint()
        .map(|ep| host_port(&ep).unwrap_or(ep))
        .unwrap_or_else(|| target.display());
    let creds = match credential_source(target) {
        Some(s) if s == UNSIGNED => format!(" ({s})"),
        Some(s) => format!(" (credentials from {s})"),
        None => " (credentials from OpenDAL's default chain)".to_owned(),
    };
    let parts = crate::errors::provider_parts(&e.message, e.code);
    let message = match (e.code, parts) {
        (ErrorCode::AuthDenied | ErrorCode::AuthNoCredentials, Some((what, rest))) => {
            format!("{what} by {at}{creds}: {rest}")
        }
        (ErrorCode::AuthDenied, None) => format!("access denied by {at}{creds}"),
        (ErrorCode::AuthNoCredentials, None) => format!("no credentials found for {at}"),
        (_, Some((what, rest))) => format!("{what} at {at}: {rest}"),
        (_, None) => return err,
    };
    let hint = match e.code {
        ErrorCode::AuthNoCredentials => credentials_hint(&target.scheme).to_owned(),
        _ => e.hint.clone().unwrap_or_default(),
    };
    OprobeError::new(e.code, message)
        .with_hint(hint)
        .with_detail(e.message.clone())
        .into()
}

/// With `--http h2`, check the target speaks HTTP/2 before anything else (design v5 §3, M6):
/// an endpoint without h2c (plain HTTP) or without `h2` in ALPN (TLS) would otherwise fail
/// every request of the run.
pub async fn check_protocol(
    base: &Operator,
    target: &TargetSpec,
    http: &HttpOptions,
) -> Result<()> {
    if http.version != crate::net::HttpVersion::H2 || !target.is_network() {
        return Ok(());
    }
    let probe = base
        .clone()
        .layer(trace_layer(target, http, None)?)
        .layer(TimeoutLayer::new().with_timeout(Duration::from_secs(10)));
    let Err(e) = probe.stat(".oprobe-protocol-check").await else {
        return Ok(());
    };
    if e.kind() == opendal::ErrorKind::NotFound || e.kind() == opendal::ErrorKind::PermissionDenied
    {
        // The server answered over HTTP/2.
        return Ok(());
    }
    let chain = error_chain(&e);
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
    let http = HttpOptions::default();
    let mut op = base.clone();
    if let Some(r) = &recorder
        && !target.is_network()
    {
        op = op.layer(InProcessObserveLayer::new(r.clone()));
    }
    Ok(op
        .layer(trace_layer(target, &http, recorder)?)
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
    Ok(base
        .clone()
        .layer(trace_layer(target, &HttpOptions::default(), None)?)
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
        assert!(h.contains("--set options.KEY=VALUE"), "{h}");
        assert!(h.contains("[operators.NAME.options]"), "{h}");
        assert!(!h.contains("-o KEY"), "{h}");
        let h = backend_config_hint("s3", &Default::default());
        assert!(h.contains("set `bucket`"), "{h}");
        let t = TargetSpec::from_url("azblob://c/p").unwrap();
        let e = base_operator(&t).unwrap_err();
        let b = crate::errors::classify(&e, "azblob");
        assert_eq!(b.code, ErrorCode::ConfigInvalid, "{b:?}");
        let hint = b.hint.unwrap_or_default();
        assert!(!hint.contains("`container`"), "{hint}");
        assert!(hint.contains("--set options."), "{hint}");
    }

    /// An unreachable endpoint: the plain cause first, OpenDAL's report only in the detail.
    #[test]
    fn unreachable_targets_state_one_plain_cause() {
        let mut t = TargetSpec::from_url("s3://b/x").unwrap();
        t.options
            .insert("endpoint".into(), "http://127.0.0.1:9999".into());
        let dump = "Unexpected (temporary) at stat, context: { uri: http://127.0.0.1:9999/b/x, \
                    service: s3 } => send http request, source: error sending request: client \
                    error (Connect): tcp connect error: Connection refused (os error 111) ";
        let e: anyhow::Error = unreachable_error(&t, dump).into();
        let b = crate::errors::classify(&e, "s3");
        assert_eq!(b.code, ErrorCode::NetUnreachable);
        assert_eq!(b.message, "connection refused by 127.0.0.1:9999 (s3://b/x)");
        assert_eq!(b.detail.as_deref(), Some(dump.trim()));
        let hint = b.hint.unwrap();
        assert_eq!(hint, hint.trim_end(), "no trailing space: {hint:?}");
        assert!(hint.contains("--set options.endpoint="), "{hint}");
        let b = crate::errors::classify(
            &unreachable_error(&t, "dns error: failed to lookup address information").into(),
            "s3",
        );
        assert_eq!(b.code, ErrorCode::NetDns);
        assert_eq!(b.message, "cannot resolve 127.0.0.1 (s3://b/x)");
    }

    /// A rejected key names the endpoint and where the key came from; the dump is the detail.
    #[test]
    fn auth_errors_name_the_endpoint_and_the_credential_source() {
        let mut t = TargetSpec::from_url("s3://b").unwrap();
        t.options.insert(
            "endpoint".into(),
            "https://s3.eu-west-1.amazonaws.com".into(),
        );
        t.options.insert("access_key_id".into(), "AKIA".into());
        let dump = "preflight: cannot list the target: PermissionDenied (persistent) at list => \
                    S3Error { code: \"InvalidAccessKeyId\", message: \"The AWS Access Key Id you \
                    provided does not exist in our records.\" }";
        let e = anyhow::Error::new(
            OprobeError::new(ErrorCode::AuthDenied, dump).with_hint("check the credentials"),
        );
        let b = crate::errors::classify(&explain_preflight(e, &t), "s3");
        assert_eq!(b.code, ErrorCode::AuthDenied);
        assert_eq!(
            b.message,
            "credentials rejected by s3.eu-west-1.amazonaws.com:443 (credentials from the \
             operator's `access_key_id` option): InvalidAccessKeyId: The AWS Access Key Id you \
             provided does not exist in our records."
        );
        assert_eq!(b.detail.as_deref(), Some(dump));
        // No credentials at all: where to put them, per service.
        let mut g = TargetSpec::from_url("gs://b").unwrap();
        g.options
            .insert("disable_config_load".into(), "true".into());
        let e = anyhow::Error::new(OprobeError::new(
            ErrorCode::AuthNoCredentials,
            "preflight: cannot list the target: ConfigInvalid => no valid credential found",
        ));
        let b = crate::errors::classify(&explain_preflight(e, &g), "gcs");
        assert_eq!(
            b.message,
            "no credentials found for storage.googleapis.com:443"
        );
        assert!(
            b.hint.unwrap().contains("GOOGLE_APPLICATION_CREDENTIALS"),
            "{:?}",
            b.detail
        );
        // Unsigned requests say so.
        g.options.insert("skip_signature".into(), "true".into());
        assert_eq!(credential_source(&g).as_deref(), Some(UNSIGNED));
    }
}
