//! Run spec (design v4 §1): one replayable definition of a run.
//!
//! A [`RunSpec`] is the fully resolved profile of a run (secrets redacted), the target as the
//! user named it, the seed, the `--existing` prefix and the label. Every report carries one
//! (`report.spec`), so any run can be replayed exactly — `oprobe run --from RUNREF` — or with a
//! change: [`RunSpec::with`] applies [`Overrides`] (the `--set key=value` vocabulary) and
//! [`RunSpec::command`] prints an equivalent `oprobe run …` command line. Rerun and
//! verification commands (findings, the App, MCP) are built here and nowhere else, so no
//! parameter (warmup, seed, mix, dataset, endpoint, region, HTTP version, tail-cut, rate,
//! `--existing`, `--max-ops`, …) is ever dropped.
//!
//! [`RunSpec::hash`] identifies the measured configuration (workload + knobs + target
//! identity); [`RunSpec::workload_hash`] the workload alone: compare and the capacity history
//! key on it.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::config::{
    Access, DatasetSpec, OpSpec, Profile, TargetSpec, TransportMode, WorkloadSpec,
};
use crate::errors::{ErrorCode, OprobeError};
use crate::input;
use crate::util::HumanDuration;

/// The value secrets are replaced with.
pub const REDACTED: &str = "<redacted>";

/// One replayable definition of a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunSpec {
    /// The fully resolved profile (preset defaults are kept implicit, as in a profile file).
    /// Secret options are `"<redacted>"` (environment references such as
    /// `${AWS_SECRET_ACCESS_KEY}` are kept: they are not secrets); options the target URL
    /// implies are left out, so the profile's target holds only what was added on top.
    pub profile: Profile,
    /// The target as given: a configured target name, a storage URL or `lab://SCENARIO`.
    pub target_ref: String,
    /// The dataset / key-choice seed the run used.
    pub seed: u64,
    /// `--existing PREFIX`: benchmark existing objects (read-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Default for RunSpec {
    fn default() -> Self {
        Self::new(
            &Profile::from_target(TargetSpec {
                url: Some("memory://".into()),
                scheme: "memory".into(),
                ..Default::default()
            }),
            "memory://",
        )
    }
}

/// The options a target URL implies (`bucket`, `root`, … ; for `lab://` everything the lab
/// sets, including its ephemeral endpoint). Never starts a lab server.
fn url_implied(url: &str) -> BTreeMap<String, String> {
    if url.trim().to_ascii_lowercase().starts_with("lab://") {
        return [
            "bucket",
            "region",
            "endpoint",
            "access_key_id",
            "secret_access_key",
            "disable_config_load",
            "root",
        ]
        .iter()
        .map(|k| ((*k).to_owned(), String::new()))
        .collect();
    }
    TargetSpec::from_url(url)
        .map(|t| t.options)
        .unwrap_or_default()
}

/// A value made only of `${NAME}` references.
fn is_env_reference(v: &str) -> bool {
    let v = v.trim();
    v.starts_with("${") && v.ends_with('}') && !v[2..v.len() - 1].contains(['{', '}', '$'])
}

