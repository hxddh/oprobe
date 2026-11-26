//! The difference between two operators, as data, as Rust builder code and as TOML: what a
//! proven change looks like to copy into a program or an `op.toml`.

use std::fmt::Write as _;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::build::{Arg, CACHE_SHARDS, OpendalDefault, cache_max_entry_bytes, layer_params};
use super::*;
use crate::model::spec::REDACTED;

/// What generated Rust says in place of a secret option's value.
const SECRET_NOTE: &str = "a secret, not written here: your credential chain provides it";

/// Operations a concurrent-limit layer allows when only the HTTP limit is set (unlimited in
/// practice; `ConcurrentLimitLayer::new` needs a number).
pub const UNLIMITED_OPERATIONS: usize = 1 << 20;

/// How one part of the operator changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Changed,
    Removed,
}

/// One changed value (`path` in `--set` form; values in TOML form, absent when unset).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// The changes from operator `a` to operator `b`, with their Rust and TOML renderings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OperatorDiff {
    pub changes: Vec<Change>,
    /// The changed layers and options as Rust (`+` added, `~` changed, `-` removed).
    pub rust: String,
    /// The changed parts of `b` as `op.toml` lines.
    pub toml: String,
    /// The whole of `b` as Rust that compiles as written ([`to_rust`]: `build_operator()`,
    /// `reader()`, `writer()`), secrets left to the credential chain. The `rust` lines above
    /// show a change and do not paste.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub operator_rust: String,
}

impl OperatorDiff {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Flatten a serializable value into `prefix.key = toml` pairs (tables recurse).
fn flatten(prefix: &str, v: &toml::Value, out: &mut BTreeMap<String, String>) {
    match v {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let p = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&p, v, out);
            }
        }
        other => {
            out.insert(prefix.to_owned(), other.to_string());
        }
    }
}

/// Every set value of `s` as `path → toml`, the default layers materialized.
fn values(s: &OperatorSpec) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let doc = |v: &dyn erased::Ser| v.toml();
    out.insert("url".into(), toml::Value::String(s.url.clone()).to_string());
    for (k, v) in &s.options {
        out.insert(
            format!("options.{k}"),
            toml::Value::String(v.clone()).to_string(),
        );
    }
    let l = &s.layers;
    let mut layer = |name: &str, v: Option<toml::Value>| {
        if let Some(v) = v {
            out.insert(format!("layers.{name}"), "on".into());
            flatten(&format!("layers.{name}"), &v, &mut out);
        }
    };
    layer(
        "concurrent_limit",
        l.concurrent_limit.as_ref().map(|v| doc(v)),
    );
    layer("retry", l.retry.as_ref().map(|v| doc(v)));
    layer("timeout", l.timeout.as_ref().map(|v| doc(v)));
    layer("tail_cut", l.tail_cut.as_ref().map(|v| doc(v)));
    layer("throttle", l.throttle.as_ref().map(|v| doc(v)));
    layer("cache", l.cache.as_ref().map(|v| doc(v)));
    flatten("read", &doc(&s.read), &mut out);
    flatten("write", &doc(&s.write), &mut out);
    flatten("http", &doc(&s.http), &mut out);
    out
}

mod erased {
    pub trait Ser {
        fn toml(&self) -> toml::Value;
    }
    impl<T: serde::Serialize> Ser for T {
        fn toml(&self) -> toml::Value {
            toml::Value::try_from(self).unwrap_or_else(|_| toml::Value::Table(Default::default()))
        }
    }
}

/// The changes from `a` to `b`.
pub fn diff(a: &OperatorSpec, b: &OperatorSpec) -> OperatorDiff {
    let (va, vb) = (values(a), values(b));
    let mut keys: Vec<&String> = va.keys().chain(vb.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut changes = Vec::new();
    for k in keys {
        let (x, y) = (va.get(k), vb.get(k));
        if x == y {
            continue;
        }
        // A whole layer added or removed is one change, not one per field.
        if let Some(layer) = layer_of(k)
            && k.as_str() != layer
            && va.contains_key(layer) != vb.contains_key(layer)
        {
            continue;
        }
        changes.push(Change {
            path: k.clone(),
            kind: match (x, y) {
                (None, Some(_)) => ChangeKind::Added,
                (Some(_), None) => ChangeKind::Removed,
                _ => ChangeKind::Changed,
            },
            from: x.cloned(),
            to: y.cloned(),
        });
    }
    // Secrets never leave as values: a changed secret option shows as changed, redacted.
    for c in &mut changes {
        for v in [&mut c.from, &mut c.to].into_iter().flatten() {
            *v = redact_value(&c.path, v);
        }
    }
    OperatorDiff {
        rust: rust_diff(a, b, &changes),
        toml: toml_diff(b, &changes),
        operator_rust: to_rust(&b.redacted()),
        changes,
    }
}

/// A value of `path` (TOML or plain) safe to show: a secret option's redacted, a URL's
/// credentials removed.
fn redact_value(path: &str, v: &str) -> String {
    let shown = super::redacted_edit(path, v.trim_matches('"'));
    let value = shown.split_once('=').map_or("", |x| x.1);
    if value == v.trim_matches('"') {
        v.to_owned()
    } else if v.starts_with('"') {
        toml::Value::String(value.to_owned()).to_string()
    } else {
        value.to_owned()
    }
}

/// `layers.retry` of `layers.retry.max_times`.
fn layer_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("layers.")?;
    let name = rest.split('.').next()?;
    Some(&path[..7 + name.len()])
}

fn rust_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if !d.subsec_nanos().is_multiple_of(1_000_000) {
        format!("Duration::from_nanos({})", d.as_nanos())
    } else if ms.is_multiple_of(1000) {
        format!("Duration::from_secs({})", ms / 1000)
    } else {
        format!("Duration::from_millis({ms})")
    }
}

fn rust_size(b: ByteSize) -> String {
    let v = b.0;
    for (unit, name) in [
        (1u64 << 30, "1024 * 1024 * 1024"),
        (1 << 20, "1024 * 1024"),
        (1 << 10, "1024"),
    ] {
        if v >= unit && v.is_multiple_of(unit) {
            return if v == unit {
                name.to_owned()
            } else {
                format!("{} * {name}", v / unit)
            };
        }
    }
    v.to_string()
}

/// One builder argument as Rust.
fn rust_arg(a: Arg) -> String {
    match a {
        Arg::Count(n) if n == UNLIMITED_OPERATIONS => "1 << 20".to_owned(),
        Arg::Count(n) => n.to_string(),
        Arg::F32(f) => format!("{f:?}"),
        Arg::F64(f) => format!("{f:?}"),
        Arg::Switch(b) => b.to_string(),
        Arg::Time(d) => rust_duration(d),
        Arg::Bytes(b) => rust_size(ByteSize(b)),
    }
}

