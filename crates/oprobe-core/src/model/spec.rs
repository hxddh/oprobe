//! Run spec: one replayable definition of a run.
//!
//! A [`RunSpec`] is the fully resolved profile of a run (the operator's secrets redacted), the
//! operator as the user named it (a URL, an `op.toml` path or a configured name), the seed, the
//! `--existing` prefix and the label. Every report carries one (`report.spec`), so any run can
//! be replayed exactly — `prove` replays a run's spec — or with a change: [`RunSpec::with`]
//! applies [`Overrides`] (the `--set PATH=VALUE` vocabulary) and [`RunSpec::command`] prints an
//! equivalent `oprobe run …` command line.
//!
//! [`RunSpec::hash`] identifies the measured configuration (workload + operator + target
//! identity); [`RunSpec::workload_hash`] the workload alone.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::config::{Access, DatasetSpec, OpSpec, Profile, WorkloadSpec};
use crate::errors::{ErrorCode, OprobeError};
use crate::input;
use crate::operator::OperatorSpec;
use crate::util::HumanDuration;

/// The value secrets are replaced with.
pub const REDACTED: &str = "<redacted>";

/// One replayable definition of a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunSpec {
    /// The fully resolved profile (preset defaults are kept implicit). Secret options of the
    /// operator are `"<redacted>"` (environment references such as `${AWS_SECRET_ACCESS_KEY}`
    /// are kept: they are not secrets).
    pub profile: Profile,
    /// The operator as given: a URL, an operator file or a configured name.
    pub operator_ref: String,
    /// The dataset / key-choice seed the run used.
    pub seed: u64,
    /// `--existing PREFIX`: profile existing objects (read-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Default for RunSpec {
    fn default() -> Self {
        Self::new(&Profile::from_url("memory://"), "memory://")
    }
}

/// A value made only of `${NAME}` references.
fn is_env_reference(v: &str) -> bool {
    let v = v.trim();
    v.starts_with("${") && v.ends_with('}') && !v[2..v.len() - 1].contains(['{', '}', '$'])
}

/// `o` as a spec stores it: URL credentials and secret query parameters stripped, secret
/// options redacted.
pub fn redact_operator(o: &OperatorSpec) -> OperatorSpec {
    let mut o = o.clone();
    o.url = crate::util::redact_url(&o.url);
    o.options.remove("ca_cert_pem");
    for (k, v) in o.options.iter_mut() {
        if crate::util::is_secret_key(k) {
            if !is_env_reference(v) {
                *v = REDACTED.to_owned();
            }
        } else {
            *v = crate::util::redact_url(v);
        }
    }
    o.http.proxy = o.http.proxy.as_deref().map(crate::util::redact_url);
    o
}

