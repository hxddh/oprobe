//! The operator spec (`op.toml`, `oprobe.operator/6`, design v6 §8): one value that names an
//! OpenDAL service, its layer stack and its read/write and HTTP options.
//!
//! The same value
//! - builds the operator oprobe measures ([`build`]),
//! - is what `--set PATH=VALUE` changes ([`OperatorSpec::set`]),
//! - and renders the difference between two operators as Rust builder code and as TOML
//!   ([`diff`]).
//!
//! ```toml
//! url = "s3://bucket/prefix"
//! [options]
//! region = "eu-west-1"
//! [layers]
//! concurrent_limit = { http = 24 }
//! retry = { max_times = 3, jitter = true, min_delay = "50ms", max_delay = "5s" }
//! timeout = { op = "30s", io = "10s" }
//! tail_cut = { percentile = 95 }
//! throttle = { bandwidth = "100MiB", burst = "16MiB" }
//! cache = { memory = "256MiB" }
//! [read]
//! chunk = "8MiB"
//! concurrent = 4
//! gap = "64KiB"
//! [write]
//! chunk = "8MiB"
//! concurrent = 4
//! [http]
//! version = "auto"
//! keep_alive = true
//! ```
//!
//! **Layers.** The defaults are OpenDAL's, plus `retry` and `timeout` (OpenDAL's own
//! recommendation): a key that is absent from `[layers]` keeps its default (on for `retry` and
//! `timeout`, off for the others), `false` (or `--set layers.NAME=off`) takes the layer out of
//! the stack, `true` puts it in with its default parameters.

pub mod build;
pub mod diff;
pub mod resolve;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::errors::{ErrorCode, OprobeError};
use crate::net::HttpVersion;
use crate::util::{ByteSize, HumanDuration};

pub use diff::{Change, ChangeKind, OperatorDiff};

/// Format id of an operator spec.
pub const OPERATOR_FORMAT: &str = "oprobe.operator/6";

/// One OpenDAL operator: service, layers, read/write options, HTTP client settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperatorSpec {
    /// The service: any OpenDAL scheme oprobe is built with (`s3://bucket/prefix`,
    /// `gcs://bucket`, `fs:///tmp/x`, `memory://`), or `lab://SCENARIO` for the local lab.
    pub url: String,
    /// OpenDAL service options (checked against the service). Values may reference
    /// environment variables as `${NAME}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    /// The layer stack.
    #[serde(default, skip_serializing_if = "Layers::is_default")]
    pub layers: Layers,
    /// `read_with(path)` / `reader_with(path)` options.
    #[serde(default, skip_serializing_if = "ReadOptions::is_default")]
    pub read: ReadOptions,
    /// `writer_with(path)` options.
    #[serde(default, skip_serializing_if = "WriteOptions::is_default")]
    pub write: WriteOptions,
    /// The `HttpFetch` client's settings.
    #[serde(default, skip_serializing_if = "HttpOptions::is_default")]
    pub http: HttpOptions,
}

/// The layer stack: each key is one OpenDAL layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Layers {
    /// `ConcurrentLimitLayer` (off by default).
    #[serde(
        default,
        deserialize_with = "layer::de_off",
        serialize_with = "layer::ser_off",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<ConcurrentLimit>")]
    pub concurrent_limit: Option<ConcurrentLimit>,
    /// `RetryLayer` (on by default).
    #[serde(
        default = "layer::some_default",
        deserialize_with = "layer::de_on",
        serialize_with = "layer::ser_on",
        skip_serializing_if = "layer::is_some_default"
    )]
    #[schemars(with = "Option<Retry>")]
    pub retry: Option<Retry>,
    /// `TimeoutLayer` (on by default).
    #[serde(
        default = "layer::some_default",
        deserialize_with = "layer::de_on",
        serialize_with = "layer::ser_on",
        skip_serializing_if = "layer::is_some_default"
    )]
    #[schemars(with = "Option<Timeout>")]
    pub timeout: Option<Timeout>,
    /// `TailCutLayer` (off by default).
    #[serde(
        default,
        deserialize_with = "layer::de_off",
        serialize_with = "layer::ser_off",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<TailCut>")]
    pub tail_cut: Option<TailCut>,
    /// `ThrottleLayer` (off by default).
    #[serde(
        default,
        deserialize_with = "layer::de_off",
        serialize_with = "layer::ser_off",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<Throttle>")]
    pub throttle: Option<Throttle>,
    /// `FoyerLayer` with an in-memory cache (off by default).
    #[serde(
        default,
        deserialize_with = "layer::de_off",
        serialize_with = "layer::ser_off",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<Cache>")]
    pub cache: Option<Cache>,
}

impl Default for Layers {
    fn default() -> Self {
        Self {
            concurrent_limit: None,
            retry: Some(Retry::default()),
            timeout: Some(Timeout::default()),
            tail_cut: None,
            throttle: None,
            cache: None,
        }
    }
}

