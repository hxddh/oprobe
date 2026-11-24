//! Profile assembly: the `OPERATOR` argument, `[defaults]`, the workload flags and `--set`
//! pairs into one validated [`Profile`] (shared by every command and front end).

use anyhow::Result;

use super::Ctx;
use super::input::*;
use crate::config::{DatasetSpec, Profile};

use crate::operator::resolve::OperatorSource;
use crate::util::HumanDuration;

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

/// Apply `[defaults]` for workload values not given on the command line.
fn apply_defaults(w: &mut WorkloadInput, d: &crate::store::Defaults) {
    if w.preset.is_none() {
        w.preset = d.preset.clone();
    }
    if w.duration.is_none() {
        w.duration = d.duration.clone();
    }
    if w.concurrency.is_none() {
        w.concurrency = d.concurrency;
    }
}

/// Apply the workload flags that were given to `p` (unset flags leave `p` alone). Not
/// validated.
pub fn apply_workload(p: &mut Profile, w: &WorkloadInput) -> Result<()> {
    if let Some(c) = w.concurrency {
        if c == 0 {
            return Err(crate::input::invalid("-c/--concurrency must be at least 1").into());
        }
        p.workload.concurrency = c;
    }
    if let Some(s) = w.seed {
        p.workload.seed = s;
    }
    let wl = &mut p.workload;
    if let Some(pr) = &w.preset {
        wl.preset = crate::input::preset(pr)?;
        wl.mix.clear();
        wl.dataset = None;
    }
    if let Some(d) = &w.duration {
        wl.duration = HumanDuration(crate::input::positive_duration(d)?);
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

/// The profile `oprobe run` measures: the operator (a URL, an `op.toml` file or a configured
/// name), `[defaults]`, the workload flags, then the `--set` pairs. Returns where the operator
/// came from.
pub fn run_profile(ctx: &Ctx, a: &ProfileRequest) -> Result<(Profile, OperatorSource)> {
    let cfg = ctx.config()?;
    let (operator, source) = crate::operator::resolve::resolve(&a.operator, cfg)?;
    let mut p = Profile::from_operator(operator);
    let mut w = a.workload.clone();
    apply_defaults(&mut w, &cfg.defaults);
    apply_workload(&mut p, &w)?;
    apply_sla(&mut p, &a.sla)?;
    if a.label.is_some() {
        p.name = a.label.clone();
    }
    let sets = crate::model::Overrides::parse(&a.set)?;
    let mut label = p.name.clone();
    sets.apply(&mut p, &mut label)?;
    p.name = label;
    p.validate()?;
    Ok((p, source))
}

/// The profile replaying `spec` (a stored run's): its operator with the redacted secrets
/// resolved again from the operator reference (configured operator or URL), credentials
/// coming from the environment as for any run.
pub fn spec_profile(ctx: &Ctx, spec: crate::model::RunSpec) -> Result<Profile> {
    use crate::model::spec::REDACTED;
    let mut p = spec.resolved_profile();
    // Secrets were redacted: take them from where the operator came from when that still
    // resolves (a configured operator, an operator file), else drop them (the environment's
    // credentials apply).
    let known = crate::operator::resolve::resolve(&spec.operator_ref, ctx.config()?)
        .ok()
        .map(|(o, _)| o.options);
    p.operator.options = p
        .operator
        .options
        .into_iter()
        .filter_map(|(k, v)| {
            if v == REDACTED {
                known
                    .as_ref()
                    .and_then(|o| o.get(&k).cloned())
                    .map(|v| (k, v))
            } else {
                Some((k, v))
            }
        })
        .collect();
    Ok(p)
}