fn sha16(v: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    let d = sha2::Sha256::digest(&bytes);
    let hex: String = d.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Quote one shell word when it needs it (POSIX single quotes).
pub fn shell_quote(a: &str) -> String {
    let plain = !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,~".contains(c));
    if plain {
        a.to_owned()
    } else {
        format!("'{}'", a.replace('\'', r"'\''"))
    }
}

/// `oprobe` followed by `args`, quoted for a POSIX shell.
pub fn command_line(args: &[String]) -> String {
    let mut s = "oprobe".to_owned();
    for a in args {
        s.push(' ');
        s.push_str(&shell_quote(a));
    }
    s
}

impl RunSpec {
    /// The spec of a run of `profile` at `operator_ref` (secrets removed from both).
    pub fn new(profile: &Profile, operator_ref: impl Into<String>) -> Self {
        let mut p = profile.clone();
        p.operator = redact_operator(&p.operator);
        let existing = p.workload.effective_dataset().existing_prefix;
        Self {
            operator_ref: crate::util::redact_url(&operator_ref.into()),
            seed: p.workload.seed,
            existing,
            label: p.name.clone(),
            profile: p,
        }
    }

    /// The profile with `seed`, `existing` and `label` applied (they win over the profile's own
    /// values). Its secrets are still redacted: replay re-resolves credentials.
    pub fn resolved_profile(&self) -> Profile {
        let mut p = self.profile.clone();
        p.workload.seed = self.seed;
        p.name = self.label.clone();
        let mut ds = p.workload.effective_dataset();
        if ds.existing_prefix != self.existing {
            ds.existing_prefix = self.existing.clone();
            p.workload.dataset = Some(ds);
        }
        p
    }

    /// The target identity (`store::TargetId`), when the operator's service resolves.
    pub fn target_id(&self) -> Option<crate::store::TargetId> {
        crate::store::TargetId::of(&self.profile.operator.target_spec()).ok()
    }

    /// `sha256:<16 hex>` over the workload, the operator and the target identity (not the
    /// label, nor the seed): two runs with the same hash measured the same configuration.
    pub fn hash(&self) -> String {
        let p = self.resolved_profile();
        #[derive(Serialize)]
        struct H<'a> {
            workload: WorkloadKey,
            concurrency: usize,
            layers: &'a crate::operator::Layers,
            read: &'a crate::operator::ReadOptions,
            write: &'a crate::operator::WriteOptions,
            http: &'a crate::operator::HttpOptions,
            target: String,
        }
        sha16(&H {
            workload: WorkloadKey::of(&p.workload),
            concurrency: p.workload.concurrency,
            layers: &p.operator.layers,
            read: &p.operator.read,
            write: &p.operator.write,
            http: &p.operator.http,
            target: self
                .target_id()
                .map(|t| t.to_string())
                .unwrap_or_else(|| self.operator_ref.clone()),
        })
    }

    /// `sha256:<16 hex>` over the workload only: dataset, mix, access, rate, warmup, duration
    /// (and `max_ops`), with preset defaults resolved.
    pub fn workload_hash(&self) -> String {
        sha16(&WorkloadKey::of(&self.resolved_profile().workload))
    }

    /// An equivalent `oprobe run …` command line: the operator as given, the workload flags
    /// (preset, concurrency and duration always), and `--set PATH=VALUE` for every operator
    /// value that differs from the default operator of its URL and every workload setting
    /// without a flag. Redacted secrets are omitted.
    pub fn command(&self) -> String {
        command_line(&self.args())
    }

    /// The arguments of [`Self::command`] after `oprobe`.
    pub fn args(&self) -> Vec<String> {
        let p = self.resolved_profile();
        let def = crate::config::defaults();
        let mut a: Vec<String> = vec!["run".into(), self.operator_ref.clone()];
        let mut flag = |f: &str, v: Option<String>| {
            a.push(f.to_owned());
            if let Some(v) = v {
                a.push(v);
            }
        };
        let w = &p.workload;
        let dw = &def.workload;
        flag("--preset", Some(w.preset.slug().to_owned()));
        flag("-c", Some(w.concurrency.to_string()));
        flag("-d", Some(w.duration.to_string()));
        if let Some(r) = w.rate {
            flag("--rate", Some(r.to_string()));
        }
        let (pre_ds, pre_mix) = w.preset.defaults();
        let ds = w.effective_dataset();
        if let Some(e) = &ds.existing_prefix {
            flag("--existing", Some(e.clone()));
        }
        if ds.objects != pre_ds.objects {
            flag("--objects", Some(ds.objects.to_string()));
        }
        if let Some(d) = &ds.size_dist {
            flag("--object-size", Some(d.to_string()));
        } else if ds.object_size != pre_ds.object_size {
            flag("--object-size", Some(ds.object_size.to_string()));
        }
        if w.seed != dw.seed {
            flag("--seed", Some(w.seed.to_string()));
        }
        let mut sets: Vec<(String, String)> = Vec::new();
        if w.warmup != dw.warmup {
            sets.push(("warmup".into(), w.warmup.to_string()));
        }
        if let Some(n) = w.max_ops {
            sets.push(("max_ops".into(), n.to_string()));
        }
        if w.access != dw.access {
            sets.push(("access".into(), w.access.to_string()));
        }
        if ds.partitions != pre_ds.partitions {
            sets.push(("partitions".into(), ds.partitions.to_string()));
        }
        if ds.persist {
            sets.push(("persist".into(), "on".into()));
        }
        if !w.verify {
            sets.push(("verify".into(), "off".into()));
        }
        if !w.cleanup {
            sets.push(("cleanup".into(), "off".into()));
        }
        if !w.arrivals.is_constant() {
            sets.push(("arrivals".into(), w.arrivals.to_string()));
        }
        if let Some(sh) = &w.shape {
            sets.push(("shape".into(), sh.to_string()));
        }
        let mix = w.effective_mix();
        if mix != pre_mix {
            sets.push((
                "mix".into(),
                serde_json::to_string(&mix).unwrap_or_default(),
            ));
        }
        // The operator: what differs from the default operator of its URL.
        let base = OperatorSpec::from_url(p.operator.url.clone());
        for c in crate::operator::diff::diff(&base, &p.operator).changes {
            let v = match &c.to {
                Some(v) => v.trim_matches('"').to_owned(),
                None if c.path.starts_with("layers.") => "off".into(),
                None => "none".into(),
            };
            if v == REDACTED || c.path == "url" {
                continue;
            }
            sets.push((c.path, v));
        }
        let s = &p.sla;
        let ds_ = &def.sla;
        if !s.enforce {
            sets.push(("sla".into(), "none".into()));
        }
        if s.max_error_rate != ds_.max_error_rate {
            sets.push(("max_error_rate".into(), s.max_error_rate.to_string()));
        }
        if let Some(t) = s.max_throttle_rate {
            sets.push(("max_throttle_rate".into(), t.to_string()));
        }
        if let Some(ms) = s.p99_ms {
            sets.push(("p99_ms".into(), ms.to_string()));
        }
        if let Some(l) = &self.label {
            sets.push(("label".into(), l.clone()));
        }
        for (k, v) in sets {
            flag("--set", Some(format!("{k}={v}")));
        }
        a
    }

    /// This spec with `o` applied. Overrides are validated when they are built
    /// ([`Overrides::set`]); a value that does not apply here is skipped (see
    /// [`Self::try_with`] for the error).
    pub fn with(&self, o: &Overrides) -> RunSpec {
        match self.try_with(o) {
            Ok(s) => s,
            Err(_) => {
                let mut s = self.clone();
                for (k, v) in o.entries() {
                    if let Ok(next) = s.try_with(&Overrides::default().with_set(&k, &v)) {
                        s = next;
                    }
                }
                s
            }
        }
    }

    /// [`Self::with`], failing on the first override that does not apply.
    pub fn try_with(&self, o: &Overrides) -> Result<RunSpec, OprobeError> {
        let mut p = self.resolved_profile();
        let mut label = self.label.clone();
        o.apply(&mut p, &mut label)?;
        let mut spec = RunSpec::new(&p, self.operator_ref.clone());
        spec.label = label;
        spec.profile.name = spec.label.clone();
        Ok(spec)
    }

    /// `oprobe prove RUN --with …`: prove the change `o` on the stored run `run_id`.
    pub fn prove_command(run_id: &str, o: &Overrides) -> String {
        let mut args = vec!["prove".to_owned(), run_id.to_owned()];
        for (k, v) in o.entries() {
            args.push("--with".into());
            args.push(format!("{k}={v}"));
        }
        command_line(&args)
    }
}

