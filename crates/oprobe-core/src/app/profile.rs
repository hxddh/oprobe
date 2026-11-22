//! Profile assembly: target names, `[defaults]`, flag groups, `--from` replays and `--set`
//! pairs into one validated [`Profile`] (shared by every command and front end).

use anyhow::Result;

use super::Ctx;
use super::input::*;
use crate::config::{DatasetSpec, Profile, TargetSpec};
use crate::engine::RunOptions;
use crate::errors::{ErrorCode, OprobeError};
use crate::store::ConfigFile;
use crate::util::{ByteSize, HumanDuration};

// ---------------------------------------------------------------------------------------
// Profile assembly
// ---------------------------------------------------------------------------------------

/// Resolve a target *name* (from the config file) into URL arguments; URLs pass through.
/// Flags given on the command line override the target's values. Returns the target name.
pub fn resolve_target(t: &TargetInput, cfg: &ConfigFile) -> Result<(TargetInput, Option<String>)> {
    let mut t = t.clone();
    if t.profile.is_some() {
        return Ok((t, None));
    }
    let given = t.target.clone().or_else(|| cfg.defaults.target.clone());
    let Some(given) = given else {
        return Ok((t, None));
    };
    if given.contains("://") {
        t.target = Some(given);
        return Ok((t, None));
    }
    let Some(entry) = cfg.target(&given) else {
        let known: Vec<&str> = cfg.targets.keys().map(String::as_str).collect();
        return Err(OprobeError::new(
            ErrorCode::UsageUnknownTarget,
            format!("unknown target `{given}`"),
        )
        .with_hint(if known.is_empty() {
            "pass a storage URL (s3://bucket/prefix, memory://) or add a target with `oprobe target add NAME URL`".to_owned()
        } else {
            format!("configured targets: {}", known.join(", "))
        })
        .into());
    };
    entry.validate()?;
    crate::input::storage_url(&entry.url).map_err(|e| {
        let mut o = OprobeError::new(
            ErrorCode::ConfigInvalid,
            format!("target `{given}` in the config file: {}", e.message),
        );
        o.hint = e
            .hint
            .map(|h| format!("{h}; fix it with `oprobe config edit`"));
        o
    })?;
    t.target = Some(entry.url.clone());
    if t.endpoint.is_none() {
        t.endpoint = entry.endpoint.clone();
    }
    if t.region.is_none() {
        t.region = entry.region.clone();
    }
    let mut opts: Vec<String> = entry
        .options
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    opts.append(&mut t.options);
    t.options = opts;
    if entry.credentials.as_deref() == Some("anonymous") {
        t.anonymous = true;
    }
    // `credentials = "profile:NAME"`: the AWS shared-config profile (S3-compatible targets).
    if let Some(p) = entry
        .credentials
        .as_deref()
        .and_then(|c| c.strip_prefix("profile:"))
        && !t.options.iter().any(|o| o.starts_with("profile="))
        && TargetSpec::from_url(&entry.url).is_ok_and(|s| s.scheme == "s3")
    {
        t.options.insert(0, format!("profile={p}"));
    }
    Ok((t, Some(given)))
}

/// Apply `[defaults]` for workload/knob values not given on the command line.
fn apply_defaults(w: &mut WorkloadInput, k: &mut KnobInput, cfg: &ConfigFile) {
    let d = &cfg.defaults;
    if w.preset.is_none() {
        w.preset = d.preset.clone();
    }
    if w.duration.is_none() {
        w.duration = d.duration.clone();
    }
    if k.concurrency.is_none() {
        k.concurrency = d.concurrency;
    }
}

/// [`build_profile`] with configured targets and `[defaults]` applied.
pub fn resolve_profile(
    ctx: &Ctx,
    t: &TargetInput,
    w: &WorkloadInput,
    k: &KnobInput,
    c: &CostInput,
) -> Result<(Profile, Option<String>)> {
    let cfg = ctx.config()?;
    let (t, name) = resolve_target(t, cfg)?;
    let (mut w, mut k) = (w.clone(), k.clone());
    if t.profile.is_none() {
        apply_defaults(&mut w, &mut k, cfg);
    }
    Ok((build_profile(&t, &w, &k, c)?, name))
}