/// `t` as a spec stores it: URL credentials stripped, URL-implied options dropped, secrets
/// redacted.
fn redact_target(t: &TargetSpec) -> TargetSpec {
    let mut t = t.clone();
    if let Some(url) = &t.url {
        let implied = url_implied(url);
        let lab = url.trim().to_ascii_lowercase().starts_with("lab://");
        t.options.retain(|k, v| match implied.get(k) {
            Some(_) if lab => false,
            Some(i) => i != v,
            None => true,
        });
        t.url = Some(crate::util::strip_url_credentials(url));
    }
    t.options.remove("ca_cert_pem");
    for (k, v) in t.options.iter_mut() {
        if crate::util::is_secret_key(k) {
            if !is_env_reference(v) {
                *v = REDACTED.to_owned();
            }
        } else {
            *v = crate::util::strip_url_credentials(v);
        }
    }
    t.proxy = t.proxy.as_deref().map(crate::util::strip_url_credentials);
    t
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
    /// The spec of a run of `profile` (as built from flags, a profile file or tool arguments,
    /// before the session normalises it) at `target_ref`.
    pub fn new(profile: &Profile, target_ref: impl Into<String>) -> Self {
        let mut p = profile.clone();
        p.target = redact_target(&p.target);
        let existing = p.workload.effective_dataset().existing_prefix;
        Self {
            target_ref: target_ref.into(),
            seed: p.workload.seed,
            existing,
            label: p.name.clone(),
            profile: p,
        }
    }

    /// The profile with `seed`, `existing` and `label` applied (they win over the profile's own
    /// values). Its secrets are still redacted: replay re-resolves credentials from the target.
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

    /// The target identity (`store::TargetId`), when the target resolves.
    pub fn target_id(&self) -> Option<crate::store::TargetId> {
        crate::store::TargetId::of(&self.profile.target).ok()
    }

    /// `sha256:<16 hex>` over the workload, the knobs and the target identity (not the label,
    /// nor the seed): two runs with the same hash measured the same configuration.
    pub fn hash(&self) -> String {
        let p = self.resolved_profile();
        #[derive(Serialize)]
        struct H<'a> {
            workload: WorkloadKey,
            knobs: &'a crate::config::Knobs,
            target: String,
        }
        sha16(&H {
            workload: WorkloadKey::of(&p.workload),
            knobs: &p.knobs,
            target: self
                .target_id()
                .map(|t| t.to_string())
                .unwrap_or_else(|| self.target_ref.clone()),
        })
    }

    /// `sha256:<16 hex>` over the workload only: dataset, mix, access, rate, warmup, duration
    /// (and `max_ops`), with preset defaults resolved, so `--preset small-files` and the same
    /// mix spelled out hash alike.
    pub fn workload_hash(&self) -> String {
        sha16(&WorkloadKey::of(&self.resolved_profile().workload))
    }

    /// An equivalent `oprobe run …` command line: the target as given, then every setting
    /// that differs from the built-in defaults ([`crate::config::defaults`]); the preset, the
    /// duration and the concurrency are always spelled out. Settings without a flag of their
    /// own are `--set key=value`. Redacted secrets are omitted (credentials are re-resolved).
    pub fn command(&self) -> String {
        command_line(&self.args())
    }

    /// The arguments of [`Self::command`] after `oprobe`.
    pub fn args(&self) -> Vec<String> {
        let p = self.resolved_profile();
        let def = crate::config::defaults();
        let mut a: Vec<String> = vec!["run".into(), self.target_ref.clone()];
        let mut flag = |f: &str, v: Option<String>| {
            a.push(f.to_owned());
            if let Some(v) = v {
                a.push(v);
            }
        };
        // Target additions (what the target name / URL does not imply).
        let t = &p.target;
        for (k, v) in &t.options {
            if v == REDACTED {
                continue;
            }
            match k.as_str() {
                "endpoint" => flag("--endpoint", Some(v.clone())),
                "region" => flag("--region", Some(v.clone())),
                _ => flag("--option", Some(format!("{k}={v}"))),
            }
        }
        if t.insecure {
            flag("--insecure", None);
        }
        if let Some(ca) = &t.ca_cert {
            flag("--ca-cert", Some(ca.display().to_string()));
        }
        if let Some(px) = &t.proxy
            && !self.target_ref.to_ascii_lowercase().starts_with("lab://")
        {
            flag("--proxy", Some(px.clone()));
        }
        // Workload.
        let w = &p.workload;
        let dw = &def.workload;
        flag("--preset", Some(w.preset.slug().to_owned()));
        flag("--duration", Some(w.duration.to_string()));
        if w.warmup != dw.warmup {
            flag("--warmup", Some(w.warmup.to_string()));
        }
        if let Some(n) = w.max_ops {
            flag("--max-ops", Some(n.to_string()));
        }
        if let Some(r) = w.rate {
            flag("--rate", Some(r.to_string()));
        }
        if w.access != dw.access {
            flag("--access", Some(w.access.to_string()));
        }
        let (pre_ds, pre_mix) = w.preset.defaults();
        let ds = w.effective_dataset();
        if let Some(e) = &ds.existing_prefix {
            flag("--existing", Some(e.clone()));
        }
        if ds.objects != pre_ds.objects {
            flag("--objects", Some(ds.objects.to_string()));
        }
        if ds.object_size != pre_ds.object_size {
            flag("--object-size", Some(ds.object_size.to_string()));
        }
        if ds.persist {
            flag("--persist-dataset", None);
        }
        if !w.verify {
            flag("--no-verify", None);
        }
        if !w.cleanup {
            flag("--keep-data", None);
        }
        let mut sets: Vec<(String, String)> = Vec::new();
        if ds.partitions != pre_ds.partitions {
            sets.push(("partitions".into(), ds.partitions.to_string()));
        }
        if w.seed != dw.seed {
            sets.push(("seed".into(), w.seed.to_string()));
        }
        let mix = w.effective_mix();
        if mix != pre_mix {
            sets.push((
                "mix".into(),
                serde_json::to_string(&mix).unwrap_or_default(),
            ));
        }
        // Knobs.
        let k = &p.knobs;
        let dk = &def.knobs;
        flag("--concurrency", Some(k.concurrency.to_string()));
        if k.io_concurrency != dk.io_concurrency {
            flag("--io-concurrency", Some(k.io_concurrency.to_string()));
        }
        if k.chunk_size != dk.chunk_size {
            flag("--chunk-size", Some(k.chunk_size.to_string()));
        }
        if k.range_gap != dk.range_gap {
            flag("--range-gap", Some(k.range_gap.to_string()));
        }
        if k.max_inflight_requests != dk.max_inflight_requests {
            flag("--max-inflight", Some(k.max_inflight_requests.to_string()));
        }
        if k.http_version != dk.http_version {
            flag("--http", Some(http_slug(k.http_version).into()));
        }
        if !k.keep_alive {
            flag("--no-keep-alive", None);
        }
        if k.transport == TransportMode::Microscope {
            flag("--microscope", None);
        }
        if k.cache {
            flag("--cache", Some(k.cache_size.to_string()));
        }
        if k.retries != dk.retries {
            flag("--retries", Some(k.retries.to_string()));
        }
        if k.tail_cut {
            flag("--tail-cut", None);
        }
        if k.timeout != dk.timeout {
            sets.push(("timeout".into(), k.timeout.to_string()));
        }
        let pk = &k.provider;
        if pk.addressing != crate::config::Addressing::Auto {
            flag(
                "--addressing",
                Some(format!("{:?}", pk.addressing).to_ascii_lowercase()),
            );
        }
        if let Some(c) = &pk.checksum {
            flag("--checksum", Some(c.clone()));
        }
        if let Some(e) = &pk.sse {
            flag("--sse", Some(e.clone()));
        }
        // SLA.
        let s = &p.sla;
        let ds_ = &def.sla;
        if !s.enforce {
            flag("--sla", Some("none".into()));
        }
        if s.max_error_rate != ds_.max_error_rate {
            flag("--max-error-rate", Some(s.max_error_rate.to_string()));
        }
        if let Some(t) = s.max_throttle_rate {
            flag("--max-throttle-rate", Some(t.to_string()));
        }
        if let Some(ms) = s.p99_ms {
            flag("--p99-ms", Some(ms.to_string()));
        }
        // Cost.
        let c = &p.cost;
        let dc = &def.cost;
        if let Some(pr) = &c.provider {
            flag("--provider", Some(pr.clone()));
        }
        if c.monthly_ops != dc.monthly_ops {
            flag("--monthly-ops", Some(c.monthly_ops.to_string()));
        }
        if c.egress_fraction != dc.egress_fraction {
            flag("--egress-fraction", Some(c.egress_fraction.to_string()));
        }
        if c.stored_gib != dc.stored_gib {
            flag("--stored-gib", Some(c.stored_gib.to_string()));
        }
        if let Some(pr) = &c.pricing {
            sets.push((
                "pricing".into(),
                serde_json::to_string(pr).unwrap_or_default(),
            ));
        }
        if let Some(l) = &self.label {
            flag("--label", Some(l.clone()));
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
        let mut spec = RunSpec::new(&p, self.target_ref.clone());
        spec.label = label;
        // The target keeps its redacted form (it was redacted already).
        spec.profile.target = p.target;
        spec.profile.name = spec.label.clone();
        Ok(spec)
    }

    /// `oprobe run --from RUN_ID --set …`: replay the stored run `run_id` with `o` applied.
    pub fn rerun_from(run_id: &str, o: &Overrides) -> String {
        let mut args = vec!["run".to_owned(), "--from".into(), run_id.to_owned()];
        args.extend(o.to_set_args());
        command_line(&args)
    }
}