/// What [`RunSpec::workload_hash`] covers.
#[derive(Serialize)]
struct WorkloadKey {
    dataset: DatasetSpec,
    mix: Vec<OpSpec>,
    access: Access,
    rate: Option<f64>,
    /// Arrival process and rate shape (`None` for constant arrivals at `rate`).
    #[serde(skip_serializing_if = "Option::is_none")]
    arrivals: Option<String>,
    warmup: HumanDuration,
    duration: HumanDuration,
    max_ops: Option<u64>,
}

impl WorkloadKey {
    fn of(w: &WorkloadSpec) -> Self {
        Self {
            dataset: w.effective_dataset(),
            mix: w.effective_mix(),
            access: w.access,
            rate: w.rate,
            arrivals: (!w.arrivals.is_constant() || w.shape.is_some()).then(|| {
                format!(
                    "{}{}",
                    w.arrivals,
                    w.shape
                        .as_ref()
                        .map(|s| format!(" {s}"))
                        .unwrap_or_default()
                )
            }),
            warmup: w.warmup,
            duration: w.duration,
            max_ops: w.max_ops,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Overrides: the `--set PATH=VALUE` vocabulary
// ---------------------------------------------------------------------------------------

/// Which part of the profile a `--set` key changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Workload,
    Sla,
    Run,
}

/// One `--set` key of the workload, the SLA or the run (operator paths are
/// [`crate::operator::SET_PATHS`]).
#[derive(Clone, Copy, Debug)]
pub struct SetKey {
    pub name: &'static str,
    pub section: Section,
    pub aliases: &'static [&'static str],
    pub help: &'static str,
}

const fn key(
    name: &'static str,
    section: Section,
    aliases: &'static [&'static str],
    help: &'static str,
) -> SetKey {
    SetKey {
        name,
        section,
        aliases,
        help,
    }
}