/// Budgets of the safety flags, unset ones from `[defaults.budgets]`.
pub fn safety_budgets(ctx: &Ctx, s: &Safety) -> Result<crate::guard::Budgets> {
    super::run::budgets(ctx, s)
}

/// Trace rows kept per run: `$OPROBE_MAX_REQUESTS` > `[defaults] max_requests` > built-in.
pub fn trace_max_requests(ctx: &Ctx) -> usize {
    std::env::var("OPROBE_MAX_REQUESTS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .or_else(|| ctx.config().ok().and_then(|c| c.defaults.max_requests))
        .unwrap_or(crate::trace::DEFAULT_MAX_REQUESTS)
}

fn parse_size(flag: &str, v: &str) -> Result<ByteSize> {
    crate::input::size(v).map_err(|e| {
        let mut o = OprobeError::new(e.code, format!("invalid {flag}: {}", e.message));
        o.hint = e.hint;
        o.into()
    })
}

/// The profile of flags (or a `--profile` file with flags on top), validated.
pub fn build_profile(
    t: &TargetInput,
    w: &WorkloadInput,
    k: &KnobInput,
    c: &CostInput,
) -> Result<Profile> {
    let mut p = match (&t.profile, &t.target) {
        (Some(path), _) => Profile::load(path)?,
        (None, Some(url)) => Profile::from_target(TargetSpec::from_url(url)?),
        (None, None) => {
            return Err(OprobeError::new(ErrorCode::UsageUnknownTarget, "no target given")
                .with_hint(
                    "try `oprobe run memory://` for a local demo, `oprobe run s3://bucket/prefix`, \
                     a configured target name, or `oprobe lab` to start a local S3 server",
                )
                .into());
        }
    };
    if t.profile.is_some()
        && let Some(url) = &t.target
    {
        let mut nt = TargetSpec::from_url(url)?;
        nt.insecure = p.target.insecure;
        nt.ca_cert = p.target.ca_cert.clone();
        nt.proxy = p.target.proxy.clone();
        p.target = nt;
    }
    apply_flags(&mut p, t, w, k, c)?;
    p.validate()?;
    Ok(p)
}

/// Apply the target, workload, knob and cost flags that were given to `p` (unset flags leave
/// `p` alone): `build_profile` and `run --from` share it. Not validated.
pub fn apply_flags(
    p: &mut Profile,
    t: &TargetInput,
    w: &WorkloadInput,
    k: &KnobInput,
    c: &CostInput,
) -> Result<()> {
    let tg = &mut p.target;
    if let Some(e) = &t.endpoint {
        let e = crate::input::endpoint(e)?;
        tg.options.insert("endpoint".into(), e.clone());
        if tg.scheme == "s3" || tg.url.as_deref().is_some_and(|u| u.starts_with("s3")) {
            tg.options
                .entry("enable_virtual_host_style".into())
                .or_insert_with(|| "false".into());
        }
    }
    if let Some(r) = &t.region {
        tg.options.insert("region".into(), r.clone());
    }
    for kv in &t.options {
        let (key, val) = crate::input::key_value("--option", kv)?;
        tg.options.insert(key, val);
    }
    if t.anonymous {
        let scheme = tg.service_scheme()?;
        for (k, v) in crate::config::anonymous_options(&scheme)? {
            tg.options.insert((*k).into(), (*v).into());
        }
    }
    if let Some(ca) = &t.ca_cert {
        tg.ca_cert = Some(ca.clone());
    }
    if t.insecure {
        tg.insecure = true;
    }
    if let Some(px) = &t.proxy {
        tg.proxy = Some(px.clone());
    }

    let wl = &mut p.workload;
    if let Some(pr) = &w.preset {
        wl.preset = crate::input::preset(pr)?;
        wl.mix.clear();
        wl.dataset = None;
    }
    if let Some(d) = &w.duration {
        wl.duration = HumanDuration(crate::input::duration(d)?);
    }
    if let Some(d) = &w.warmup {
        wl.warmup = HumanDuration(crate::input::duration(d)?);
    }
    if w.max_ops.is_some() {
        wl.max_ops = w.max_ops;
    }
    if let Some(r) = w.rate {
        wl.rate = Some(crate::input::positive("--rate", &r.to_string())?);
    }
    if let Some(a) = &w.access {
        wl.access = crate::input::access(a)?;
    }
    if let Some(a) = &w.arrivals {
        wl.arrivals = a.parse()?;
    }
    if let Some(sh) = &w.shape {
        wl.shape = Some(sh.parse()?);
    }
    if let Some(file) = &w.replay {
        let mut r = crate::engine::replay::ReplaySpec::new(file.clone());
        if let Some(sp) = w.speed {
            r.speed = crate::input::positive("--speed", &sp.to_string())?;
        }
        for m in &w.map_prefix {
            let (from, to) = crate::input::key_value("--map-prefix", m)?;
            r.map_prefix.push((from, to));
        }
        wl.replay = Some(r);
    } else if w.speed.is_some() || !w.map_prefix.is_empty() {
        return Err(crate::input::invalid("--speed and --map-prefix need --replay FILE").into());
    }
    if w.existing.is_some() || w.objects.is_some() || w.object_size.is_some() || w.persist_dataset {
        let mut ds: DatasetSpec = wl.effective_dataset();
        if let Some(e) = &w.existing {
            ds.existing_prefix = Some(e.clone());
        }
        if let Some(n) = w.objects {
            ds.objects = n;
        }
        if let Some(n) = w.objects
            && n == 0
        {
            return Err(crate::input::invalid("--objects must be at least 1").into());
        }
        if let Some(s) = &w.object_size {
            if crate::config::SizeDist::is_dist(s) {
                let dist: crate::config::SizeDist = s.parse()?;
                ds.object_size = crate::util::ByteSize(dist.nominal());
                ds.size_dist = Some(dist);
            } else {
                ds.object_size = crate::input::nonzero_size(s)?;
                ds.size_dist = None;
            }
        }
        if w.persist_dataset {
            ds.persist = true;
        }
        wl.dataset = Some(ds);
    }
    if w.no_verify {
        wl.verify = false;
    }
    if w.keep_data {
        wl.cleanup = false;
    }

    let kn = &mut p.knobs;
    if let Some(v) = k.concurrency {
        if v < 1 {
            return Err(crate::input::invalid("concurrency must be at least 1").into());
        }
        kn.concurrency = v;
    }
    if let Some(v) = k.io_concurrency {
        if v < 1 {
            return Err(crate::input::invalid("I/O concurrency must be at least 1").into());
        }
        kn.io_concurrency = v;
    }
    if let Some(v) = &k.chunk_size {
        kn.chunk_size = parse_size("--chunk-size", v)?;
    }
    if let Some(v) = &k.range_gap {
        kn.range_gap = parse_size("--range-gap", v)?;
    }
    if let Some(v) = k.max_inflight {
        kn.max_inflight_requests = v;
    }
    if let Some(h) = k.http {
        kn.http_version = h;
    }
    if k.no_keep_alive {
        kn.keep_alive = false;
    }
    if k.microscope {
        kn.transport = crate::config::TransportMode::Microscope;
    }
    if let Some(sz) = &k.cache {
        kn.cache = true;
        kn.cache_size = parse_size("--cache", sz)?;
    }
    if let Some(r) = k.retries {
        kn.retries = r;
    }
    if k.tail_cut {
        kn.tail_cut = true;
    }
    if let Some(a) = &k.addressing {
        kn.provider.addressing = crate::input::addressing(a)?;
    }
    if k.checksum.is_some() {
        kn.provider.checksum = k.checksum.clone();
    }
    if k.sse.is_some() {
        kn.provider.sse = k.sse.clone();
    }

    let cs = &mut p.cost;
    if let Some(v) = &c.provider {
        cs.provider = Some(crate::input::provider(v)?.to_owned());
    }
    if let Some(v) = c.monthly_ops {
        cs.monthly_ops = crate::input::non_negative("--monthly-ops", &v.to_string())?;
    }
    if let Some(v) = c.egress_fraction {
        cs.egress_fraction = crate::input::share("--egress-fraction", &v.to_string())?;
    }
    if let Some(v) = c.stored_gib {
        cs.stored_gib = crate::input::non_negative("--stored-gib", &v.to_string())?;
    }
    Ok(())
}

/// A share given as `1%`, `0.5 %` or a fraction (`0.01`).
fn parse_rate(flag: &str, v: &str) -> Result<f64> {
    Ok(crate::input::share(flag, v)?)
}

/// Apply `--sla`, `--max-error-ratio`, `--max-throttle-ratio` and `--p99-ms` to the profile.
pub fn apply_sla(p: &mut Profile, a: &SlaInput) -> Result<()> {
    if let Some(m) = a.sla {
        p.sla.enforce = m == SlaMode::Enforce;
    }
    if let Some(v) = &a.max_error_rate {
        p.sla.max_error_rate = parse_rate("--max-error-ratio", v)?;
    }
    if let Some(v) = &a.max_throttle_rate {
        p.sla.max_throttle_rate = Some(parse_rate("--max-throttle-ratio", v)?);
    }
    if let Some(ms) = a.p99_ms {
        p.sla.p99_ms = Some(crate::input::positive("--p99-ms", &ms.to_string())?);
    }
    p.sla.validate()
}

/// The profile `oprobe run` measures: built from the flags (and `[defaults]`), or — with
/// `--from RUNREF` — the stored run's spec with its target resolved again (credentials come
/// from the target, never from the spec) and the given flags on top; `--set` pairs last.
/// Returns the configured target name.
pub fn run_profile(ctx: &Ctx, a: &ProfileRequest) -> Result<(Profile, Option<String>)> {
    let sets = crate::model::Overrides::parse(&a.set)?;
    let (mut p, name) = match &a.from {
        Some(r) => replay_profile(ctx, r, a)?,
        None => {
            let (mut p, name) = resolve_profile(ctx, &a.target, &a.workload, &a.knobs, &a.cost)?;
            apply_sla(&mut p, &a.sla)?;
            (p, name)
        }
    };
    if a.label.is_some() {
        p.name = a.label.clone();
    }
    let mut label = p.name.clone();
    sets.apply(&mut p, &mut label)?;
    p.name = label;
    p.validate()?;
    Ok((p, name))
}

/// `run --from RUNREF`: the stored spec, the target re-resolved, the flags on top.
fn replay_profile(ctx: &Ctx, r: &str, a: &ProfileRequest) -> Result<(Profile, Option<String>)> {
    let run = ctx.store().load(r)?;
    spec_profile(ctx, run.report.spec, a)
}

/// The profile replaying `spec` (a stored run's, or one in memory): the target re-resolved
/// (credentials from the environment, profile or configured target), `a`'s flags on top.
pub fn spec_profile(
    ctx: &Ctx,
    spec: crate::model::RunSpec,
    a: &ProfileRequest,
) -> Result<(Profile, Option<String>)> {
    use crate::model::spec::REDACTED;
    let mut ta = a.target.clone();
    let same = ta.target.is_none() || ta.target.as_deref() == Some(spec.target_ref.as_str());
    if ta.target.is_none() {
        ta.target = Some(spec.target_ref.clone());
    }
    let (ta, name) = resolve_target(&ta, ctx.config()?)?;
    let url = ta.target.clone().ok_or_else(|| {
        OprobeError::new(
            ErrorCode::UsageUnknownTarget,
            "the stored run names no target",
        )
    })?;
    let mut p = spec.resolved_profile();
    let mut tgt = TargetSpec::from_url(&url)?;
    if same {
        let st = &spec.profile.target;
        for (k, v) in &st.options {
            if v != REDACTED {
                tgt.options.insert(k.clone(), v.clone());
            }
        }
        tgt.insecure = st.insecure;
        tgt.ca_cert = st.ca_cert.clone();
        if st.proxy.is_some() {
            tgt.proxy = st.proxy.clone();
        }
    }
    p.target = tgt;
    apply_flags(&mut p, &ta, &a.workload, &a.knobs, &a.cost)?;
    apply_sla(&mut p, &a.sla)?;
    Ok((p, name))
}

/// The tuner's search settings, as text the user typed (flags or the app's Tune page).
#[derive(Clone, Copy, Debug)]
pub struct TuneSettings<'a> {
    pub trial_duration: &'a str,
    pub trial_warmup: &'a str,
    pub max_trials: usize,
    pub max_concurrency: usize,
    pub cooldown: &'a str,
    /// Throttle budget (unset: `config::defaults().tune.max_throttle_ratio`).
    pub max_throttle_ratio: Option<f64>,
    pub boundary_repeats: usize,
    /// In-flight headroom below the ceiling (unset: `config::defaults().headroom_ratio`).
    pub headroom_ratio: Option<f64>,
    /// Posterior risk of exceeding the throttle budget (unset: 0.05).
    pub risk_ratio: Option<f64>,
}