impl Layers {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `ConcurrentLimitLayer`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConcurrentLimit {
    /// HTTP requests in flight (`with_http_concurrent_limit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<usize>,
    /// Operations in flight (`ConcurrentLimitLayer::new`; unset: unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operations: Option<usize>,
}

/// `RetryLayer` (OpenDAL's defaults: 3 times, 1 s to 60 s, factor 2, no jitter).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Retry {
    pub max_times: usize,
    pub jitter: bool,
    pub min_delay: HumanDuration,
    pub max_delay: HumanDuration,
    pub factor: f32,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            max_times: 3,
            jitter: false,
            min_delay: HumanDuration::secs(1),
            max_delay: HumanDuration::secs(60),
            factor: 2.0,
        }
    }
}

/// `TimeoutLayer` (OpenDAL's defaults: 60 s per operation, 10 s per I/O).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Timeout {
    /// `with_timeout`: one operation.
    pub op: HumanDuration,
    /// `with_io_timeout`: one read or write call.
    pub io: HumanDuration,
}

impl Default for Timeout {
    fn default() -> Self {
        Self {
            op: HumanDuration::secs(60),
            io: HumanDuration::secs(10),
        }
    }
}

/// `TailCutLayer`: cancel (and, through `RetryLayer`, re-issue) a request slower than the
/// `percentile` of the recent latencies × `safety_factor`, the deadline clamped to
/// `[min_deadline, max_deadline]`.
///
/// The defaults are what oprobe measures with, not OpenDAL's (1.3, 200 samples, 500 ms, 30 s):
/// a shorter warm-up and floor so that short runs can see the layer work, and a ceiling at the
/// operation timeout. Generated Rust spells out every value that differs from OpenDAL's default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct TailCut {
    /// The latency percentile the deadline follows (50..99).
    pub percentile: u8,
    /// The deadline = percentile × this factor (1..5).
    pub safety_factor: f64,
    /// Latencies recorded before the layer cuts anything.
    pub min_samples: usize,
    /// The shortest deadline.
    pub min_deadline: HumanDuration,
    /// The longest deadline (unset: the operation timeout, `layers.timeout.op`, or 60 s without
    /// the timeout layer).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_deadline: Option<HumanDuration>,
    /// The sliding window of latencies (at most 120 s).
    pub window: HumanDuration,
}

impl Default for TailCut {
    fn default() -> Self {
        Self {
            percentile: 95,
            safety_factor: 1.5,
            min_samples: 32,
            min_deadline: HumanDuration(Duration::from_millis(50)),
            max_deadline: None,
            window: HumanDuration::secs(60),
        }
    }
}

impl TailCut {
    /// The parameters applied with an operation timeout of `op_timeout`.
    pub fn params(&self, op_timeout: Duration) -> crate::engine::stack::TailCutParams {
        crate::engine::stack::TailCutParams {
            percentile: self.percentile,
            safety_factor: self.safety_factor,
            min_samples: self.min_samples,
            min_deadline: self.min_deadline.get(),
            max_deadline: self.max_deadline.map_or(op_timeout, HumanDuration::get),
            window: self.window.get(),
        }
    }
}

/// `ThrottleLayer` (bytes per second, and the largest burst).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Throttle {
    pub bandwidth: ByteSize,
    pub burst: ByteSize,
}

impl Default for Throttle {
    fn default() -> Self {
        Self {
            bandwidth: ByteSize::mib(100),
            burst: ByteSize::mib(16),
        }
    }
}

/// `FoyerLayer` over an in-memory foyer cache.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Cache {
    pub memory: ByteSize,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            memory: ByteSize::mib(256),
        }
    }
}

/// `read_with` / `reader_with` options (unset: OpenDAL's default).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<ByteSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrent: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<ByteSize>,
}

impl ReadOptions {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `writer_with` options (unset: OpenDAL's default).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<ByteSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrent: Option<usize>,
}

impl WriteOptions {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The HTTP client (OpenDAL's reqwest `HttpFetch`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct HttpOptions {
    /// `auto` (OpenDAL's default: HTTP/1.1), `h1` or `h2`.
    pub version: HttpVersion,
    /// Reuse connections.
    pub keep_alive: bool,
    /// Egress proxy URL; `none` ignores HTTPS_PROXY/HTTP_PROXY (unset: the environment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// An extra PEM CA bundle to trust.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
    /// Skip TLS certificate verification (self-signed development endpoints only).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            version: HttpVersion::Auto,
            keep_alive: true,
            proxy: None,
            ca_cert: None,
            insecure: false,
        }
    }
}

impl HttpOptions {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// (De)serialization of a layer key: absent → its default, `false` → off, `true` → on with
/// default parameters, a table → on with those parameters.
mod layer {
    use super::*;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw<T> {
        Flag(bool),
        Params(T),
    }

    fn de<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        Ok(match Raw::<T>::deserialize(d)? {
            Raw::Flag(false) => None,
            Raw::Flag(true) => Some(T::default()),
            Raw::Params(t) => Some(t),
        })
    }