/// Every non-operator `--set` key.
pub const SET_KEYS: &[SetKey] = &[
    key("concurrency", Section::Workload, &["c"], "concurrent operations"),
    key(
        "preset",
        Section::Workload,
        &[],
        "workload preset (resets dataset and mix)",
    ),
    key("duration", Section::Workload, &["d"], "measurement window"),
    key("warmup", Section::Workload, &[], "warmup before measuring"),
    key(
        "max_ops",
        Section::Workload,
        &[],
        "stop after N measured operations (0 = no limit)",
    ),
    key(
        "rate",
        Section::Workload,
        &[],
        "open-loop arrival rate in ops/s (0 = closed loop)",
    ),
    key("access", Section::Workload, &[], "uniform, zipf:S or hotset:F/P"),
    key("objects", Section::Workload, &[], "dataset objects to generate"),
    key("object_size", Section::Workload, &[], "size of each generated object"),
    key(
        "partitions",
        Section::Workload,
        &[],
        "dataset partitions (part=NN/ prefixes)",
    ),
    key(
        "existing",
        Section::Workload,
        &["existing_prefix"],
        "profile existing objects under this prefix (empty: generate)",
    ),
    key(
        "persist",
        Section::Workload,
        &["persist_dataset"],
        "keep the generated dataset (on/off)",
    ),
    key("verify", Section::Workload, &[], "CRC32C verification (on/off)"),
    key("cleanup", Section::Workload, &[], "delete test data at the end (on/off)"),
    key("seed", Section::Workload, &[], "dataset and key-choice seed"),
    key(
        "arrivals",
        Section::Workload,
        &[],
        "open-loop arrival process: constant or poisson",
    ),
    key(
        "shape",
        Section::Workload,
        &["rate_shape"],
        "time-varying open-loop rate: ramp:FROM..TO/D, step:R1,R2/D, diurnal[:MIN..MAX/P], or none",
    ),
    key(
        "mix",
        Section::Workload,
        &[],
        "operation mix as JSON ([{\"op\":\"read\",\"weight\":1}]), or empty for the preset's",
    ),
    key("max_error_rate", Section::Sla, &[], "SLA: max share of failed operations"),
    key(
        "max_throttle_rate",
        Section::Sla,
        &[],
        "SLA: max share of throttled operations (none: unchecked)",
    ),
    key(
        "p99_ms",
        Section::Sla,
        &[],
        "SLA: max p99 per operation in ms (none: unchecked)",
    ),
    key("sla", Section::Sla, &[], "enforce or none"),
    key("label", Section::Run, &[], "label stored with the run"),
];

/// Short names of operator values (the 5.x `--set` keys) and the operator paths they set.
/// `max_inflight_requests=0` and `retries=0` take the layer out; `cache=SIZE` sizes the cache.
fn alias_paths(name: &str, v: &str) -> Option<Vec<(&'static str, String)>> {
    let off = |v: &str| matches!(v.trim(), "0");
    Some(match name {
        "io_concurrency" => vec![
            ("read.concurrent", v.to_owned()),
            ("write.concurrent", v.to_owned()),
        ],
        "chunk_size" => vec![("read.chunk", v.to_owned()), ("write.chunk", v.to_owned())],
        "range_gap" => vec![("read.gap", v.to_owned())],
        "max_inflight_requests" | "max_inflight" if off(v) => {
            vec![("layers.concurrent_limit", "off".to_owned())]
        }
        "max_inflight_requests" | "max_inflight" => {
            vec![("layers.concurrent_limit.http", v.to_owned())]
        }
        "http_version" | "http" => vec![("http.version", v.to_owned())],
        "keep_alive" => vec![("http.keep_alive", v.to_owned())],
        "cache" => match v.trim().to_ascii_lowercase().as_str() {
            "on" | "off" | "true" | "false" | "yes" | "no" => {
                vec![("layers.cache", v.to_owned())]
            }
            _ => vec![("layers.cache.memory", v.to_owned())],
        },
        "cache_size" => vec![("layers.cache.memory", v.to_owned())],
        "retries" if off(v) => vec![("layers.retry", "off".to_owned())],
        "retries" => vec![("layers.retry.max_times", v.to_owned())],
        "tail_cut" => vec![("layers.tail_cut", v.to_owned())],
        "timeout" => vec![("layers.timeout.op", v.to_owned())],
        "endpoint" => vec![("options.endpoint", v.to_owned())],
        "region" => vec![("options.region", v.to_owned())],
        _ => return None,
    })
}