fn http_slug(h: crate::net::HttpVersion) -> &'static str {
    match h {
        crate::net::HttpVersion::Auto => "auto",
        crate::net::HttpVersion::H1 => "h1",
        crate::net::HttpVersion::H2 => "h2",
    }
}

/// What [`RunSpec::workload_hash`] covers.
#[derive(Serialize)]
struct WorkloadKey {
    dataset: DatasetSpec,
    mix: Vec<OpSpec>,
    access: Access,
    rate: Option<f64>,
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
            warmup: w.warmup,
            duration: w.duration,
            max_ops: w.max_ops,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Overrides: the `--set key=value` vocabulary
// ---------------------------------------------------------------------------------------

/// Which part of the profile a `--set` key changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Knobs,
    Workload,
    Target,
    Sla,
    Cost,
    Run,
}

/// One `--set` key.
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

/// Every `--set` key (`Overrides`), with the profile field it sets.
pub const SET_KEYS: &[SetKey] = &[
    key(
        "concurrency",
        Section::Knobs,
        &["c"],
        "concurrent operations (workers)",
    ),
    key(
        "io_concurrency",
        Section::Knobs,
        &[],
        "per-operation reader/writer parallelism",
    ),
    key(
        "chunk_size",
        Section::Knobs,
        &[],
        "reader/writer chunk and multipart part size",
    ),
    key(
        "range_gap",
        Section::Knobs,
        &[],
        "merge vectored ranges closer than this",
    ),
    key(
        "max_inflight_requests",
        Section::Knobs,
        &["max_inflight"],
        "cap on in-flight HTTP requests (0 = unlimited)",
    ),
    key("http_version", Section::Knobs, &["http"], "auto, h1 or h2"),
    key("transport", Section::Knobs, &[], "opendal or microscope"),
    key(
        "keep_alive",
        Section::Knobs,
        &[],
        "reuse connections (on/off)",
    ),
    key(
        "cache",
        Section::Knobs,
        &[],
        "read cache: off, on, or its size",
    ),
    key("cache_size", Section::Knobs, &[], "read cache size"),
    key(
        "retries",
        Section::Knobs,
        &[],
        "retry attempts for temporary errors",
    ),
    key(
        "tail_cut",
        Section::Knobs,
        &[],
        "cancel and retry stragglers (on/off)",
    ),
    key("timeout", Section::Knobs, &[], "per-operation timeout"),
    key(
        "addressing",
        Section::Knobs,
        &[],
        "S3 addressing: auto, virtual or path",
    ),
    key(
        "checksum",
        Section::Knobs,
        &[],
        "write checksum (crc32c), or none",
    ),
    key(
        "sse",
        Section::Knobs,
        &[],
        "server-side encryption (AES256, aws:kms), or none",
    ),
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
    key(
        "access",
        Section::Workload,
        &[],
        "uniform, zipf:S or hotset:F/P",
    ),
    key(
        "objects",
        Section::Workload,
        &[],
        "dataset objects to generate",
    ),
    key(
        "object_size",
        Section::Workload,
        &[],
        "size of each generated object",
    ),
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
        "benchmark existing objects under this prefix (empty: generate)",
    ),
    key(
        "persist",
        Section::Workload,
        &["persist_dataset"],
        "keep the generated dataset (on/off)",
    ),
    key(
        "verify",
        Section::Workload,
        &[],
        "CRC32C verification (on/off)",
    ),
    key(
        "cleanup",
        Section::Workload,
        &[],
        "delete test data at the end (on/off)",
    ),
    key(
        "seed",
        Section::Workload,
        &[],
        "dataset and key-choice seed",
    ),
    key(
        "mix",
        Section::Workload,
        &[],
        "operation mix as JSON ([{\"op\":\"read\",\"weight\":1}]), or empty for the preset's",
    ),
    key("endpoint", Section::Target, &[], "custom endpoint URL"),
    key("region", Section::Target, &[], "bucket region"),
    key(
        "max_error_rate",
        Section::Sla,
        &[],
        "SLA: max share of failed operations",
    ),
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
    key("provider", Section::Cost, &[], "pricing preset"),
    key(
        "monthly_ops",
        Section::Cost,
        &[],
        "operations per month for the cost projection",
    ),
    key("stored_gib", Section::Cost, &[], "stored data in GiB"),
    key(
        "egress_fraction",
        Section::Cost,
        &[],
        "share of downloads leaving the cloud",
    ),
    key(
        "pricing",
        Section::Cost,
        &[],
        "custom price table as JSON, or none",
    ),
    key("label", Section::Run, &[], "label stored with the run"),
];