/// The builder expression of one layer of `s` (`None`: not in the stack): the constructor and
/// every applied parameter that differs from OpenDAL's default ([`layer_params`]), so the code
/// builds the layer that was measured.
pub fn layer_rust(s: &OperatorSpec, name: &str) -> Option<String> {
    let params = layer_params(s, name)?;
    let ctor: Vec<String> = params
        .iter()
        .filter(|p| p.opendal_default == OpendalDefault::Ctor)
        .map(|p| rust_arg(p.value))
        .collect();
    let mut calls = String::new();
    for p in params
        .iter()
        .filter(|p| p.opendal_default != OpendalDefault::Ctor && p.spelled())
    {
        match p.value {
            Arg::Switch(true) => {
                let _ = write!(calls, ".{}()", p.method);
            }
            Arg::Switch(false) => {}
            v => {
                let _ = write!(calls, ".{}({})", p.method, rust_arg(v));
            }
        }
    }
    let ctor = ctor.join(", ");
    Some(match name {
        "concurrent_limit" => format!("ConcurrentLimitLayer::new({ctor}){calls}"),
        "retry" => format!("RetryLayer::new(){calls}"),
        "timeout" => format!("TimeoutLayer::new(){calls}"),
        "tail_cut" if calls.is_empty() => "TailCutLayer::new()".to_owned(),
        "tail_cut" => format!("TailCutLayer::builder(){calls}.build()"),
        "throttle" => format!("ThrottleLayer::new({ctor})"),
        "cache" => cache_rust(params.first()?.value),
        _ => return None,
    })
}

/// The cache layer as [`super::build::memory_cache`] builds it and `apply_layers` installs it:
/// the same budget, shards, weighter and admission limit.
fn cache_rust(memory: Arg) -> String {
    let Arg::Bytes(memory_bytes) = memory else {
        unreachable!("cache parameter {memory:?}")
    };
    let memory = rust_arg(memory);
    let max_entry = rust_size(ByteSize(cache_max_entry_bytes(memory_bytes as usize) as u64));
    format!(
        "FoyerLayer::new(HybridCacheBuilder::new().memory({memory}).with_shards({CACHE_SHARDS}).with_weighter(|_: &FoyerKey, v: &FoyerValue| v.len()).with_filter(|_: &FoyerKey, v: &FoyerValue| v.len() <= {max_entry}).storage().build().await?).with_size_limit(..={max_entry})"
    )
}

/// The layer names in the order oprobe stacks them, innermost first: the one order the
/// measured operator is built in ([`super::build::apply_layers`]) and the generated Rust is
/// written in. Read outermost first, a call passes
/// `cache → retry → throttle → concurrent_limit → tail_cut → timeout → service`:
///
/// - `timeout` and `tail_cut` time one call to the service and nothing else. `tail_cut` is
///   the outer of the two, so its default ceiling is the timeout that severs the same call
///   ([`OperatorSpec::call_timeout`]).
/// - `concurrent_limit` is outside both timers: waiting for an operation permit neither
///   consumes a timeout nor counts as latency that `tail_cut` would cut (and re-issue into
///   the same queue).
/// - `throttle` is outside the limiter and the timers: a write waits for its bytes before it
///   takes an operation permit, and the wait is not a slow call. (OpenDAL keeps the permit
///   of a writer, a lister or a deleter from its first call until it is dropped, so the later
///   writes of a multi-part upload are throttled with the permit held.)
/// - `retry` is outside everything that can fail per attempt: every attempt is throttled,
///   limited and timed again, and a `stat` or a read gives its permit back during the
///   backoff.
/// - `cache` is outermost: a hit touches none of the others.
///
/// One wait cannot be moved: the HTTP permit of `concurrent_limit.http` is taken inside the
/// service (OpenDAL wraps the HTTP transport), so it is always inside `timeout` and
/// `tail_cut`.
pub const LAYER_ORDER: [&str; 6] = [
    "timeout",
    "tail_cut",
    "concurrent_limit",
    "throttle",
    "retry",
    "cache",
];

fn read_rust(s: &OperatorSpec) -> String {
    let mut e = "op.reader_with(path)".to_owned();
    if let Some(c) = s.read.chunk {
        let _ = write!(e, ".chunk({})", rust_size(c));
    }
    if let Some(c) = s.read.concurrent {
        let _ = write!(e, ".concurrent({c})");
    }
    if let Some(g) = s.read.gap {
        let _ = write!(e, ".gap({})", rust_size(g));
    }
    e
}

fn write_rust(s: &OperatorSpec) -> String {
    let mut e = "op.writer_with(path)".to_owned();
    if let Some(c) = s.write.chunk {
        let _ = write!(e, ".chunk({})", rust_size(c));
    }
    if let Some(c) = s.write.concurrent {
        let _ = write!(e, ".concurrent({c})");
    }
    e
}

fn need_http_client(s: &OperatorSpec) -> bool {
    s.http.version != HttpVersion::Auto
        || !s.http.keep_alive
        || s.http.insecure
        || s.http.ca_cert.is_some()
        || s.http.proxy.is_some()
}

fn http_client_expr(s: &OperatorSpec) -> (String, bool) {
    let mut e = "reqwest::Client::builder()".to_owned();
    let (_, resolved_opts) = resolved_rust_target(s);
    let h2c = s.url.starts_with("http://")
        || s.options
            .get("endpoint")
            .is_some_and(|u| u.trim().starts_with("http://"))
        || resolved_opts
            .get("endpoint")
            .is_some_and(|u| u.trim().starts_with("http://"));
    match s.http.version {
        HttpVersion::H2 if h2c => e += ".http2_prior_knowledge()",
        HttpVersion::H2 => {}
        // `auto` is OpenDAL's default client, which speaks HTTP/1.1 only: a client built here
        // must say so, or reqwest's `http2` feature would negotiate h2.
        HttpVersion::H1 | HttpVersion::Auto => e += ".http1_only()",
    }
    if !s.http.keep_alive {
        e += ".pool_max_idle_per_host(0)";
    }
    if s.http.insecure {
        e += ".danger_accept_invalid_certs(true)";
    }
    if let Some(ca) = &s.http.ca_cert {
        let _ = write!(
            e,
            ".add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read({ca:?})?)?)"
        );
    }
    if let Some(proxy) = &s.http.proxy {
        if proxy.eq_ignore_ascii_case("none") {
            e += ".no_proxy()";
        } else {
            let red = crate::util::redact_url(proxy);
            let _ = write!(e, ".proxy(reqwest::Proxy::all({red:?})?)");
        }
    }
    e += ".build()?";
    (e, s.http.version == HttpVersion::H2 && !h2c)
}

fn http_rust(s: &OperatorSpec) -> String {
    let (e, alpn_h2) = http_client_expr(s);
    let note = if alpn_h2 {
        "reqwest `http2` feature negotiates ALPN h2; install with OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client)))"
    } else {
        "install with OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client)))"
    };
    format!("{e} // {note}")
}