/// Whether `name` is an operator path (`url`, `options.KEY`, `layers.…`, `read.…`, …).
fn is_operator_path(name: &str) -> bool {
    name == "url"
        || ["options.", "layers.", "read.", "write.", "http."]
            .iter()
            .any(|p| name.starts_with(p))
}

/// The non-operator `--set` key `name` (or an alias; `-` spellings accepted).
pub fn set_key(name: &str) -> Result<&'static SetKey, OprobeError> {
    let n = name.trim().to_ascii_lowercase().replace('-', "_");
    if let Some(k) = SET_KEYS
        .iter()
        .find(|k| k.name == n || k.aliases.contains(&n.as_str()))
    {
        return Ok(k);
    }
    let names: Vec<&str> = SET_KEYS
        .iter()
        .map(|k| k.name)
        .chain(crate::operator::SET_PATHS.iter().copied())
        .collect();
    let hint = match input::suggest(&n, names.iter().copied()) {
        Some(s) => format!("did you mean `{s}`?"),
        None => "operator paths: url, options.KEY, layers.NAME[.FIELD], read.*, write.*, http.*; \
                 workload keys: concurrency, preset, duration, warmup, rate, objects, object_size, …"
            .to_owned(),
    };
    Err(input::invalid(format!("unknown --set key `{}`", name.trim())).with_hint(hint))
}

fn none_word(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "" | "none" | "off" | "unset"
    )
}