    pub fn de_on<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        de(d)
    }

    pub fn de_off<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        de(d)
    }

    /// A layer that is on by default: `false` when off.
    pub fn ser_on<S: Serializer, T: Serialize>(v: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(t) => t.serialize(s),
            None => s.serialize_bool(false),
        }
    }

    /// A layer that is off by default (only serialized when on).
    pub fn ser_off<S: Serializer, T: Serialize>(v: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(t) => t.serialize(s),
            None => s.serialize_bool(false),
        }
    }

    pub fn some_default<T: Default>() -> Option<T> {
        Some(T::default())
    }

    pub fn is_some_default<T: Default + PartialEq>(v: &Option<T>) -> bool {
        v.as_ref().is_some_and(|t| *t == T::default())
    }
}

fn invalid(msg: impl Into<String>) -> OprobeError {
    OprobeError::new(ErrorCode::UsageInvalidArgument, msg)
}

/// The paths `--set` accepts (`options.KEY` takes any key of the service).
pub const SET_PATHS: &[&str] = &[
    "url",
    "options.KEY",
    "layers.concurrent_limit",
    "layers.concurrent_limit.http",
    "layers.concurrent_limit.operations",
    "layers.retry",
    "layers.retry.max_times",
    "layers.retry.jitter",
    "layers.retry.min_delay",
    "layers.retry.max_delay",
    "layers.retry.factor",
    "layers.timeout",
    "layers.timeout.op",
    "layers.timeout.io",
    "layers.tail_cut",
    "layers.tail_cut.percentile",
    "layers.tail_cut.safety_factor",
    "layers.tail_cut.min_samples",
    "layers.tail_cut.min_deadline",
    "layers.tail_cut.max_deadline",
    "layers.tail_cut.window",
    "layers.throttle",
    "layers.throttle.bandwidth",
    "layers.throttle.burst",
    "layers.cache",
    "layers.cache.memory",
    "read.chunk",
    "read.concurrent",
    "read.gap",
    "write.chunk",
    "write.concurrent",
    "http.version",
    "http.keep_alive",
    "http.proxy",
    "http.ca_cert",
    "http.insecure",
];