impl Default for TuneSettings<'static> {
    /// `oprobe tune`'s defaults.
    fn default() -> Self {
        Self {
            trial_duration: tune_defaults::TRIAL_DURATION,
            trial_warmup: tune_defaults::TRIAL_WARMUP,
            max_trials: tune_defaults::MAX_TRIALS,
            max_concurrency: tune_defaults::MAX_CONCURRENCY,
            cooldown: tune_defaults::COOLDOWN,
            max_throttle_ratio: None,
            boundary_repeats: tune_defaults::BOUNDARY_REPEATS,
            headroom_ratio: None,
            risk_ratio: None,
        }
    }
}

/// Validate tuner settings and build the options `oprobe tune` and the app both run.
pub fn tune_options(
    objective: crate::tune::Objective,
    s: &TuneSettings<'_>,
    rate: Option<f64>,
) -> Result<crate::tune::TuneOptions> {
    let dur = |what: &str, v: &str| -> Result<std::time::Duration> {
        crate::input::duration(v).map_err(|e| {
            let mut o = OprobeError::new(e.code, format!("invalid {what}: {}", e.message));
            o.hint = e.hint;
            o.into()
        })
    };
    let duration = dur("trial duration", s.trial_duration)?;
    if duration.is_zero() {
        return Err(crate::input::invalid("the trial duration must be greater than zero").into());
    }
    if s.max_trials < tune_defaults::MIN_TRIALS {
        return Err(crate::input::invalid(format!(
            "max trials must be at least {}",
            tune_defaults::MIN_TRIALS
        ))
        .into());
    }
    if s.max_concurrency < 1 {
        return Err(crate::input::invalid("max concurrency must be at least 1").into());
    }
    let d = crate::config::defaults();
    let max_throttle_ratio = s.max_throttle_ratio.unwrap_or(d.tune.max_throttle_ratio);
    if !(0.0..=1.0).contains(&max_throttle_ratio) {
        return Err(crate::input::invalid("the throttle budget must be within 0..1").into());
    }
    let headroom_ratio = s.headroom_ratio.unwrap_or(d.headroom_ratio);
    if !(0.0..0.95).contains(&headroom_ratio) {
        return Err(crate::input::invalid("the headroom must be within 0..0.95").into());
    }
    let mut o = crate::tune::TuneOptions::new(
        objective,
        RunOptions {
            duration,
            warmup: dur("trial warmup", s.trial_warmup)?,
            max_ops: None,
            rate,
            trace_slow_ops: false,
        },
    );
    o.max_concurrency = s.max_concurrency;
    o.max_trials = s.max_trials;
    o.cooldown = dur("cooldown", s.cooldown)?;
    o.max_throttle_ratio = max_throttle_ratio;
    o.boundary_repeats = s.boundary_repeats.max(1);
    o.headroom_ratio = headroom_ratio;
    if let Some(r) = s.risk_ratio {
        if !(0.0..0.5).contains(&r) {
            return Err(crate::input::invalid("the risk must be within 0..0.5").into());
        }
        o.risk_ratio = r;
    }
    Ok(o)
}