/// Apply one validated workload, SLA or run key to `p`.
fn apply_one(
    p: &mut Profile,
    label: &mut Option<String>,
    key: &SetKey,
    v: &str,
) -> Result<(), OprobeError> {
    let v = v.trim();
    let what = format!("--set {}", key.name);
    let usize_of = |min: u64| -> Result<usize, OprobeError> {
        Ok(input::count_at_least(&what, v, min)? as usize)
    };
    let dataset = |p: &mut Profile, f: &dyn Fn(&mut DatasetSpec)| {
        let mut ds = p.workload.effective_dataset();
        f(&mut ds);
        p.workload.dataset = Some(ds);
    };
    match key.name {
        "concurrency" => p.workload.concurrency = usize_of(1)?,
        "preset" => {
            p.workload.preset = input::preset(v)?;
            p.workload.mix.clear();
            p.workload.dataset = None;
        }
        "duration" => p.workload.duration = HumanDuration(input::duration(v)?),
        "warmup" => p.workload.warmup = HumanDuration(input::duration(v)?),
        "max_ops" => {
            let n = input::count_at_least(&what, v, 0)?;
            p.workload.max_ops = (n > 0).then_some(n);
        }
        "rate" => {
            let r = input::non_negative(&what, v)?;
            p.workload.rate = (r > 0.0).then_some(r);
        }
        "access" => p.workload.access = input::access(v)?,
        "arrivals" => p.workload.arrivals = v.parse()?,
        "shape" => {
            p.workload.shape = if none_word(v) {
                None
            } else {
                Some(v.parse()?)
            };
        }
        "objects" => {
            let n = usize_of(1)?;
            dataset(p, &|d| d.objects = n);
        }
        "object_size" => {
            if crate::config::SizeDist::is_dist(v) {
                let dist: crate::config::SizeDist = v.parse()?;
                let nominal = crate::util::ByteSize(dist.nominal());
                dataset(p, &|d| {
                    d.object_size = nominal;
                    d.size_dist = Some(dist.clone());
                });
            } else {
                let s = input::nonzero_size(v)?;
                dataset(p, &|d| {
                    d.object_size = s;
                    d.size_dist = None;
                });
            }
        }
        "partitions" => {
            let n = usize_of(1)?;
            dataset(p, &|d| d.partitions = n);
        }
        "existing" => {
            let e = (!v.is_empty()).then(|| v.to_owned());
            dataset(p, &|d| d.existing_prefix = e.clone());
        }
        "persist" => {
            let b = input::boolean(&what, v)?;
            dataset(p, &|d| d.persist = b);
        }
        "verify" => p.workload.verify = input::boolean(&what, v)?,
        "cleanup" => p.workload.cleanup = input::boolean(&what, v)?,
        "seed" => p.workload.seed = input::count_at_least(&what, v, 0)?,
        "mix" => {
            p.workload.mix = if none_word(v) {
                Vec::new()
            } else {
                let mix: Vec<OpSpec> = serde_json::from_str(v).map_err(|e| {
                    input::invalid(format!("invalid {what}: {e}"))
                        .with_hint(r#"a JSON array, e.g. [{"op":"read","weight":3},{"op":"stat"}]"#)
                })?;
                if !mix.iter().any(|m| m.weight > 0) {
                    return Err(input::invalid(format!(
                        "{what} needs at least one op with weight > 0"
                    )));
                }
                mix
            }
        }
        "max_error_rate" => p.sla.max_error_rate = input::share(&what, v)?,
        "max_throttle_rate" => {
            p.sla.max_throttle_rate = if none_word(v) {
                None
            } else {
                Some(input::share(&what, v)?)
            }
        }
        "p99_ms" => {
            p.sla.p99_ms = if none_word(v) {
                None
            } else {
                Some(input::positive(&what, v)?)
            }
        }
        "sla" => {
            p.sla.enforce = input::choice("SLA mode", v, &[("enforce", true), ("none", false)])?
        }
        "label" => *label = (!v.is_empty()).then(|| v.to_owned()),
        other => {
            return Err(OprobeError::new(
                ErrorCode::Internal,
                format!("--set key `{other}` has no setter"),
            ));
        }
    }
    Ok(())
}

/// A change to a run: operator paths, workload and SLA keys, and the label. Built with
/// [`Overrides::set`], which validates.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Overrides {
    /// Operator paths (`layers.retry.max_times`, `read.gap`, `options.region`, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operator: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workload: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sla: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Overrides {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `key` (an operator path, a workload/SLA key, or a short alias such as `tail_cut`)
    /// to `value`, validated.
    pub fn set(&mut self, key: &str, value: &str) -> Result<&mut Self, OprobeError> {
        let k = key.trim().to_ascii_lowercase().replace('-', "_");
        let v = value.trim().to_owned();
        if let Some(paths) = alias_paths(&k, &v) {
            for (p, pv) in paths {
                self.set_operator(p, &pv)?;
            }
            return Ok(self);
        }
        if is_operator_path(&k) {
            self.set_operator(key.trim(), &v)?;
            return Ok(self);
        }
        let sk = set_key(&k)?;
        let mut p = Profile::from_url("memory://");
        apply_one(&mut p, &mut None, sk, &v)?;
        match sk.section {
            Section::Workload => {
                self.workload.insert(sk.name.into(), v);
            }
            Section::Sla => {
                self.sla.insert(sk.name.into(), v);
            }
            Section::Run => self.label = Some(v),
        }
        Ok(self)
    }

    fn set_operator(&mut self, path: &str, v: &str) -> Result<(), OprobeError> {
        let mut scratch = OperatorSpec::from_url("memory://");
        // A field of a layer that a previous pair took out is checked on its own.
        scratch.set(path, v)?;
        // A layer turned on or off replaces the pairs of its fields given before.
        if path.matches('.').count() == 1 && path.starts_with("layers.") {
            let prefix = format!("{path}.");
            self.operator.retain(|k, _| !k.starts_with(&prefix));
        }
        self.operator.insert(path.to_owned(), v.to_owned());
        Ok(())
    }

    /// Builder form of [`Self::set`] for values known to be valid (an invalid pair is
    /// dropped, with a debug assertion).
    pub fn with_set(mut self, key: &str, value: &str) -> Self {
        let r = self.set(key, value).map(|_| ());
        debug_assert!(r.is_ok(), "invalid override {key}={value}: {r:?}");
        self
    }

    /// Parse `KEY=VALUE` strings (`--set` arguments).
    pub fn parse<S: AsRef<str>>(sets: &[S]) -> Result<Self, OprobeError> {
        let mut o = Self::default();
        for s in sets {
            let (k, v) = input::key_value("--set", s.as_ref())?;
            o.set(&k, &v)?;
        }
        Ok(o)
    }

    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Every pair as `(key, value)`, in application order (preset first, so a dataset or mix
    /// override given with it survives the preset's reset; a whole layer before its fields).
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = Vec::new();
        if let Some(p) = self.workload.get("preset") {
            v.push(("preset".into(), p.clone()));
        }
        for (k, val) in &self.workload {
            if k != "preset" {
                v.push((k.clone(), val.clone()));
            }
        }
        let mut ops: Vec<(&String, &String)> = self.operator.iter().collect();
        ops.sort_by_key(|(k, _)| (k.matches('.').count(), k.as_str().to_owned()));
        v.extend(ops.into_iter().map(|(k, val)| (k.clone(), val.clone())));
        for (k, val) in &self.sla {
            v.push((k.clone(), val.clone()));
        }
        if let Some(l) = &self.label {
            v.push(("label".into(), l.clone()));
        }
        v
    }

    /// `["--set", "k=v", …]`.
    pub fn to_set_args(&self) -> Vec<String> {
        self.entries()
            .into_iter()
            .flat_map(|(k, v)| ["--set".to_owned(), format!("{k}={v}")])
            .collect()
    }

    /// Add `other`'s pairs (they win).
    pub fn merge(&mut self, other: &Overrides) {
        for (k, v) in other.entries() {
            let _ = self.set(&k, &v);
        }
    }

    /// Apply to a profile (and the run label).
    pub fn apply(&self, p: &mut Profile, label: &mut Option<String>) -> Result<(), OprobeError> {
        for (k, v) in self.entries() {
            if is_operator_path(&k) {
                p.operator.set(&k, &v)?;
            } else {
                apply_one(p, label, set_key(&k)?, &v)?;
            }
        }
        Ok(())
    }

    /// A one-line summary: `concurrency=8 layers.tail_cut=on`.
    pub fn summary(&self) -> String {
        self.entries()
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ByteSize;

    fn profile() -> Profile {
        let mut p = Profile::from_url("s3://bkt/x");
        let o = &mut p.operator.options;
        o.insert("endpoint".into(), "https://minio.local:9000".into());
        o.insert("region".into(), "eu-west-1".into());
        o.insert("secret_access_key".into(), "hunter2".into());
        o.insert("access_key_id".into(), "${MY_KEY_ID}".into());
        p.operator.layers.tail_cut = Some(Default::default());
        p.workload.preset = crate::config::Preset::SmallFiles;
        p.workload.concurrency = 24;
        p.workload.duration = HumanDuration::secs(12);
        p.workload.warmup = HumanDuration(std::time::Duration::from_millis(500));
        p.workload.seed = 7;
        p.name = Some("base".into());
        p
    }

    #[test]
    fn secrets_are_redacted() {
        let s = RunSpec::new(&profile(), "s3://bkt/x");
        let o = &s.profile.operator.options;
        assert_eq!(o["secret_access_key"], REDACTED);
        assert_eq!(o["access_key_id"], "${MY_KEY_ID}");
        assert_eq!(s.seed, 7);
        assert_eq!(s.label.as_deref(), Some("base"));
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
        assert!(!s.command().contains("hunter2"), "{}", s.command());
        for (url, clean) in [
            ("webdav://alice:hunter2@host/files", "webdav://host/files"),
            (
                "s3://bucket/p?region=eu-west-1&secret_access_key=hunter2",
                "s3://bucket/p?region=eu-west-1",
            ),
        ] {
            let s = RunSpec::new(&Profile::from_url(url), url);
            assert_eq!(s.operator_ref, clean);
            assert_eq!(s.profile.operator.url, clean);
            assert!(!serde_json::to_string(&s).unwrap().contains("hunter2"));
        }
    }

    #[test]
    fn commands_name_the_operator_changes() {
        let d = RunSpec::new(&Profile::from_url("memory://"), "memory://");
        assert_eq!(
            d.command(),
            "oprobe run memory:// --preset parquet -c 16 -d 10s"
        );
        let s = RunSpec::new(&profile(), "s3://bkt/x");
        let c = s.command();
        assert!(c.contains("--set layers.tail_cut=on"), "{c}");
        assert!(c.contains("--set options.region=eu-west-1"), "{c}");
        assert!(c.contains("-c 24") && c.contains("--set warmup=500ms"), "{c}");
    }

    #[test]
    fn overrides_change_only_what_they_name() {
        let s = RunSpec::new(&profile(), "s3://bkt/x");
        let o = Overrides::new()
            .with_set("concurrency", "48")
            .with_set("tail_cut", "off")
            .with_set("layers.retry.max_times", "5")
            .with_set("warmup", "2s");
        assert_eq!(o.workload["concurrency"], "48");
        assert_eq!(o.operator["layers.tail_cut"], "off");
        let n = s.with(&o);
        let (a, b) = (s.resolved_profile(), n.resolved_profile());
        assert_eq!(b.workload.concurrency, 48);
        assert!(b.operator.layers.tail_cut.is_none());
        assert_eq!(b.operator.retries(), 5);
        assert_eq!(b.workload.warmup, HumanDuration::secs(2));
        assert_eq!(b.workload.dataset, a.workload.dataset);
        assert_eq!(b.operator.options, a.operator.options);
        assert_ne!(n.hash(), s.hash());
        assert_ne!(n.workload_hash(), s.workload_hash(), "warmup is workload");
        let op_only = s.with(&Overrides::new().with_set("read.gap", "1MiB"));
        assert_eq!(op_only.workload_hash(), s.workload_hash());
        assert_ne!(op_only.hash(), s.hash());
        let relabel = s.with(&Overrides::new().with_set("label", "x").with_set("seed", "9"));
        assert_eq!(relabel.hash(), s.hash());
        assert_eq!(relabel.seed, 9);
        assert!(n.command().contains("-c 48"), "{}", n.command());
        assert_eq!(
            RunSpec::prove_command("20261121-101010-abcd", &Overrides::new().with_set("tail_cut", "on")),
            "oprobe prove 20261121-101010-abcd --with layers.tail_cut=on"
        );
    }

    #[test]
    fn aliases_map_to_operator_paths() {
        let o = Overrides::parse(&["http=h2", "max-inflight=4", "cache=128MiB", "io_concurrency=3"])
            .unwrap();
        assert_eq!(o.operator["http.version"], "h2");
        assert_eq!(o.operator["layers.concurrent_limit.http"], "4");
        assert_eq!(o.operator["read.concurrent"], "3");
        let s = RunSpec::default().with(&o);
        let op = &s.profile.operator;
        assert_eq!(op.layers.cache.as_ref().unwrap().memory, ByteSize::mib(128));
        assert_eq!(op.http_limit(), Some(4));
        assert_eq!(op.write_concurrent(), 3);
        let o = Overrides::parse(&["retries=0"]).unwrap();
        assert_eq!(RunSpec::default().with(&o).profile.operator.retries(), 0);
    }

    #[test]
    fn set_keys_are_validated_with_hints() {
        let e = Overrides::parse(&["concurency=8"]).unwrap_err();
        assert_eq!(e.code, ErrorCode::UsageInvalidArgument);
        assert!(e.hint.unwrap().contains("did you mean `concurrency`"));
        let e = Overrides::parse(&["layers.retry.max_tims=8"]).unwrap_err();
        assert!(e.hint.unwrap().contains("layers.retry.max_times"));
        for bad in [
            "concurrency=0",
            "warmup=5x",
            "preset=nope",
            "access=weird",
            "mix=[",
            "chunk_size=1",
            "read.concurrent=0",
            "novalue",
        ] {
            let e = Overrides::parse(&[bad]).unwrap_err();
            assert_eq!(e.code, ErrorCode::UsageInvalidArgument, "{bad}");
        }
    }
}