fn rust_diff(a: &OperatorSpec, b: &OperatorSpec, changes: &[Change]) -> String {
    let mut out = String::new();
    let touched = |prefix: &str| {
        changes
            .iter()
            .any(|c| c.path == prefix || c.path.starts_with(&format!("{prefix}.")))
    };
    let shown = b.redacted().options;
    for c in changes
        .iter()
        .filter(|c| c.path.starts_with("options.") || c.path == "url")
    {
        let sign = match c.kind {
            ChangeKind::Added => "+",
            ChangeKind::Changed => "~",
            ChangeKind::Removed => "-",
        };
        let value = c.to.as_deref().or(c.from.as_deref()).unwrap_or_default();
        // A secret is never code to paste: the line says where it comes from instead. A
        // `${NAME}` reference is read from the environment, as the whole operator reads it.
        // The option as the TOML shows it (`b`'s, a reference kept), else the removed value.
        let option = c.path.strip_prefix("options.").and_then(|k| shown.get(k));
        let plain = option.map_or(value.trim_matches('"'), String::as_str);
        let _ = match rust_string(plain) {
            _ if plain == REDACTED => writeln!(out, "{sign} {} // {SECRET_NOTE}", c.path),
            (read, true) if option.is_some() => writeln!(out, "{sign} {} = {read}", c.path),
            _ => writeln!(out, "{sign} {} = {value}", c.path),
        };
    }
    for name in LAYER_ORDER {
        if !touched(&format!("layers.{name}")) {
            continue;
        }
        match (layer_rust(a, name), layer_rust(b, name)) {
            (None, Some(e)) => {
                let _ = writeln!(out, "+ {e}");
            }
            (Some(_), Some(e)) => {
                let _ = writeln!(out, "~ {e}");
            }
            (Some(e), None) => {
                let ty = e.split("::").next().unwrap_or(&e).to_owned();
                let _ = writeln!(out, "- {ty}");
            }
            (None, None) => {}
        }
    }
    if touched("read") {
        let _ = writeln!(out, "~ {}", read_rust(b));
    }
    if touched("write") {
        let _ = writeln!(out, "~ {}", write_rust(b));
    }
    if touched("http") {
        let _ = writeln!(out, "~ {}", http_rust(b));
    }
    out
}

fn toml_diff(b: &OperatorSpec, changes: &[Change]) -> String {
    use erased::Ser;
    let mut doc = toml::map::Map::new();
    fn put(
        doc: &mut toml::map::Map<String, toml::Value>,
        section: &str,
        key: &str,
        v: toml::Value,
    ) {
        if let Some(t) = doc
            .entry(section.to_owned())
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
        {
            t.insert(key.to_owned(), v);
        }
    }
    let mut removed = Vec::new();
    for c in changes {
        let parts: Vec<&str> = c.path.split('.').collect();
        match parts[0] {
            "url" => {
                doc.insert("url".into(), toml::Value::String(b.display_url()));
            }
            "options" | "read" | "write" | "http" => {
                let value = match parts[0] {
                    "options" => b
                        .redacted()
                        .options
                        .get(parts[1])
                        .map(|v| toml::Value::String(v.clone())),
                    "read" => b.read.toml().get(parts[1]).cloned(),
                    "write" => b.write.toml().get(parts[1]).cloned(),
                    _ => b.http.toml().get(parts[1]).cloned(),
                };
                match value {
                    Some(v) => put(&mut doc, parts[0], parts[1], v),
                    None => removed.push(c.path.clone()),
                }
            }
            "layers" => {
                let name = parts[1];
                let l = &b.layers;
                // Only the fields that differ from the layer's defaults (`true`: the layer
                // with its defaults): the TOML applied to `a` still gives `b`.
                let v = match name {
                    "concurrent_limit" => l
                        .concurrent_limit
                        .as_ref()
                        .map(|x| changed_fields(x.toml(), ConcurrentLimit::default().toml())),
                    "retry" => l
                        .retry
                        .as_ref()
                        .map(|x| changed_fields(x.toml(), Retry::default().toml())),
                    "timeout" => l
                        .timeout
                        .as_ref()
                        .map(|x| changed_fields(x.toml(), Timeout::default().toml())),
                    "tail_cut" => l
                        .tail_cut
                        .as_ref()
                        .map(|x| changed_fields(x.toml(), TailCut::default().toml())),
                    "throttle" => l
                        .throttle
                        .as_ref()
                        .map(|x| changed_fields(x.toml(), Throttle::default().toml())),
                    "cache" => l
                        .cache
                        .as_ref()
                        .map(|x| changed_fields(x.toml(), Cache::default().toml())),
                    _ => None,
                }
                .unwrap_or(toml::Value::Boolean(false));
                put(&mut doc, "layers", name, v);
            }
            _ => {}
        }
    }
    let mut out = String::new();
    if let Some(u) = doc.remove("url") {
        let _ = writeln!(out, "url = {u}");
    }
    for (name, t) in &doc {
        let toml::Value::Table(t) = t else { continue };
        let _ = writeln!(out, "[{name}]");
        for (k, v) in t {
            let _ = writeln!(out, "{k} = {}", inline(v));
        }
    }
    for r in removed {
        let _ = writeln!(out, "# remove {r}");
    }
    out
}

/// The fields of a layer table `v` that differ from its defaults `d`; `true` when none do.
fn changed_fields(v: toml::Value, d: toml::Value) -> toml::Value {
    let (toml::Value::Table(t), toml::Value::Table(d)) = (v, d) else {
        return toml::Value::Boolean(true);
    };
    let kept: toml::map::Map<String, toml::Value> =
        t.into_iter().filter(|(k, v)| d.get(k) != Some(v)).collect();
    if kept.is_empty() {
        toml::Value::Boolean(true)
    } else {
        toml::Value::Table(kept)
    }
}

/// A TOML value on one line (tables inline).
fn inline(v: &toml::Value) -> String {
    match v {
        toml::Value::Table(t) => {
            let items: Vec<String> = t
                .iter()
                .map(|(k, v)| format!("{k} = {}", inline(v)))
                .collect();
            format!("{{ {} }}", items.join(", "))
        }
        other => other.to_string(),
    }
}

const CANONICAL_URI_SCHEMES: &[&str] = &[
    "s3", "gcs", "azblob", "azdls", "azfile", "oss", "cos", "obs", "tos", "b2", "swift", "webhdfs",
    "upyun", "http", "fs", "memory",
];