impl OperatorSpec {
    /// The default operator of a service URL.
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            options: BTreeMap::new(),
            layers: Layers::default(),
            read: ReadOptions::default(),
            write: WriteOptions::default(),
            http: HttpOptions::default(),
        }
    }

    /// Parse an `op.toml` document (validated).
    pub fn parse(toml_text: &str) -> Result<Self, OprobeError> {
        let s: Self = toml::from_str(toml_text).map_err(|e| {
            OprobeError::new(ErrorCode::ConfigInvalid, format!("invalid operator: {e}"))
        })?;
        s.validate()?;
        Ok(s)
    }

    /// Load an operator file (`op.toml`).
    pub fn load(path: &Path) -> Result<Self, OprobeError> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            invalid(format!(
                "cannot read operator file `{}`: {e}",
                path.display()
            ))
        })?;
        let mut s = Self::parse(&raw).map_err(|e| {
            let mut o = OprobeError::new(e.code, format!("`{}`: {}", path.display(), e.message));
            o.hint = e.hint;
            o
        })?;
        if let Some(ca) = &s.http.ca_cert
            && ca.is_relative()
        {
            let dir = path
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let base = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            s.http.ca_cert = Some(base.join(ca));
        }
        Ok(s)
    }

    /// This operator with `http.ca_cert` as the operator file `operator_ref` gives it: a
    /// relative path stays relative (what a result stores and shows), where [`Self::load`]
    /// made it absolute for this machine. Unchanged for a URL, a configured name, or a path
    /// that is not the file's.
    pub fn as_given(&self, operator_ref: &str) -> Self {
        let mut s = self.clone();
        let Some(path) = resolve::operator_file_path(operator_ref) else {
            return s;
        };
        let given = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| toml::from_str::<Self>(&raw).ok())
            .and_then(|o| o.http.ca_cert);
        if let (Some(given), Some(abs)) = (given, &self.http.ca_cert)
            && given.is_relative()
            && abs.ends_with(&given)
        {
            s.http.ca_cert = Some(given);
        }
        s
    }

    /// The `op.toml` form (defaults left out).
    pub fn to_toml(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }

    /// Check every value (the service options are checked when the operator is built).
    pub fn validate(&self) -> Result<(), OprobeError> {
        if self.url.trim().is_empty() {
            return Err(invalid("the operator has no `url`"));
        }
        let l = &self.layers;
        if let Some(c) = &l.concurrent_limit {
            if c.http.is_none() && c.operations.is_none() {
                return Err(invalid(
                    "layers.concurrent_limit needs `http` or `operations` (e.g. { http = 24 })",
                ));
            }
            if c.http == Some(0) || c.operations == Some(0) {
                return Err(invalid("layers.concurrent_limit limits must be at least 1"));
            }
        }
        if let Some(r) = &l.retry {
            if !(r.factor.is_finite() && r.factor >= 1.0) {
                return Err(invalid("layers.retry.factor must be >= 1"));
            }
            if r.min_delay > r.max_delay {
                return Err(invalid(
                    "layers.retry.min_delay must not exceed layers.retry.max_delay",
                ));
            }
        }
        if let Some(t) = &l.timeout
            && (t.op.get().is_zero() || t.io.get().is_zero())
        {
            return Err(invalid("layers.timeout durations must be > 0"));
        }
        if let Some(t) = &l.tail_cut {
            // OpenDAL's builder panics outside these ranges.
            if !(50..=99).contains(&t.percentile) {
                return Err(invalid("layers.tail_cut.percentile must be within 50..99"));
            }
            if !(t.safety_factor.is_finite() && (1.0..=5.0).contains(&t.safety_factor)) {
                return Err(invalid("layers.tail_cut.safety_factor must be within 1..5"));
            }
            if t.window.get().is_zero() || t.window.get() > Duration::from_secs(120) {
                return Err(invalid("layers.tail_cut.window must be within 1ms..120s"));
            }
            if t.max_deadline
                .is_some_and(|m| m.get().is_zero() || m.get() < t.min_deadline.get())
            {
                return Err(invalid(
                    "layers.tail_cut.max_deadline must be > 0 and not below min_deadline",
                ));
            }
        }
        if let Some(t) = &l.throttle {
            let ok = |b: ByteSize| b.0 > 0 && b.0 <= u64::from(u32::MAX);
            if !ok(t.bandwidth) || !ok(t.burst) {
                return Err(invalid(
                    "layers.throttle bandwidth and burst must be within 1B..4GiB",
                ));
            }
        }
        if let Some(c) = &l.cache
            && c.memory.0 == 0
        {
            return Err(invalid("layers.cache.memory must be > 0"));
        }
        for (what, v) in [
            ("read.concurrent", self.read.concurrent),
            ("write.concurrent", self.write.concurrent),
        ] {
            if v == Some(0) {
                return Err(invalid(format!("{what} must be at least 1")));
            }
        }
        for (what, v) in [
            ("read.chunk", self.read.chunk),
            ("write.chunk", self.write.chunk),
        ] {
            if v.is_some_and(|c| c.0 < 4096) {
                return Err(invalid(format!("{what} must be at least 4KiB")));
            }
        }
        Ok(())
    }

    /// Change one value: `layers.retry.max_times=5`, `read.gap=1MiB`, `layers.cache=off`,
    /// `options.region=eu-west-1`; `none` removes an optional value. Validated.
    pub fn set(&mut self, path: &str, value: &str) -> Result<(), OprobeError> {
        self.apply_edits(&[(path.to_owned(), value.to_owned())])
    }

    /// Apply a set of `PATH=VALUE` edits (`--set`, `prove --with`, the app's edits) as one
    /// change, validated once at the end: the order they are given in does not matter
    /// ([`normalize_edits`]), so `layers.concurrent_limit=on` with
    /// `layers.concurrent_limit.http=24` works either way round.
    pub fn apply_edits(&mut self, edits: &[(String, String)]) -> Result<(), OprobeError> {
        let edits = normalize_edits(edits);
        let mut next = self.clone();
        for (k, v) in &edits {
            next.set_unchecked(k, v)?;
        }
        next.validate().map_err(|e| {
            let given: Vec<String> = edits.iter().map(|(k, v)| redacted_edit(k, v)).collect();
            let mut o = OprobeError::new(
                e.code,
                format!("--set {}: {}", given.join(" --set "), e.message),
            );
            o.hint = e.hint;
            o
        })?;
        *self = next;
        Ok(())
    }

    /// Check one edit on its own (its path, its value's type and range), before it is applied
    /// with others: a layer switched on is not required to be complete yet, since the field it
    /// needs (`layers.concurrent_limit` needs `http`) may come in the same set.
    pub fn check_edit(path: &str, value: &str) -> Result<(), OprobeError> {
        let mut scratch = Self::from_url("memory://");
        let toggle_on = path
            .trim()
            .strip_prefix("layers.")
            .is_some_and(|n| !n.contains('.'))
            && !is_layer_off(value);
        if toggle_on {
            scratch.set_unchecked(path, value)
        } else {
            scratch.set(path, value)
        }
    }

    /// One edit without validating the result (a step of [`Self::apply_edits`]).
    fn set_unchecked(&mut self, path: &str, value: &str) -> Result<(), OprobeError> {
        let path = path.trim();
        let value = value.trim();
        let mut doc = toml::Value::try_from(&*self)
            .map_err(|e| OprobeError::new(ErrorCode::Internal, e.to_string()))?;
        let parts: Vec<&str> = path.split('.').collect();
        let known = parts[0] == "options" && parts.len() == 2 && !parts[1].is_empty()
            || SET_PATHS.contains(&path);
        if !known {
            let hint = match crate::input::suggest(path, SET_PATHS.iter().copied()) {
                Some(s) => format!("did you mean `{s}`?"),
                None => "paths: url, options.KEY, layers.NAME[.FIELD], read.*, write.*, http.*"
                    .to_owned(),
            };
            return Err(invalid(format!("unknown operator path `{path}`")).with_hint(hint));
        }
        let layer_toggle = parts.len() == 2 && parts[0] == "layers";
        let parsed = if parts[0] == "options" || path == "url" {
            Some(toml::Value::String(value.to_owned()))
        } else if path == "http.proxy" {
            // `none` is a value here (no proxy, whatever HTTPS_PROXY says); `unset` or
            // `default` go back to the environment's proxy.
            match value.to_ascii_lowercase().as_str() {
                "unset" | "default" | "" => None,
                "none" | "off" => Some(toml::Value::String("none".to_owned())),
                _ => Some(toml::Value::String(value.to_owned())),
            }
        } else if path == "http.ca_cert" {
            match value.to_ascii_lowercase().as_str() {
                "none" | "unset" | "default" | "" => None,
                _ => Some(toml::Value::String(value.to_owned())),
            }
        } else {
            scalar(value, layer_toggle)
        };
        // Walk to the parent table, creating tables (a layer that was off or default).
        let root = doc.as_table_mut().expect("a spec is a table");
        let mut table = root;
        for key in &parts[..parts.len() - 1] {
            let entry = table
                .entry((*key).to_owned())
                .or_insert_with(|| toml::Value::Table(Default::default()));
            if !entry.is_table() {
                // `layers.cache = false` (or the default `true`) before setting a field.
                *entry = toml::Value::Table(Default::default());
            }
            table = entry.as_table_mut().expect("table");
        }
        let last = parts[parts.len() - 1].to_owned();
        // A layer switched on that is already on keeps its parameters.
        let keep = layer_toggle
            && parsed == Some(toml::Value::Boolean(true))
            && table.get(&last).is_some_and(toml::Value::is_table);
        match parsed {
            _ if keep => {}
            Some(v) => {
                table.insert(last, v);
            }
            None => {
                table.remove(&last);
            }
        }
        let next: Self = doc.try_into().map_err(|e: toml::de::Error| {
            let why = e.message();
            let why = if why.contains("untagged enum") {
                "not a value of this field's type"
            } else {
                why
            };
            invalid(format!("invalid value `{value}` for {path}: {why}")).with_hint(
                "counts and percentiles are numbers, sizes like 8MiB, durations like 50ms, \
                 switches on/off; a layer itself is on or off",
            )
        })?;
        *self = next;
        Ok(())
    }

    /// The service scheme of the URL (`s3`, `lab`, …).
    pub fn scheme(&self) -> String {
        self.url
            .split_once("://")
            .map(|(s, _)| s.to_ascii_lowercase())
            .unwrap_or_default()
    }

    /// The URL without secrets.
    pub fn display_url(&self) -> String {
        crate::util::redact_url(&self.url)
    }

    /// A copy whose secret options, URL credentials, proxy credentials and `ca_cert_pem` are
    /// redacted while preserving `${ENV}` references (stored in reports, proves and run files).
    pub fn redacted(&self) -> Self {
        crate::model::spec::redact_operator(self)
    }

    /// `read.concurrent` as OpenDAL applies it (1 when unset).
    pub fn read_concurrent(&self) -> usize {
        self.read.concurrent.unwrap_or(1).max(1)
    }

    /// `write.concurrent` as OpenDAL applies it (1 when unset).
    pub fn write_concurrent(&self) -> usize {
        self.write.concurrent.unwrap_or(1).max(1)
    }

    /// The operation timeout (`layers.timeout.op`; OpenDAL's 60 s default without the layer).
    pub fn op_timeout(&self) -> Duration {
        self.layers
            .timeout
            .as_ref()
            .map_or(Duration::from_secs(60), |t| t.op.get())
    }

    /// Retries of a failed request (0 without the retry layer).
    pub fn retries(&self) -> usize {
        self.layers.retry.as_ref().map_or(0, |r| r.max_times)
    }

    /// The HTTP in-flight cap (`layers.concurrent_limit.http`).
    pub fn http_limit(&self) -> Option<usize> {
        self.layers.concurrent_limit.as_ref().and_then(|c| c.http)
    }
}