/// The `--set` key `name` (or an alias; `-` spellings accepted). Unknown keys get a "did you
/// mean" hint.
pub fn set_key(name: &str) -> Result<&'static SetKey, OprobeError> {
    let n = name.trim().to_ascii_lowercase().replace('-', "_");
    if let Some(k) = SET_KEYS
        .iter()
        .find(|k| k.name == n || k.aliases.contains(&n.as_str()))
    {
        return Ok(k);
    }
    let names: Vec<&str> = SET_KEYS.iter().map(|k| k.name).collect();
    let hint = match input::suggest(&n, names.iter().copied()) {
        Some(s) => format!("did you mean `{s}`? keys: {}", names.join(", ")),
        None => format!("keys: {}", names.join(", ")),
    };
    Err(input::invalid(format!("unknown --set key `{}`", name.trim())).with_hint(hint))
}

/// Validate `key=value` (without a profile to apply it to).
pub fn check_set(k: &str, v: &str) -> Result<(), OprobeError> {
    let key = set_key(k)?;
    let mut p = Profile::from_target(TargetSpec {
        url: Some("memory://".into()),
        scheme: "memory".into(),
        ..Default::default()
    });
    let mut label = None;
    apply_one(&mut p, &mut label, key, v)
}

fn none_word(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "" | "none" | "off" | "unset"
    )
}