fn resolved_rust_target(s: &OperatorSpec) -> (String, BTreeMap<String, String>) {
    let red = s.redacted();
    let url = red.display_url();
    let raw_scheme = url
        .split_once("://")
        .map(|(k, _)| k.to_ascii_lowercase())
        .unwrap_or_default();
    if raw_scheme == "lab" {
        let mut opts = red.options.clone();
        let rest = url.strip_prefix("lab://").unwrap_or_default();
        let path = rest.split_once('/').map_or("", |(_, p)| p);
        let root = path.split('?').next().unwrap_or_default().trim_matches('/');
        opts.entry("bucket".into())
            .or_insert_with(|| crate::lab::LAB_BUCKET.into());
        opts.entry("region".into())
            .or_insert_with(|| "us-east-1".into());
        opts.entry("endpoint".into())
            .or_insert_with(|| "http://127.0.0.1:9000".into());
        opts.entry("disable_config_load".into())
            .or_insert_with(|| "true".into());
        if !root.is_empty() {
            opts.entry("root".into())
                .or_insert_with(|| format!("/{root}"));
        }
        return ("s3".into(), opts);
    }
    if let Ok(mut t) = red.target() {
        t.options.remove("ca_cert_pem");
        return (t.scheme, t.options);
    }
    if let Ok(mut t) = crate::config::TargetSpec::from_url(&url) {
        for (k, v) in red.options {
            t.options.insert(k, v);
        }
        t.options.remove("ca_cert_pem");
        return (t.scheme, t.options);
    }
    let svc = crate::input::scheme(&raw_scheme).unwrap_or("s3").to_owned();
    (svc, red.options)
}

/// A string option as a Rust `String` expression, and whether it reads the environment: a
/// `${NAME}` reference becomes `std::env::var("NAME")?`, read when the code runs, as oprobe
/// expands it when it builds the operator ([`crate::config::expand_env`]).
fn rust_string(v: &str) -> (String, bool) {
    let escaped = |text: &str| text.replace('{', "{{").replace('}', "}}");
    let (mut format, mut vars) = (String::new(), Vec::new());
    let mut rest = v;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else { break };
        format += &escaped(&rest[..start]);
        format += "{}";
        vars.push(format!("std::env::var({:?})?", &after[..end]));
        rest = &after[end + 1..];
    }
    format += &escaped(rest);
    match vars.as_slice() {
        [] => (format!("{v:?}.to_owned()"), false),
        [var] if format == "{}" => (var.clone(), true),
        _ => (format!("format!({format:?}, {})", vars.join(", ")), true),
    }
}

/// The service of `s` as a Rust expression (indented for a function body, without the final
/// `;`), and whether it reads the environment.
///
/// A secret option is never written: a value oprobe redacted would replace the credential the
/// program loads itself, so it is left out with a comment; a `${NAME}` reference is read from
/// the environment, as the TOML keeps it.
fn service_rust(s: &OperatorSpec) -> (String, bool) {
    let disp_url = s.display_url();
    let url_scheme = disp_url
        .split_once("://")
        .map(|(k, _)| k.to_ascii_lowercase())
        .unwrap_or_default();
    let fs_ok = url_scheme != "fs" || (disp_url.starts_with("fs:///") && disp_url.len() > 6);
    let plain_uri = s.options.is_empty()
        && CANONICAL_URI_SCHEMES.contains(&url_scheme.as_str())
        && fs_ok
        && !disp_url.contains('?');
    if plain_uri {
        return (format!("Operator::from_uri({disp_url:?})?"), false);
    }
    let (scheme, opts) = resolved_rust_target(s);
    if opts.is_empty() {
        return (format!("Operator::via_iter({scheme:?}, [])?"), false);
    }
    let mut out = format!("Operator::via_iter(\n        {scheme:?},\n        [\n");
    let mut reads_env = false;
    for (k, v) in &opts {
        if v == REDACTED {
            let _ = writeln!(out, "            // {k}: {SECRET_NOTE}");
            continue;
        }
        let (value, env) = rust_string(v);
        reads_env |= env;
        let _ = writeln!(out, "            ({k:?}.to_owned(), {value}),");
    }
    out.push_str("        ],\n    )?");
    (out, reads_env)
}