/// Whether a layer switch value takes the layer out (`off`, `false`, `no`, `none`).
pub fn is_layer_off(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "off" | "false" | "no" | "none"
    )
}

/// The layer switch of a layer field (`layers.retry.jitter` → `layers.retry`).
pub fn layer_switch(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("layers.")?;
    let (name, field) = rest.split_once('.')?;
    (!field.is_empty()).then(|| &path[..7 + name.len()])
}

/// A set of `PATH=VALUE` edits in the order they apply, the same for every front end:
/// - the last value given for a path wins;
/// - the fields of a layer switched off in the same set are dropped (the layer is out);
/// - layer switches (`layers.NAME=on|off`) and every other path apply before layer fields, so
///   a field lands in a layer switched on in the same set; otherwise the given order is kept.
pub fn normalize_edits(edits: &[(String, String)]) -> Vec<(String, String)> {
    let mut last: Vec<(String, String)> = Vec::new();
    for (k, v) in edits {
        let (k, v) = (k.trim(), v.trim());
        last.retain(|(x, _)| x != k);
        last.push((k.to_owned(), v.to_owned()));
    }
    let off = |layer: &str| last.iter().any(|(k, v)| k == layer && is_layer_off(v));
    let mut out: Vec<(String, String)> = last
        .iter()
        .filter(|(k, _)| layer_switch(k).is_none_or(|l| !off(l)))
        .cloned()
        .collect();
    out.sort_by_key(|(k, _)| layer_switch(k).is_some());
    out
}

