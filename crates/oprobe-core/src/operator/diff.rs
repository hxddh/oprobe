//! The difference between two operators, as data, as Rust builder code and as TOML: what a
//! proven change looks like to copy into a program or an `op.toml`.

use std::fmt::Write as _;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::build::{Arg, OpendalDefault, layer_params};
use super::*;

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
        "cache" => format!(
            "FoyerLayer::new(HybridCacheBuilder::new().memory({ctor}).storage().build().await?)"
        ),
        _ => return None,
    })
}

/// The layer names in the order oprobe stacks them (innermost first).
pub const LAYER_ORDER: [&str; 6] = [
    "throttle",
    "concurrent_limit",
    "timeout",
    "tail_cut",
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

fn http_client_expr(s: &OperatorSpec) -> (String, bool) {
    let mut e = "reqwest::Client::builder()".to_owned();
    let h2c = s.url.starts_with("http://")
        || s.options
            .get("endpoint")
            .is_some_and(|u| u.trim().starts_with("http://"));
    match s.http.version {
        HttpVersion::H2 if h2c => e += ".http2_prior_knowledge()",
        HttpVersion::H2 => {}
        HttpVersion::H1 => e += ".http1_only()",
        HttpVersion::Auto => {}
    }
    if !s.http.keep_alive {
        e += ".pool_max_idle_per_host(0)";
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
    for c in changes
        .iter()
        .filter(|c| c.path.starts_with("options.") || c.path == "url")
    {
        let _ = writeln!(
            out,
            "{} {} = {}",
            match c.kind {
                ChangeKind::Added => "+",
                ChangeKind::Changed => "~",
                ChangeKind::Removed => "-",
            },
            c.path,
            c.to.as_deref().or(c.from.as_deref()).unwrap_or_default()
        );
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
    "s3", "gcs", "azblob", "azdls", "azfile", "oss", "cos", "obs", "tos", "b2", "swift", "webdav",
    "webhdfs", "upyun", "http", "fs", "memory",
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

/// The whole operator as Rust: the service from its URL (or canonical scheme and options) and
/// every layer of the stack.
pub fn to_rust(s: &OperatorSpec) -> String {
    let mut out = String::new();
    let need_http = s.http.version != HttpVersion::Auto || !s.http.keep_alive;
    if need_http {
        let _ = writeln!(out, "use opendal::{{HttpTransporter, OperationContext}};");
        let _ = writeln!(
            out,
            "use opendal_http_transport_reqwest::ReqwestTransport;\n"
        );
    }
    let disp_url = s.display_url();
    let url_scheme = disp_url
        .split_once("://")
        .map(|(k, _)| k.to_ascii_lowercase())
        .unwrap_or_default();
    let plain_uri = s.options.is_empty()
        && CANONICAL_URI_SCHEMES.contains(&url_scheme.as_str())
        && !disp_url.contains('?');
    if plain_uri {
        let _ = writeln!(out, "let op = Operator::from_uri({disp_url:?})?");
    } else {
        let (scheme, opts) = resolved_rust_target(s);
        if opts.is_empty() {
            let _ = writeln!(
                out,
                "let op = Operator::via_iter({scheme:?}, std::iter::empty::<(&str, &str)>())?"
            );
        } else {
            let _ = writeln!(out, "let op = Operator::via_iter(\n    {scheme:?},\n    [");
            for (k, v) in &opts {
                let _ = writeln!(out, "        ({k:?}, {v:?}),");
            }
            let _ = writeln!(out, "    ],\n)?");
        }
    }
    for name in LAYER_ORDER {
        if let Some(e) = layer_rust(s, name) {
            let _ = writeln!(out, "    .layer({e})");
        }
    }
    out.push_str(";\n");
    if !s.read.is_default() {
        let _ = writeln!(out, "let reader = {}.await?;", read_rust(s));
    }
    if !s.write.is_default() {
        let _ = writeln!(out, "let writer = {}.await?;", write_rust(s));
    }
    if need_http {
        let (client, alpn_h2) = http_client_expr(s);
        if alpn_h2 {
            let _ = writeln!(out, "// reqwest `http2` feature negotiates ALPN h2");
        }
        let _ = writeln!(out, "let client = {client};");
        let _ = writeln!(
            out,
            "let op = op.with_context(OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client))));"
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
            r.contains(".layer(TailCutLayer::builder().safety_factor(1.5).min_samples(32).min_deadline(Duration::from_millis(50)).max_deadline(Duration::from_secs(60)).build())"),
            "{r}"
        );
        assert!(r.contains(".layer(RetryLayer::new())"), "{r}");
        assert!(
            r.contains("FoyerLayer::new(HybridCacheBuilder::new().memory(256 * 1024 * 1024)"),
            "{r}"
        );
        assert!(r.contains("op.reader_with(path).concurrent(4)"), "{r}");
        // Innermost first: throttle before retry, cache last.
        assert!(r.find("ThrottleLayer").unwrap() < r.find("RetryLayer").unwrap());
        assert!(r.find("RetryLayer").unwrap() < r.find("FoyerLayer").unwrap());
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
        // The tail-cut ceiling follows the operation timeout.
        let s = OperatorSpec::parse(specs[1]).unwrap();
        assert!(
            layer_rust(&s, "tail_cut")
                .unwrap()
                .contains(".max_deadline(Duration::from_secs(3))")
        );
        let mut s = OperatorSpec::parse(specs[0]).unwrap();
        s.set("layers.timeout.op", "12s").unwrap();
        assert!(
            layer_rust(&s, "tail_cut")
                .unwrap()
                .contains(".max_deadline(Duration::from_secs(12))")
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
            full.contains("use opendal::{HttpTransporter, OperationContext};")
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
        assert!(r.contains("Operator::via_iter(\n    \"gcs\","), "{r}");
        assert!(r.contains("(\"bucket\", \"my-bucket\")"), "{r}");
        assert!(r.contains("(\"root\", \"/data\")"), "{r}");
        assert!(r.contains("(\"service_account\", \"sa@proj.iam\")"), "{r}");

        let r2 = OperatorSpec::from_url("r2://assets/img");
        let r2_code = to_rust(&r2);
        assert!(
            r2_code.contains("Operator::via_iter(\n    \"s3\","),
            "{r2_code}"
        );
        assert!(r2_code.contains("(\"bucket\", \"assets\")"), "{r2_code}");

        let lab = OperatorSpec::from_url("lab://demo");
        let lab_code = to_rust(&lab);
        assert!(
            lab_code.contains("Operator::via_iter(\n    \"s3\","),
            "{lab_code}"
        );
        assert!(
            lab_code.contains("(\"endpoint\", \"http://127.0.0.1:9000\")"),
            "{lab_code}"
        );
    }
}
