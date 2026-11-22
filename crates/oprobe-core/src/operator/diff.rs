//! The difference between two operators, as data, as Rust builder code and as TOML: what a
//! proven change looks like to copy into a program or an `op.toml`.

use std::fmt::Write as _;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    OperatorDiff {
        rust: rust_diff(a, b, &changes),
        toml: toml_diff(b, &changes),
        changes,
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
    if ms.is_multiple_of(1000) {
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

/// The builder expression of one layer of `s` (`None`: not in the stack).
pub fn layer_rust(s: &OperatorSpec, name: &str) -> Option<String> {
    let l = &s.layers;
    Some(match name {
        "concurrent_limit" => {
            let c = l.concurrent_limit.as_ref()?;
            let mut e = format!(
                "ConcurrentLimitLayer::new({})",
                c.operations.unwrap_or(UNLIMITED_OPERATIONS)
            );
            if let Some(h) = c.http {
                let _ = write!(e, ".with_http_concurrent_limit({h})");
            }
            e
        }
        "retry" => {
            let r = l.retry.as_ref()?;
            let d = Retry::default();
            let mut e = "RetryLayer::new()".to_owned();
            if r.jitter {
                e += ".with_jitter()";
            }
            if r.max_times != d.max_times {
                let _ = write!(e, ".with_max_times({})", r.max_times);
            }
            if r.min_delay != d.min_delay {
                let _ = write!(e, ".with_min_delay({})", rust_duration(r.min_delay.get()));
            }
            if r.max_delay != d.max_delay {
                let _ = write!(e, ".with_max_delay({})", rust_duration(r.max_delay.get()));
            }
            if r.factor != d.factor {
                let _ = write!(e, ".with_factor({:?})", r.factor);
            }
            e
        }
        "timeout" => {
            let t = l.timeout.as_ref()?;
            let d = Timeout::default();
            let mut e = "TimeoutLayer::new()".to_owned();
            if t.op != d.op {
                let _ = write!(e, ".with_timeout({})", rust_duration(t.op.get()));
            }
            if t.io != d.io {
                let _ = write!(e, ".with_io_timeout({})", rust_duration(t.io.get()));
            }
            e
        }
        "tail_cut" => {
            let t = l.tail_cut.as_ref()?;
            if *t == TailCut::default() {
                "TailCutLayer::new()".to_owned()
            } else {
                format!(
                    "TailCutLayer::builder().percentile({}).build()",
                    t.percentile
                )
            }
        }
        "throttle" => {
            let t = l.throttle.as_ref()?;
            format!(
                "ThrottleLayer::new({}, {})",
                rust_size(t.bandwidth),
                rust_size(t.burst)
            )
        }
        "cache" => {
            let c = l.cache.as_ref()?;
            format!(
                "FoyerLayer::new(HybridCacheBuilder::new().memory({}).storage().build().await?)",
                rust_size(c.memory)
            )
        }
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

fn http_rust(s: &OperatorSpec) -> String {
    let mut e = "reqwest::Client::builder()".to_owned();
    match s.http.version {
        HttpVersion::H2 => e += ".http2_prior_knowledge()",
        HttpVersion::H1 => e += ".http1_only()",
        HttpVersion::Auto => {}
    }
    if !s.http.keep_alive {
        e += ".pool_max_idle_per_host(0)";
    }
    e + " // install with HttpClient::with(...)"
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
                doc.insert("url".into(), toml::Value::String(b.url.clone()));
            }
            "options" | "read" | "write" | "http" => {
                let value = match parts[0] {
                    "options" => b
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
                let v = match name {
                    "concurrent_limit" => l.concurrent_limit.as_ref().map(|x| x.toml()),
                    "retry" => l.retry.as_ref().map(|x| x.toml()),
                    "timeout" => l.timeout.as_ref().map(|x| x.toml()),
                    "tail_cut" => l.tail_cut.as_ref().map(|x| x.toml()),
                    "throttle" => l.throttle.as_ref().map(|x| x.toml()),
                    "cache" => l.cache.as_ref().map(|x| x.toml()),
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

/// The whole operator as Rust: the service from its URL and every layer of the stack.
pub fn to_rust(s: &OperatorSpec) -> String {
    let mut out = format!("let op = Operator::from_uri({:?})?\n", s.display_url());
    for (k, v) in &s.options {
        let _ = writeln!(out, "    // option {k} = {v:?}");
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
    if s.http.version != HttpVersion::Auto || !s.http.keep_alive {
        let _ = writeln!(out, "let client = {};", http_rust(s));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
                .contains("+ ConcurrentLimitLayer::new(1048576).with_http_concurrent_limit(24)"),
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
        assert!(r.contains(".layer(TailCutLayer::new())"), "{r}");
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
}