/// The whole operator as Rust that compiles as written: the imports, `build_operator()` (the
/// service from its URL or its canonical scheme and options, every layer of the stack in
/// [`LAYER_ORDER`], the HTTP transport), and `reader()` / `writer()` with the `[read]` and
/// `[write]` options. `crates/oprobe-core/tests/codegen/` holds this output for a matrix of
/// operators, compiled against the workspace's OpenDAL.
pub fn to_rust(s: &OperatorSpec) -> String {
    let layers: Vec<(&str, String)> = LAYER_ORDER
        .iter()
        .filter_map(|name| Some((*name, layer_rust(s, name)?)))
        .collect();
    let need_http = need_http_client(s);
    let cache = s.layers.cache.is_some();
    let (service, reads_env) = service_rust(s);

    let mut out = String::new();
    if layers.iter().any(|(_, e)| e.contains("Duration::")) {
        out.push_str("use std::time::Duration;\n\n");
    }
    if cache {
        out.push_str("use foyer::HybridCacheBuilder;\n");
    }
    let mut layer_types: Vec<&str> = layers
        .iter()
        .flat_map(|(name, _)| match *name {
            "throttle" => &["ThrottleLayer"][..],
            "concurrent_limit" => &["ConcurrentLimitLayer"],
            "timeout" => &["TimeoutLayer"],
            "tail_cut" => &["TailCutLayer"],
            "retry" => &["RetryLayer"],
            // The cache's weighter names its key and value types.
            "cache" => &["FoyerKey", "FoyerLayer", "FoyerValue"],
            _ => unreachable!(),
        })
        .copied()
        .collect();
    layer_types.sort_unstable();
    let import = |module: &str, names: &[&str]| match names {
        [] => String::new(),
        [one] => format!("use {module}::{one};\n"),
        _ => format!("use {module}::{{{}}};\n", names.join(", ")),
    };
    out += &import("opendal::layers", &layer_types);
    let mut types = vec!["Operator"];
    if need_http {
        types.extend(["HttpTransporter", "OperationContext"]);
    }
    if !s.read.is_default() {
        types.push("Reader");
    }
    if !s.write.is_default() {
        types.push("Writer");
    }
    types.sort_unstable();
    out += &import("opendal", &types);
    if need_http {
        out.push_str("use opendal_http_transport_reqwest::ReqwestTransport;\n");
    }

    // Only OpenDAL can fail unless the code also reads the environment, builds an HTTP client
    // or builds the cache.
    let result = if reads_env || need_http || cache {
        "Result<Operator, Box<dyn std::error::Error + Send + Sync>>"
    } else {
        "opendal::Result<Operator>"
    };
    let asyncness = if cache { "async " } else { "" };
    let _ = writeln!(
        out,
        "\n/// The operator as oprobe measured it.\npub {asyncness}fn build_operator() -> {result} {{"
    );
    let _ = write!(out, "    let op = {service}");
    // A multi-line service expression ends at the statement's own indent.
    let indent = if service.ends_with("\n    )?") {
        "    "
    } else {
        "        "
    };
    if !layers.is_empty() {
        let _ = write!(
            out,
            "\n{indent}// Innermost first: each layer wraps the ones before it."
        );
    }
    if s.http_limit().is_some() {
        let _ = write!(
            out,
            "\n{indent}// The HTTP permits are taken inside the service: waiting for one counts against\n{indent}// the timeout and tail-cut layers."
        );
    }
    for (_, e) in &layers {
        let _ = write!(out, "\n{indent}.layer({e})");
    }
    out.push_str(";\n");
    if need_http {
        let (client, alpn_h2) = http_client_expr(s);
        if alpn_h2 {
            out.push_str("    // reqwest's `http2` feature negotiates h2 through ALPN.\n");
        }
        let _ = writeln!(out, "    let client = {client};");
        out.push_str(
            "    let op = op.with_context(OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client))));\n",
        );
    }
    out.push_str("    Ok(op)\n}\n");
    if !s.read.is_default() {
        let _ = write!(
            out,
            "\n/// A reader of `path` with the measured `[read]` options.\npub async fn reader(op: &Operator, path: &str) -> opendal::Result<Reader> {{\n    {}.await\n}}\n",
            read_rust(s)
        );
    }
    if !s.write.is_default() {
        let _ = write!(
            out,
            "\n/// A writer to `path` with the measured `[write]` options.\npub async fn writer(op: &Operator, path: &str) -> opendal::Result<Writer> {{\n    {}.await\n}}\n",
            write_rust(s)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator::build::Param;

    #[test]
    fn a_diff_renders_rust_and_toml() {
        let a = OperatorSpec::from_url("s3://b/x");
        let mut b = a.clone();
        b.set("layers.concurrent_limit.http", "24").unwrap();
        b.set("layers.retry.jitter", "true").unwrap();
        b.set("layers.retry.max_times", "4").unwrap();
        b.set("read.gap", "1MiB").unwrap();
        b.set("layers.timeout", "off").unwrap();
        let d = diff(&a, &b);
        let paths: Vec<&str> = d.changes.iter().map(|c| c.path.as_str()).collect();
        assert!(paths.contains(&"layers.concurrent_limit"), "{paths:?}");
        assert!(
            !paths.contains(&"layers.concurrent_limit.http"),
            "{paths:?}"
        );
        assert!(paths.contains(&"layers.retry.jitter"), "{paths:?}");
        assert!(paths.contains(&"read.gap"), "{paths:?}");
        assert!(
            d.rust
                .contains("+ ConcurrentLimitLayer::new(1 << 20).with_http_concurrent_limit(24)"),
            "{}",
            d.rust
        );
        assert!(
            d.rust
                .contains("~ RetryLayer::new().with_jitter().with_max_times(4)"),
            "{}",
            d.rust
        );
        assert!(d.rust.contains("- TimeoutLayer"), "{}", d.rust);
        assert!(
            d.rust.contains("op.reader_with(path).gap(1024 * 1024)"),
            "{}",
            d.rust
        );
        assert!(d.toml.contains("[layers]"), "{}", d.toml);
        assert!(
            d.toml.contains("concurrent_limit = { http = 24 }"),
            "{}",
            d.toml
        );
        assert!(d.toml.contains("timeout = false"), "{}", d.toml);
        // Only what differs from the defaults: not the whole retry table.
        assert!(
            d.toml.contains("retry = { jitter = true, max_times = 4 }"),
            "{}",
            d.toml
        );
        assert!(!d.toml.contains("min_delay"), "{}", d.toml);
        assert!(d.toml.contains("[read]\ngap = \"1MiB\""), "{}", d.toml);
        // The TOML applied to `a` gives `b`.
        let mut merged = toml::Value::try_from(&a).unwrap();
        let patch: toml::Value = toml::from_str(&d.toml).unwrap();
        for (k, v) in patch.as_table().unwrap() {
            match (merged.get_mut(k), v) {
                (Some(toml::Value::Table(t)), toml::Value::Table(p)) => {
                    for (pk, pv) in p {
                        t.insert(pk.clone(), pv.clone());
                    }
                }
                _ => {
                    merged.as_table_mut().unwrap().insert(k.clone(), v.clone());
                }
            }
        }
        let merged: OperatorSpec = merged.try_into().unwrap();
        assert_eq!(merged, b);
        assert!(diff(&b, &b).is_empty());
    }

    #[test]
    fn the_whole_operator_renders_as_rust() {
        let s = OperatorSpec::parse(
            "url = \"s3://b/x\"\n[layers]\ntail_cut = true\nthrottle = { bandwidth = \"100MiB\", burst = \"16MiB\" }\ncache = { memory = \"256MiB\" }\n[read]\nconcurrent = 4\n",
        )
        .unwrap();
        let r = to_rust(&s);
        assert!(r.contains("Operator::from_uri(\"s3://b/x\")"), "{r}");
        assert!(
            r.contains(".layer(ThrottleLayer::new(100 * 1024 * 1024, 16 * 1024 * 1024))"),
            "{r}"
        );
        // oprobe's tail-cut parameters differ from OpenDAL's: they are spelled out.
        assert!(
            r.contains(".layer(TailCutLayer::builder().safety_factor(1.5).min_samples(32).min_deadline(Duration::from_millis(50)).max_deadline(Duration::from_secs(10)).build())"),
            "{r}"
        );
        assert!(r.contains(".layer(RetryLayer::new())"), "{r}");
        // The cache's budget is bytes: each entry weighs its value's length, as oprobe builds
        // it (foyer's default weighs every entry 1).
        assert!(
            r.contains(
                "FoyerLayer::new(HybridCacheBuilder::new().memory(256 * 1024 * 1024).with_shards(1).with_weighter(|_: &FoyerKey, v: &FoyerValue| v.len()).with_filter(|_: &FoyerKey, v: &FoyerValue| v.len() <= 256 * 1024 * 1024).storage().build().await?).with_size_limit(..=256 * 1024 * 1024)"
            ),
            "{r}"
        );
        assert!(
            r.contains("use opendal::layers::{") && r.contains("FoyerKey, FoyerLayer, FoyerValue"),
            "{r}"
        );
        assert!(r.contains("op.reader_with(path).concurrent(4)"), "{r}");
        // The read options are a function of their own: `path` is its parameter.
        assert!(
            r.contains("pub async fn reader(op: &Operator, path: &str) -> opendal::Result<Reader> {\n    op.reader_with(path).concurrent(4).await\n}"),
            "{r}"
        );
        // Innermost first, in the order the operator is built in.
        let at = |layer: &str| r.find(&format!(".layer({layer}")).unwrap();
        let order = [
            "TimeoutLayer",
            "TailCutLayer",
            "ThrottleLayer",
            "RetryLayer",
            "FoyerLayer",
        ];
        assert!(order.windows(2).all(|w| at(w[0]) < at(w[1])), "{r}");
    }

    /// The one order of the stack, outermost first: cache, retry, throttle, the limiter, then
    /// the two timers around the service. The measured operator is built by walking it
    /// (`apply_layers`) and so is the generated Rust, so they cannot disagree.
    #[test]
    fn the_stack_is_built_and_generated_in_one_order() {
        let outermost_first: Vec<&str> = LAYER_ORDER.iter().rev().copied().collect();
        assert_eq!(
            outermost_first,
            [
                "cache",
                "retry",
                "throttle",
                "concurrent_limit",
                "tail_cut",
                "timeout"
            ]
        );
        let s = OperatorSpec::parse(
            "url = \"memory://\"\n[layers]\nconcurrent_limit = { http = 4 }\ntail_cut = true\nthrottle = true\ncache = true\n",
        )
        .unwrap();
        let r = to_rust(&s);
        let generated: Vec<&str> = r
            .lines()
            .filter_map(|l| l.trim().strip_prefix(".layer("))
            .map(|l| l.split("::").next().unwrap())
            .collect();
        assert_eq!(
            generated,
            [
                "TimeoutLayer",
                "TailCutLayer",
                "ConcurrentLimitLayer",
                "ThrottleLayer",
                "RetryLayer",
                "FoyerLayer"
            ]
        );
        // The wait OpenDAL's design leaves inside the timers is said where the code is read.
        assert!(
            r.contains("// The HTTP permits are taken inside the service"),
            "{r}"
        );
    }

    /// The tail-cut ceiling is one the layer can reach: the shorter of the two timeouts that
    /// sever the calls it times (9.1 used the operation timeout, 60 s, under a 10 s io
    /// timeout: every deadline above 10 s ended as "io timeout"). A given `max_deadline` is
    /// kept; without the timeout layer the ceiling stays 60 s.
    #[test]
    fn the_tail_cut_ceiling_is_the_timeout_that_bounds_the_call() {
        let ceiling = |layers: &str| {
            let s =
                OperatorSpec::parse(&format!("url = \"s3://b\"\n[layers]\n{layers}\n")).unwrap();
            let p = layer_params(&s, "tail_cut").unwrap();
            let max = p.iter().find(|p| p.method == "max_deadline").unwrap().value;
            // The analysis replays the same parameters.
            let model = s.layers.tail_cut.clone().unwrap().params(s.call_timeout());
            assert_eq!(max, Arg::Time(model.max_deadline));
            let Arg::Time(d) = max else { unreachable!() };
            (d, layer_rust(&s, "tail_cut").unwrap())
        };
        let secs = Duration::from_secs;
        // The defaults: op 60 s, io 10 s.
        let (d, rust) = ceiling("tail_cut = true");
        assert_eq!(d, secs(10));
        assert!(
            rust.contains(".max_deadline(Duration::from_secs(10))"),
            "{rust}"
        );
        // The shorter of the two, whichever it is.
        assert_eq!(
            ceiling("tail_cut = true\ntimeout = { op = \"4s\", io = \"10s\" }").0,
            secs(4)
        );
        assert_eq!(
            ceiling("tail_cut = true\ntimeout = { op = \"30s\", io = \"2500ms\" }").0,
            Duration::from_millis(2500)
        );
        // Given: kept as given, even above the timeouts.
        assert_eq!(ceiling("tail_cut = { max_deadline = \"45s\" }").0, secs(45));
        // No timeout layer: nothing severs the call; OpenDAL's 60 s.
        assert_eq!(ceiling("tail_cut = true\ntimeout = false").0, secs(60));
        // Never below the floor (OpenDAL clamps the deadline to [min, max] and panics on
        // min > max).
        assert_eq!(
            ceiling("tail_cut = true\ntimeout = { op = \"20ms\", io = \"20ms\" }").0,
            Duration::from_millis(50)
        );
    }

    /// The generated cache is the measured cache: one admission rule
    /// ([`cache_max_entry_bytes`]) and one shard count, in the cache's filter and in the
    /// layer's size limit.
    #[test]
    fn the_generated_cache_is_the_measured_cache() {
        let s = OperatorSpec::parse("url = \"s3://b\"\n[layers]\ncache = { memory = \"3MiB\" }\n")
            .unwrap();
        let rust = layer_rust(&s, "cache").unwrap();
        let max_entry = cache_max_entry_bytes(3 << 20);
        assert_eq!(
            max_entry,
            3 << 20,
            "an object that fits the budget is admitted"
        );
        assert_eq!(rust_size(ByteSize(max_entry as u64)), "3 * 1024 * 1024");
        for part in [
            ".memory(3 * 1024 * 1024)".to_owned(),
            format!(".with_shards({CACHE_SHARDS})"),
            ".with_weighter(|_: &FoyerKey, v: &FoyerValue| v.len())".to_owned(),
            ".with_filter(|_: &FoyerKey, v: &FoyerValue| v.len() <= 3 * 1024 * 1024)".to_owned(),
            ".with_size_limit(..=3 * 1024 * 1024)".to_owned(),
        ] {
            assert!(rust.contains(&part), "{part} not in {rust}");
        }
    }

    /// Evaluate one argument of generated Rust.
    fn eval_arg(src: &str) -> Arg {
        let src = src.trim();
        if src == "1 << 20" {
            return Arg::Count(UNLIMITED_OPERATIONS);
        }
        for (f, scale) in [
            ("Duration::from_secs(", 1_000_000_000u128),
            ("Duration::from_millis(", 1_000_000),
            ("Duration::from_nanos(", 1),
        ] {
            if let Some(n) = src.strip_prefix(f).and_then(|r| r.strip_suffix(')')) {
                let n: u128 = n.parse().unwrap();
                return Arg::Time(Duration::from_nanos((n * scale) as u64));
            }
        }
        if src.contains('*') {
            let b: u64 = src
                .split('*')
                .map(|x| x.trim().parse::<u64>().unwrap())
                .product();
            return Arg::Bytes(b);
        }
        if src.contains('.') {
            return Arg::F64(src.parse().unwrap());
        }
        Arg::Count(src.parse().unwrap())
    }

    /// The parameters a layer expression of generated Rust applies: OpenDAL's defaults, then the
    /// constructor arguments and the called methods.
    fn emitted_params(expr: &str, applied: &[Param]) -> Vec<Param> {
        let mut out: Vec<Param> = applied
            .iter()
            .filter_map(|p| match p.opendal_default {
                OpendalDefault::Value(d) => Some(Param { value: d, ..*p }),
                _ => None,
            })
            .collect();
        // Constructor arguments: inside the first `(...)` of `X::new(...)` (or `.memory(...)`).
        let ctors: Vec<&Param> = applied
            .iter()
            .filter(|p| p.opendal_default == OpendalDefault::Ctor)
            .collect();
        let (head, rest) = if let Some(i) = expr.find(".memory(") {
            (&expr[i + 8..], "")
        } else {
            let i = expr.find("::new(").map_or(0, |i| i + 6);
            let j = expr[i..].find(')').map_or(i, |j| i + j);
            (&expr[i..j], &expr[j..])
        };
        if !ctors.is_empty() {
            let head = head.split(')').next().unwrap();
            let args: Vec<&str> = head.split(", ").collect();
            assert_eq!(args.len(), ctors.len(), "{expr}");
            for (p, a) in ctors.iter().zip(args) {
                out.push(Param {
                    value: eval_arg(a),
                    ..**p
                });
            }
        }
        // Method calls `.name(arg)` at the top level of the expression.
        let mut calls = Vec::new();
        let (bytes, mut i, mut depth) = (rest.as_bytes(), 0, 0i32);
        while i < bytes.len() {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                b'.' if depth <= 0 => {
                    let open = rest[i..].find('(').map(|o| i + o);
                    if let Some(open) = open {
                        let (mut d, mut j) = (0, open);
                        while j < bytes.len() {
                            match bytes[j] {
                                b'(' => d += 1,
                                b')' => {
                                    d -= 1;
                                    if d == 0 {
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            j += 1;
                        }
                        calls.push((&rest[i + 1..open], &rest[open + 1..j]));
                        i = j;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        for (m, arg) in calls {
            let Some(p) = applied.iter().find(|p| p.method == m) else {
                continue;
            };
            let v = match p.value {
                Arg::Switch(_) => Arg::Switch(true),
                Arg::F32(_) => Arg::F32(arg.parse().unwrap()),
                Arg::F64(_) => Arg::F64(arg.parse().unwrap()),
                Arg::Bytes(_) => match eval_arg(arg) {
                    Arg::Count(n) => Arg::Bytes(n as u64),
                    a => a,
                },
                _ => eval_arg(arg),
            };
            out.retain(|q| q.method != m);
            out.push(Param { value: v, ..*p });
        }
        out.sort_by_key(|p| p.method);
        out
    }

    #[test]
    fn emitted_rust_applies_what_is_measured_for_every_layer() {
        let specs = [
            // Every layer with its defaults (tail-cut and timeout defaults differ from
            // OpenDAL's).
            "url = \"s3://b\"\n[layers]\nconcurrent_limit = { http = 8 }\ntail_cut = true\nthrottle = true\ncache = true\n",
            // Every parameter changed.
            "url = \"s3://b\"\n[layers]\nconcurrent_limit = { http = 24, operations = 64 }\nretry = { max_times = 5, jitter = true, min_delay = \"50ms\", max_delay = \"5s\", factor = 1.5 }\ntimeout = { op = \"30s\", io = \"2500ms\" }\ntail_cut = { percentile = 99, safety_factor = 2.0, min_samples = 10, min_deadline = \"20ms\", max_deadline = \"3s\", window = \"90s\" }\nthrottle = { bandwidth = \"64MiB\", burst = \"3KiB\" }\ncache = { memory = \"1GiB\" }\n",
            // OpenDAL's own values everywhere they can be expressed.
            "url = \"s3://b\"\n[layers]\nconcurrent_limit = { operations = 4 }\ntimeout = { op = \"60s\", io = \"10s\" }\ntail_cut = { percentile = 95, safety_factor = 1.3, min_samples = 200, min_deadline = \"500ms\", max_deadline = \"30s\", window = \"60s\" }\n",
            // Without the timeout layer, the tail-cut ceiling is OpenDAL's 60 s.
            "url = \"s3://b\"\n[layers]\ntimeout = false\ntail_cut = { min_samples = 7 }\n",
        ];
        for text in specs {
            let s = OperatorSpec::parse(text).unwrap();
            for name in LAYER_ORDER {
                let Some(applied) = layer_params(&s, name) else {
                    continue;
                };
                let expr = layer_rust(&s, name).unwrap();
                let mut want: Vec<Param> = applied.clone();
                want.retain(|p| !(p.value == Arg::Switch(false)));
                want.sort_by_key(|p| p.method);
                let mut got = emitted_params(&expr, &applied);
                got.retain(|p| !(p.value == Arg::Switch(false)));
                assert_eq!(got, want, "{name}: measured != emitted: {expr}\n{text}");
            }
        }
        // A given tail-cut ceiling is kept; the default follows the timeouts
        // (`the_tail_cut_ceiling_is_the_timeout_that_bounds_the_call`).
        let s = OperatorSpec::parse(specs[1]).unwrap();
        assert!(
            layer_rust(&s, "tail_cut")
                .unwrap()
                .contains(".max_deadline(Duration::from_secs(3))")
        );
        let mut s = OperatorSpec::parse(specs[0]).unwrap();
        s.set("layers.timeout.op", "7s").unwrap();
        assert!(
            layer_rust(&s, "tail_cut")
                .unwrap()
                .contains(".max_deadline(Duration::from_secs(7))")
        );
    }

    #[test]
    fn secrets_never_show_in_a_diff() {
        let a = OperatorSpec::parse(
            "url = \"s3://b/x\"\n[options]\nsecret_access_key = \"OLDSEKRIT\"\nregion = \"r1\"\n",
        )
        .unwrap();
        let mut b = a.clone();
        b.set("options.secret_access_key", "NEWSEKRIT").unwrap();
        b.set("options.session_token", "TOKSEKRIT").unwrap();
        b.set("url", "s3://user:PWSEKRIT@b/x").unwrap();
        b.set("read.gap", "1MiB").unwrap();
        let d = diff(&a, &b);
        let all = format!(
            "{}\n{}\n{}\n{}",
            serde_json::to_string(&d).unwrap(),
            d.rust,
            d.toml,
            to_rust(&b)
        );
        for s in ["OLDSEKRIT", "NEWSEKRIT", "TOKSEKRIT", "PWSEKRIT"] {
            assert!(!all.contains(s), "{s} in {all}");
        }
        // The secret's change is still shown.
        assert!(
            d.changes
                .iter()
                .any(|c| c.path == "options.secret_access_key"),
            "{:?}",
            d.changes
        );
        assert!(
            d.toml.contains("secret_access_key = \"<redacted>\""),
            "{}",
            d.toml
        );
        // The Rust never carries a placeholder to paste in a credential's place: the diff
        // names the option, the whole operator leaves it to the program.
        assert!(
            d.rust
                .contains("~ options.secret_access_key // a secret, not written here"),
            "{}",
            d.rust
        );
        assert!(
            d.rust.contains("+ options.session_token // a secret"),
            "{}",
            d.rust
        );
        let rust = to_rust(&b);
        assert!(
            !d.rust.contains(REDACTED) && !rust.contains(REDACTED),
            "{}\n{rust}",
            d.rust
        );
        assert!(
            rust.contains("// secret_access_key: a secret, not written here"),
            "{rust}"
        );
        // A `${NAME}` reference is no secret: the TOML keeps it, the Rust reads it.
        b.set("options.secret_access_key", "${MY_SECRET}").unwrap();
        let d = diff(&a, &b);
        assert!(
            d.toml.contains("secret_access_key = \"${MY_SECRET}\""),
            "{}",
            d.toml
        );
        assert!(
            d.rust
                .contains("~ options.secret_access_key = std::env::var(\"MY_SECRET\")?"),
            "{}",
            d.rust
        );
        assert!(
            to_rust(&b)
                .contains("(\"secret_access_key\".to_owned(), std::env::var(\"MY_SECRET\")?),"),
            "{}",
            to_rust(&b)
        );
    }

    #[test]
    fn http_rust_matches_opendal_059_and_alpn_semantics() {
        let a = OperatorSpec::from_url("s3://b/x");
        let mut https_h2 = a.clone();
        https_h2.set("http.version", "h2").unwrap();
        let d = diff(&a, &https_h2);
        assert!(
            !d.rust.contains("http2_prior_knowledge"),
            "HTTPS H2 negotiates via ALPN, not h2c prior knowledge: {}",
            d.rust
        );
        assert!(
            d.rust
                .contains("OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client)))"),
            "{}",
            d.rust
        );
        assert!(!d.rust.contains("HttpClient"), "{}", d.rust);

        let mut h2c = a.clone();
        h2c.set("options.endpoint", "http://127.0.0.1:9000")
            .unwrap();
        h2c.set("http.version", "h2").unwrap();
        let d_h2c = diff(&a, &h2c);
        assert!(
            d_h2c.rust.contains(".http2_prior_knowledge()"),
            "{}",
            d_h2c.rust
        );

        let full = to_rust(&https_h2);
        assert!(
            full.contains("let client = reqwest::Client::builder().build()?;"),
            "{full}"
        );
        assert!(
            full.contains("let op = op.with_context(OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client))));"),
            "{full}"
        );
        assert!(
            full.contains("use opendal::{HttpTransporter, OperationContext, Operator};")
                && full.contains("use opendal_http_transport_reqwest::ReqwestTransport;"),
            "{full}"
        );
    }

    #[test]
    fn to_rust_uses_canonical_schemes_and_via_iter_when_options_or_aliases_exist() {
        let gcs = OperatorSpec::parse(
            "url = \"gs://my-bucket/data\"\n[options]\nservice_account = \"sa@proj.iam\"\n",
        )
        .unwrap();
        let r = to_rust(&gcs);
        assert!(r.contains("Operator::via_iter(\n        \"gcs\","), "{r}");
        let option = |k: &str, v: &str| format!("({k:?}.to_owned(), {v:?}.to_owned()),");
        assert!(r.contains(&option("bucket", "my-bucket")), "{r}");
        assert!(r.contains(&option("root", "/data")), "{r}");
        assert!(r.contains(&option("service_account", "sa@proj.iam")), "{r}");

        let r2 = OperatorSpec::from_url("r2://assets/img");
        let r2_code = to_rust(&r2);
        assert!(
            r2_code.contains("Operator::via_iter(\n        \"s3\","),
            "{r2_code}"
        );
        assert!(r2_code.contains(&option("bucket", "assets")), "{r2_code}");

        let lab = OperatorSpec::from_url("lab://demo");
        let lab_code = to_rust(&lab);
        assert!(
            lab_code.contains("Operator::via_iter(\n        \"s3\","),
            "{lab_code}"
        );
        assert!(
            lab_code.contains(&option("endpoint", "http://127.0.0.1:9000")),
            "{lab_code}"
        );

        // WebDAV and relative FS paths emit `Operator::via_iter`, never broken `from_uri`.
        let dav = OperatorSpec::from_url("webdav://127.0.0.1:8080/files");
        let dav_code = to_rust(&dav);
        assert!(
            dav_code.contains("Operator::via_iter(\n        \"webdav\","),
            "{dav_code}"
        );

        let fs_rel = OperatorSpec::from_url("fs://./local-dir");
        let fs_rel_code = to_rust(&fs_rel);
        assert!(
            fs_rel_code.contains("Operator::via_iter(\n        \"fs\","),
            "{fs_rel_code}"
        );

        let fs_abs = OperatorSpec::from_url("fs:///var/tmp/bench");
        let fs_abs_code = to_rust(&fs_abs);
        assert!(
            fs_abs_code.contains("Operator::from_uri(\"fs:///var/tmp/bench\")"),
            "{fs_abs_code}"
        );

        // Custom HTTP client (`with_context`) is installed BEFORE `reader_with`, and `lab://`
        // with HTTP/2 uses h2c prior knowledge + proxy/CA/insecure options.
        let mut custom = OperatorSpec::from_url("lab://demo");
        custom.set("http.version", "h2").unwrap();
        custom
            .set("http.proxy", "http://u:secret@127.0.0.1:8888")
            .unwrap();
        custom.set("http.insecure", "true").unwrap();
        custom.set("read.concurrent", "4").unwrap();
        let code = to_rust(&custom);
        assert!(code.contains(".http2_prior_knowledge()"), "{code}");
        assert!(
            code.contains(".danger_accept_invalid_certs(true)"),
            "{code}"
        );
        assert!(
            code.contains("reqwest::Proxy::all(\"http://127.0.0.1:8888\")"),
            "{code}"
        );
        assert!(!code.contains("secret"), "{code}");
        assert!(
            code.find("op.with_context(").unwrap() < code.find("op.reader_with(").unwrap(),
            "{code}"
        );
    }

    /// K4-2: the `+`/`~` lines of a diff do not paste; the whole changed operator does, and
    /// travels beside them (`diff.operator_rust`), with no secret in it.
    #[test]
    fn a_diff_carries_the_whole_operator_as_rust() {
        let a = OperatorSpec::from_url("s3://bucket/x");
        let mut b = a.clone();
        b.set("layers.concurrent_limit.http", "24").unwrap();
        b.set("options.secret_access_key", "hunter2hunter2")
            .unwrap();
        let d = diff(&a, &b);
        assert_eq!(d.operator_rust, to_rust(&b.redacted()));
        assert!(
            d.operator_rust.contains("fn build_operator()"),
            "{}",
            d.operator_rust
        );
        assert!(d.operator_rust.contains("with_http_concurrent_limit(24)"));
        assert!(!d.operator_rust.contains("hunter2"), "{}", d.operator_rust);
        assert!(!d.operator_rust.starts_with('+'));
        // An operator with nothing changed still has its Rust.
        assert!(diff(&a, &a).operator_rust.contains("fn build_operator()"));
    }
}