/// Apply one validated key to `p`.
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
    let k = &mut p.knobs;
    match key.name {
        "concurrency" => k.concurrency = usize_of(1)?,
        "io_concurrency" => k.io_concurrency = usize_of(1)?,
        "chunk_size" => {
            let s = input::size(v)?;
            if s.0 < 4096 {
                return Err(input::invalid(format!("{what} must be >= 4KiB (got {v})")));
            }
            k.chunk_size = s;
        }
        "range_gap" => k.range_gap = input::size(v)?,
        "max_inflight_requests" => k.max_inflight_requests = usize_of(0)?,
        "http_version" => k.http_version = input::http_version(v)?,
        "transport" => {
            k.transport = input::choice(
                "transport",
                v,
                &[
                    ("opendal", TransportMode::Opendal),
                    ("microscope", TransportMode::Microscope),
                ],
            )?
        }
        "keep_alive" => k.keep_alive = input::boolean(&what, v)?,
        "cache" => match input::boolean(&what, v) {
            Ok(b) => k.cache = b,
            Err(_) => {
                k.cache_size = input::nonzero_size(v)?;
                k.cache = true;
            }
        },
        "cache_size" => k.cache_size = input::nonzero_size(v)?,
        "retries" => k.retries = usize_of(0)?,
        "tail_cut" => k.tail_cut = input::boolean(&what, v)?,
        "timeout" => k.timeout = HumanDuration(input::positive_duration(v)?),
        "addressing" => k.provider.addressing = input::addressing(v)?,
        "checksum" => {
            k.provider.checksum = (!none_word(v)).then(|| v.to_owned());
            k.provider.options("s3").map_err(to_usage)?;
        }
        "sse" => {
            k.provider.sse = (!none_word(v)).then(|| v.to_owned());
            k.provider.options("s3").map_err(to_usage)?;
        }
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
        "objects" => {
            let n = usize_of(1)?;
            dataset(p, &|d| d.objects = n);
        }
        "object_size" => {
            let s = input::nonzero_size(v)?;
            dataset(p, &|d| d.object_size = s);
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
        "endpoint" => {
            if none_word(v) {
                p.target.options.remove("endpoint");
            } else {
                p.target
                    .options
                    .insert("endpoint".into(), input::endpoint(v)?);
            }
        }
        "region" => {
            if none_word(v) {
                p.target.options.remove("region");
            } else {
                p.target.options.insert("region".into(), v.to_owned());
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
        "provider" => {
            p.cost.provider = if none_word(v) {
                None
            } else {
                Some(input::provider(v)?.to_owned())
            }
        }
        "monthly_ops" => p.cost.monthly_ops = input::non_negative(&what, v)?,
        "stored_gib" => p.cost.stored_gib = input::non_negative(&what, v)?,
        "egress_fraction" => {
            let f = input::share(&what, v)?;
            p.cost.egress_fraction = f;
        }
        "pricing" => {
            p.cost.pricing = if none_word(v) {
                None
            } else {
                Some(serde_json::from_str(v).map_err(|e| {
                    input::invalid(format!("invalid {what}: {e}"))
                        .with_hint("a JSON object with the fields of [cost.pricing]")
                })?)
            }
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

fn to_usage(e: anyhow::Error) -> OprobeError {
    let b = crate::errors::classify(&e, "s3");
    let mut o = OprobeError::new(b.code, b.message);
    o.hint = b.hint;
    o
}

/// A change to a run: `--set key=value` pairs grouped by the part of the profile they change
/// (canonical key names, see [`SET_KEYS`]). Built with [`Overrides::set`], which validates.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Overrides {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub knobs: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workload: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub target: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sla: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cost: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Overrides {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `key` (canonical name or alias) to `value`, validated.
    pub fn set(&mut self, key: &str, value: &str) -> Result<&mut Self, OprobeError> {
        let k = set_key(key)?;
        check_set(k.name, value)?;
        let v = value.trim().to_owned();
        match k.section {
            Section::Knobs => self.knobs.insert(k.name.into(), v),
            Section::Workload => self.workload.insert(k.name.into(), v),
            Section::Target => self.target.insert(k.name.into(), v),
            Section::Sla => self.sla.insert(k.name.into(), v),
            Section::Cost => self.cost.insert(k.name.into(), v),
            Section::Run => {
                self.label = Some(v);
                None
            }
        };
        Ok(self)
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
    /// override given with it survives the preset's reset).
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = Vec::new();
        if let Some(p) = self.workload.get("preset") {
            v.push(("preset".into(), p.clone()));
        }
        for m in [
            &self.workload,
            &self.knobs,
            &self.target,
            &self.sla,
            &self.cost,
        ] {
            for (k, val) in m {
                if k != "preset" {
                    v.push((k.clone(), val.clone()));
                }
            }
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
            apply_one(p, label, set_key(&k)?, &v)?;
        }
        Ok(())
    }

    /// A one-line summary: `concurrency=8 tail_cut=on`.
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

    fn lab_profile() -> Profile {
        let mut t = TargetSpec::from_url("s3://bkt/x").unwrap();
        t.options
            .insert("endpoint".into(), "https://minio.local:9000".into());
        t.options.insert("region".into(), "eu-west-1".into());
        t.options
            .insert("secret_access_key".into(), "hunter2".into());
        t.options
            .insert("access_key_id".into(), "${MY_KEY_ID}".into());
        let mut p = Profile::from_target(t);
        p.workload.preset = crate::config::Preset::SmallFiles;
        p.workload.duration = HumanDuration::secs(12);
        p.workload.warmup = HumanDuration(std::time::Duration::from_millis(500));
        p.workload.seed = 7;
        p.workload.max_ops = Some(900);
        p.workload.rate = Some(250.0);
        p.workload.access = Access::Zipf(1.2);
        p.workload.dataset = Some(DatasetSpec {
            objects: 64,
            object_size: ByteSize::kib(64),
            partitions: 2,
            ..Default::default()
        });
        p.workload.mix = vec![OpSpec {
            weight: 3,
            op: crate::config::OpKind::Read { size: None },
        }];
        p.knobs.concurrency = 24;
        p.knobs.io_concurrency = 3;
        p.knobs.http_version = crate::net::HttpVersion::H1;
        p.knobs.tail_cut = true;
        p.knobs.timeout = HumanDuration::secs(9);
        p.sla.max_throttle_rate = Some(0.01);
        p.cost.provider = Some("aws-s3".into());
        p.name = Some("base".into());
        p
    }

    /// Parse a printed command with the CLI and build its profile.
    fn replay(cmd: &str) -> Profile {
        use clap::Parser;
        let words = shell_words(cmd);
        assert_eq!(words[0], "oprobe");
        let cli =
            crate::cli::args::Cli::try_parse_from(&words).unwrap_or_else(|e| panic!("{cmd}: {e}"));
        let Some(crate::cli::args::Command::Run(a)) = cli.command else {
            panic!("not a run: {cmd}");
        };
        let mut p = crate::cli::build_profile(&a.target, &a.workload, &a.knobs, &a.cost).unwrap();
        crate::cli::apply_sla(&mut p, &a.sla).unwrap();
        let mut label = a.output.label.clone();
        Overrides::parse(&a.set)
            .unwrap()
            .apply(&mut p, &mut label)
            .unwrap();
        p.name = label;
        p
    }

    /// Minimal POSIX word splitting with single quotes (what `command_line` produces).
    fn shell_words(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut q = false;
        let mut any = false;
        for c in s.chars() {
            match (c, q) {
                ('\'', _) => {
                    q = !q;
                    any = true;
                }
                (' ', false) => {
                    if any || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    any = false;
                }
                (c, _) => cur.push(c),
            }
        }
        if any || !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    #[test]
    fn secrets_are_redacted_and_url_implied_options_dropped() {
        let s = RunSpec::new(&lab_profile(), "s3://bkt/x");
        let o = &s.profile.target.options;
        assert_eq!(o["secret_access_key"], REDACTED);
        assert_eq!(o["access_key_id"], "${MY_KEY_ID}");
        assert!(
            !o.contains_key("bucket") && !o.contains_key("root"),
            "{o:?}"
        );
        assert_eq!(s.seed, 7);
        assert_eq!(s.label.as_deref(), Some("base"));
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
        // lab:// targets keep nothing of the ephemeral server.
        let lab = Profile::from_target(TargetSpec::from_url("lab://demo").unwrap());
        let s = RunSpec::new(&lab, "lab://demo");
        assert!(
            s.profile.target.options.is_empty(),
            "{:?}",
            s.profile.target
        );
        assert!(!s.command().contains("127.0.0.1"), "{}", s.command());
        assert!(!s.command().contains("--proxy"), "{}", s.command());
    }

    #[test]
    fn command_replays_every_setting() {
        let p = lab_profile();
        let s = RunSpec::new(&p, "s3://bkt/x");
        let cmd = s.command();
        let back = replay(&cmd);
        // Same workload, knobs, SLA, cost and label; the secret is not in the command.
        assert!(!cmd.contains("hunter2"), "{cmd}");
        assert_eq!(back.workload, p.workload, "{cmd}");
        assert_eq!(back.knobs, p.knobs, "{cmd}");
        assert_eq!(back.sla, p.sla, "{cmd}");
        assert_eq!(back.cost, p.cost, "{cmd}");
        assert_eq!(back.name, p.name, "{cmd}");
        assert_eq!(back.target.options["endpoint"], "https://minio.local:9000");
        assert_eq!(back.target.options["region"], "eu-west-1");
        assert_eq!(RunSpec::new(&back, "s3://bkt/x").hash(), s.hash());
        // A default profile prints a short command.
        let d = RunSpec::new(
            &Profile::from_target(TargetSpec::from_url("memory://").unwrap()),
            "memory://",
        );
        assert_eq!(
            d.command(),
            "oprobe run memory:// --preset parquet --duration 10s --concurrency 16"
        );
    }

    #[test]
    fn overrides_change_only_what_they_name() {
        let s = RunSpec::new(&lab_profile(), "s3://bkt/x");
        let o = Overrides::new()
            .with_set("concurrency", "48")
            .with_set("tail_cut", "off")
            .with_set("warmup", "2s");
        assert_eq!(o.knobs["concurrency"], "48");
        assert_eq!(o.workload["warmup"], "2s");
        let n = s.with(&o);
        let (a, b) = (s.resolved_profile(), n.resolved_profile());
        assert_eq!(b.knobs.concurrency, 48);
        assert!(!b.knobs.tail_cut);
        assert_eq!(b.workload.warmup, HumanDuration::secs(2));
        assert_eq!(b.workload.dataset, a.workload.dataset);
        assert_eq!(b.workload.mix, a.workload.mix);
        assert_eq!(b.workload.seed, 7);
        assert_eq!(b.target, a.target);
        assert_ne!(n.hash(), s.hash());
        assert_ne!(n.workload_hash(), s.workload_hash(), "warmup is workload");
        let knob_only = s.with(&Overrides::new().with_set("c", "8"));
        assert_eq!(knob_only.workload_hash(), s.workload_hash());
        assert_ne!(knob_only.hash(), s.hash());
        // Label and seed are not part of the hash.
        let relabel = s.with(
            &Overrides::new()
                .with_set("label", "x")
                .with_set("seed", "9"),
        );
        assert_eq!(relabel.hash(), s.hash());
        assert_eq!(relabel.seed, 9);
        // The command of the changed spec replays the change.
        let back = replay(&n.command());
        assert_eq!(back.knobs.concurrency, 48);
        assert_eq!(back.workload.mix, a.workload.mix);
        // A preset override resets dataset and mix, unless given with it.
        let pr = s.with(&Overrides::new().with_set("preset", "parquet"));
        assert!(pr.profile.workload.mix.is_empty() && pr.profile.workload.dataset.is_none());
        let pr = s.with(
            &Overrides::new()
                .with_set("objects", "5")
                .with_set("preset", "parquet"),
        );
        assert_eq!(
            pr.resolved_profile().workload.effective_dataset().objects,
            5
        );
        assert_eq!(
            RunSpec::rerun_from("20261121-101010-abcd", &o),
            "oprobe run --from 20261121-101010-abcd --set warmup=2s --set concurrency=48 --set tail_cut=off"
        );
    }

    #[test]
    fn set_keys_are_validated_with_hints() {
        let e = Overrides::parse(&["concurency=8"]).unwrap_err();
        assert_eq!(e.code, ErrorCode::UsageInvalidArgument);
        assert!(e.hint.unwrap().contains("did you mean `concurrency`"));
        for bad in [
            "concurrency=0",
            "warmup=5x",
            "preset=nope",
            "access=weird",
            "provider=nope",
            "mix=[",
            "endpoint=ftp://x",
            "chunk_size=1",
            "novalue",
        ] {
            let e = Overrides::parse(&[bad]).unwrap_err();
            assert_eq!(e.code, ErrorCode::UsageInvalidArgument, "{bad}");
        }
        let o = Overrides::parse(&["http=h2", "max-inflight=4", "provider=aws", "cache=128MiB"])
            .unwrap();
        assert_eq!(o.knobs["http_version"], "h2");
        assert_eq!(o.knobs["max_inflight_requests"], "4");
        assert_eq!(o.cost["provider"], "aws");
        let s = RunSpec::default().with(&o);
        assert_eq!(s.profile.cost.provider.as_deref(), Some("aws-s3"));
        assert!(s.profile.knobs.cache);
        assert_eq!(s.profile.knobs.cache_size, ByteSize::mib(128));
        // Every key has a setter and accepts a plausible value.
        for k in SET_KEYS {
            let v = match k.name {
                "mix" => r#"[{"op":"stat"}]"#,
                "pricing" => "none",
                "endpoint" => "http://127.0.0.1:9000",
                "transport" => "microscope",
                "http_version" => "h2",
                "addressing" => "path",
                "checksum" => "crc32c",
                "sse" => "AES256",
                "preset" => "mixed",
                "access" => "uniform",
                "provider" => "gcs",
                "sla" => "none",
                "region" | "existing" | "label" => "x",
                "keep_alive" | "tail_cut" | "persist" | "verify" | "cleanup" | "cache" => "on",
                "chunk_size" | "range_gap" | "object_size" | "cache_size" => "8MiB",
                "duration" | "warmup" | "timeout" => "3s",
                "max_error_rate" | "max_throttle_rate" | "egress_fraction" => "1%",
                _ => "4",
            };
            check_set(k.name, v).unwrap_or_else(|e| panic!("{}={v}: {e}", k.name));
        }
    }
}