/// `PATH=VALUE` for display: the value of a secret option (`options.secret_access_key`)
/// redacted, a URL without credentials.
pub fn redacted_edit(path: &str, value: &str) -> String {
    match path.strip_prefix("options.") {
        Some(k) if crate::util::is_secret_key(k) => format!("{path}=<redacted>"),
        Some(_) => format!("{path}={}", crate::util::strip_url_credentials(value)),
        None if path == "url" || path == "http.proxy" => {
            format!("{path}={}", crate::util::redact_url(value))
        }
        None => format!("{path}={value}"),
    }
}

/// A `--set` value as TOML: booleans (`on`/`off` too), integers, floats, else a string;
/// `None` for `none`/`unset` (remove). For a layer key, `off`/`on` are `false`/`true`.
fn scalar(v: &str, layer_toggle: bool) -> Option<toml::Value> {
    let l = v.to_ascii_lowercase();
    match l.as_str() {
        "none" | "unset" | "default" if !layer_toggle => return None,
        "true" | "on" | "yes" => return Some(toml::Value::Boolean(true)),
        "false" | "off" | "no" | "none" => return Some(toml::Value::Boolean(false)),
        _ => {}
    }
    if let Ok(i) = v.parse::<i64>() {
        return Some(toml::Value::Integer(i));
    }
    if let Ok(f) = v.parse::<f64>()
        && f.is_finite()
    {
        return Some(toml::Value::Float(f));
    }
    Some(toml::Value::String(v.to_owned()))
}

fn to_edit_value(v: toml::Value) -> toml_edit::Value {
    match v {
        toml::Value::Boolean(b) => toml_edit::Value::from(b),
        toml::Value::Integer(i) => toml_edit::Value::from(i),
        toml::Value::Float(f) => toml_edit::Value::from(f),
        toml::Value::String(s) => toml_edit::Value::from(s),
        other => toml_edit::Value::from(other.to_string()),
    }
}

/// Apply `PATH=VALUE` `edits` in place to a [`toml_edit::Table`] representing an operator
/// (`op.toml` root or `[operators.NAME]` in `config.toml`), preserving comments, whitespace,
/// key ordering and unedited fields.
pub fn patch_toml_table(
    root: &mut toml_edit::Table,
    edits: &[(String, String)],
) -> Result<(), OprobeError> {
    let normalized = normalize_edits(edits);
    for (path, value) in &normalized {
        OperatorSpec::check_edit(path, value)?;
        let parts: Vec<&str> = path.split('.').collect();
        let layer_toggle = parts.len() == 2 && parts[0] == "layers";
        let parsed: Option<toml_edit::Value> = if parts[0] == "options" || path == "url" {
            Some(toml_edit::Value::from(value.as_str()))
        } else if path == "http.proxy" {
            match value.to_ascii_lowercase().as_str() {
                "unset" | "default" | "" => None,
                "none" | "off" => Some(toml_edit::Value::from("none")),
                _ => Some(toml_edit::Value::from(value.as_str())),
            }
        } else if path == "http.ca_cert" {
            match value.to_ascii_lowercase().as_str() {
                "none" | "unset" | "default" | "" => None,
                _ => Some(toml_edit::Value::from(value.as_str())),
            }
        } else {
            scalar(value, layer_toggle).map(to_edit_value)
        };

        if parts.len() == 1 {
            match parsed {
                Some(v) => {
                    root["url"] = toml_edit::Item::Value(v);
                }
                None => {
                    root.remove("url");
                }
            }
            continue;
        }

        let sec_key = parts[0];
        if !root.contains_key(sec_key)
            || (!root[sec_key].is_table() && !root[sec_key].is_inline_table())
        {
            if parsed.is_none() {
                continue;
            }
            root.insert(sec_key, toml_edit::Item::Table(toml_edit::Table::new()));
        }

        if parts.len() == 2 {
            let last = parts[1];
            let keep = layer_toggle
                && parsed.as_ref().and_then(|v| v.as_bool()) == Some(true)
                && match &root[sec_key] {
                    toml_edit::Item::Table(t) => t
                        .get(last)
                        .is_some_and(|i| i.is_table() || i.is_inline_table()),
                    toml_edit::Item::Value(toml_edit::Value::InlineTable(it)) => {
                        it.get(last).is_some_and(|v| v.as_inline_table().is_some())
                    }
                    _ => false,
                };
            if keep {
                continue;
            }
            match (&mut root[sec_key], parsed) {
                (toml_edit::Item::Table(t), Some(v)) => {
                    t[last] = toml_edit::Item::Value(v);
                }
                (toml_edit::Item::Table(t), None) => {
                    t.remove(last);
                }
                (toml_edit::Item::Value(toml_edit::Value::InlineTable(it)), Some(v)) => {
                    it.insert(last, v);
                    it.fmt();
                }
                (toml_edit::Item::Value(toml_edit::Value::InlineTable(it)), None) => {
                    it.remove(last);
                    it.fmt();
                }
                _ => {}
            }
            continue;
        }

        // `parts.len() == 3`: `layers.<layer>.<field>`
        let (layer_name, field) = (parts[1], parts[2]);
        if let toml_edit::Item::Value(toml_edit::Value::InlineTable(it)) = &root[sec_key] {
            // Promote inline `[layers]` if needed so layer sub-entries edit cleanly.
            let mut tbl = it.clone().into_table();
            tbl.set_implicit(false);
            root[sec_key] = toml_edit::Item::Table(tbl);
        }
        let Some(layers_tbl) = root[sec_key].as_table_mut() else {
            continue;
        };
        match layers_tbl.get_mut(layer_name) {
            Some(toml_edit::Item::Table(t)) => match parsed {
                Some(v) => {
                    t[field] = toml_edit::Item::Value(v);
                }
                None => {
                    t.remove(field);
                }
            },
            Some(toml_edit::Item::Value(toml_edit::Value::InlineTable(it))) => match parsed {
                Some(v) => {
                    it.insert(field, v);
                    it.fmt();
                }
                None => {
                    it.remove(field);
                    it.fmt();
                }
            },
            _ => {
                if let Some(v) = parsed {
                    let mut it = toml_edit::InlineTable::new();
                    it.insert(field, v);
                    it.fmt();
                    layers_tbl[layer_name] =
                        toml_edit::Item::Value(toml_edit::Value::InlineTable(it));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
url = "s3://bucket/prefix"
[options]
region = "eu-west-1"
[layers]
concurrent_limit = { http = 24 }
retry = { max_times = 3, jitter = true, min_delay = "50ms", max_delay = "5s" }
timeout = { op = "30s", io = "10s" }
tail_cut = { percentile = 95 }
throttle = { bandwidth = "100MiB", burst = "16MiB" }
cache = { memory = "256MiB" }
[read]
chunk = "8MiB"
concurrent = 4
gap = "64KiB"
[write]
chunk = "8MiB"
concurrent = 4
[http]
version = "auto"
keep_alive = true
"#;

    #[test]
    fn the_contract_example_parses_and_round_trips() {
        let s = OperatorSpec::parse(FULL).unwrap();
        assert_eq!(s.options["region"], "eu-west-1");
        assert_eq!(s.http_limit(), Some(24));
        let r = s.layers.retry.as_ref().unwrap();
        assert!(r.jitter);
        assert_eq!(r.min_delay.get(), Duration::from_millis(50));
        assert_eq!(
            s.layers.timeout.as_ref().unwrap().op.get(),
            Duration::from_secs(30)
        );
        assert_eq!(s.layers.tail_cut.as_ref().unwrap().percentile, 95);
        assert_eq!(s.layers.cache.as_ref().unwrap().memory, ByteSize::mib(256));
        assert_eq!(s.read.gap, Some(ByteSize::kib(64)));
        assert_eq!(s.write_concurrent(), 4);
        let back = OperatorSpec::parse(&s.to_toml()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn defaults_are_opendal_plus_retry_and_timeout() {
        let s = OperatorSpec::parse(r#"url = "memory://""#).unwrap();
        assert_eq!(s, OperatorSpec::from_url("memory://"));
        assert_eq!(s.retries(), 3);
        assert!(s.layers.timeout.is_some());
        assert!(s.layers.cache.is_none() && s.layers.tail_cut.is_none());
        assert_eq!(s.read_concurrent(), 1);
        // Defaults are not written back.
        assert_eq!(s.to_toml().trim(), r#"url = "memory://""#);
        // Default-on layers are written as `false` when off.
        let mut off = s.clone();
        off.layers.retry = None;
        assert!(off.to_toml().contains("retry = false"), "{}", off.to_toml());
        assert_eq!(OperatorSpec::parse(&off.to_toml()).unwrap(), off);
        let on =
            OperatorSpec::parse("url = \"memory://\"\n[layers]\ntail_cut = true\nretry = false\n")
                .unwrap();
        assert_eq!(on.layers.tail_cut, Some(TailCut::default()));
        assert!(on.layers.retry.is_none());
    }

    #[test]
    fn set_changes_one_value() {
        let mut s = OperatorSpec::from_url("s3://b/x");
        s.set("layers.retry.max_times", "5").unwrap();
        assert_eq!(s.retries(), 5);
        assert_eq!(
            s.layers.retry.as_ref().unwrap().max_delay,
            HumanDuration::secs(60)
        );
        s.set("read.gap", "1MiB").unwrap();
        assert_eq!(s.read.gap, Some(ByteSize::mib(1)));
        s.set("layers.cache.memory", "1GiB").unwrap();
        assert_eq!(s.layers.cache.as_ref().unwrap().memory.0, 1 << 30);
        s.set("layers.cache", "off").unwrap();
        assert!(s.layers.cache.is_none());
        s.set("layers.tail_cut", "on").unwrap();
        assert!(s.layers.tail_cut.is_some());
        s.set("layers.concurrent_limit.http", "24").unwrap();
        assert_eq!(s.http_limit(), Some(24));
        s.set("layers.retry.jitter", "true").unwrap();
        s.set("http.version", "h2").unwrap();
        assert_eq!(s.http.version, HttpVersion::H2);
        s.set("options.region", "auto").unwrap();
        assert_eq!(s.options["region"], "auto");
        s.set("read.gap", "none").unwrap();
        assert!(s.read.gap.is_none());
        s.set("layers.retry", "off").unwrap();
        assert_eq!(s.retries(), 0);
        // `http.proxy=none` means no proxy (not the environment's); `unset` restores it.
        s.set("http.proxy", "none").unwrap();
        assert_eq!(s.http.proxy.as_deref(), Some("none"));
        assert_eq!(
            crate::engine::operator::proxy_setting(&s.target_spec()),
            Some(None),
            "the transport bypasses HTTPS_PROXY"
        );
        assert!(s.to_toml().contains("proxy = \"none\""), "{}", s.to_toml());
        s.set("http.proxy", "http://p:3128").unwrap();
        assert_eq!(s.http.proxy.as_deref(), Some("http://p:3128"));
        s.set("http.proxy", "unset").unwrap();
        assert_eq!(s.http.proxy, None);
        assert_eq!(
            crate::engine::operator::proxy_setting(&s.target_spec()),
            None
        );
        for (p, v) in [
            ("layers.retry.max_tims", "5"),
            ("read.concurrent", "0"),
            ("read.chunk", "1"),
            ("layers.retry.max_times", "lots"),
            ("layers.tail_cut.percentile", "100"),
            ("http.version", "h3"),
        ] {
            let e = s.clone().set(p, v).unwrap_err();
            assert_eq!(e.code, ErrorCode::UsageInvalidArgument, "{p}={v}: {e:?}");
        }
        let e = s.set("layers.retry.max_tims", "5").unwrap_err();
        assert!(e.hint.unwrap().contains("layers.retry.max_times"));
    }

    #[test]
    fn edits_apply_in_any_order() {
        let e = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let want = {
            let mut s = OperatorSpec::from_url("s3://b/x");
            s.set("layers.concurrent_limit.http", "24").unwrap();
            s
        };
        for edits in [
            e(&[
                ("layers.concurrent_limit", "on"),
                ("layers.concurrent_limit.http", "24"),
            ]),
            e(&[
                ("layers.concurrent_limit.http", "24"),
                ("layers.concurrent_limit", "on"),
            ]),
        ] {
            let mut s = OperatorSpec::from_url("s3://b/x");
            s.apply_edits(&edits).unwrap();
            assert_eq!(s, want, "{edits:?}");
        }
        // Switching on a layer that is already on keeps its parameters.
        let mut s = want.clone();
        s.set("layers.concurrent_limit", "on").unwrap();
        assert_eq!(s, want);
        // The fields of a layer switched off are dropped; the last value wins.
        let mut s = OperatorSpec::from_url("s3://b/x");
        s.apply_edits(&e(&[
            ("layers.retry.max_times", "9"),
            ("layers.retry", "off"),
            ("read.gap", "1MiB"),
            ("read.gap", "2MiB"),
        ]))
        .unwrap();
        assert!(s.layers.retry.is_none());
        assert_eq!(s.read.gap, Some(ByteSize::mib(2)));
        // A layer switched on without a needed parameter still fails, once, at the end.
        let err = OperatorSpec::from_url("s3://b/x")
            .apply_edits(&e(&[("layers.concurrent_limit", "on")]))
            .unwrap_err();
        assert!(err.message.contains("needs `http`"), "{err:?}");
        // Secrets never show in the error.
        let err = OperatorSpec::from_url("s3://b/x")
            .apply_edits(&e(&[
                ("options.secret_access_key", "SEKRIT"),
                ("layers.concurrent_limit", "on"),
            ]))
            .unwrap_err();
        assert!(!err.message.contains("SEKRIT"), "{err:?}");
    }

    #[test]
    fn invalid_specs_are_refused() {
        for bad in [
            "",
            "url = \"\"",
            "url = \"s3://b\"\nbogus = 1",
            "url = \"s3://b\"\n[layers]\nconcurrent_limit = {}",
            "url = \"s3://b\"\n[layers]\nretry = { factor = 0.5 }",
            "url = \"s3://b\"\n[layers]\nthrottle = { bandwidth = 0 }",
        ] {
            assert!(OperatorSpec::parse(bad).is_err(), "{bad}");
        }
    }
}
