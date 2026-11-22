//! From what the user typed (the New Run sheet) or a stored run's spec (Re-run, Verify, the
//! tuner's "Run with these settings") to a run request, exactly as the CLI resolves them:
//! configured target names, `[defaults]`, `lab://scenario` targets, and `oprobe run --from`
//! for a spec (design v4 §1, VER-1).
//!
//! A form opened on a run edits that run's [`RunSpec`]: the fields it shows become
//! [`Overrides`] of the spec, and everything it does not show (dataset, mix, warmup, seed,
//! access, rate, HTTP version, tail-cut, `--existing`, `--max-ops`, cost settings, …) is kept.

use crate::app::input::{CostInput, KnobInput, TargetInput, WorkloadInput};
use crate::config::{Profile, TargetSpec};
use crate::model::spec::{Overrides, REDACTED, RunSpec};
use crate::store::{ConfigFile, RunStore};
use crate::ui::bridge::RunRequest;

/// The New Run sheet's values (text as typed), and the spec they edit when the form repeats
/// a run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunForm {
    pub target: String,
    pub preset: String,
    pub duration: String,
    pub concurrency: String,
    pub io_concurrency: String,
    pub chunk: String,
    pub endpoint: String,
    pub region: String,
    pub label: String,
    /// Stored data for the cost projection, GiB (empty: the built-in assumption).
    pub stored_gib: String,
    pub anonymous: bool,
    /// The run this form repeats: the fields above edit it, nothing else is dropped.
    pub spec: Option<RunSpec>,
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn num(s: &str, what: &str) -> Result<Option<usize>, String> {
    match opt(s) {
        None => Ok(None),
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| format!("{what} must be a whole number")),
    }
}

fn err(e: impl std::fmt::Display) -> String {
    format!("{e:#}")
}

/// Target flags the app adds to every run (`oprobe top --ca-cert … --insecure --proxy …`).
fn launch_target_args() -> TargetInput {
    let l = crate::ui::launch();
    TargetInput {
        ca_cert: l.ca_cert,
        insecure: l.insecure,
        proxy: l.proxy,
        ..Default::default()
    }
}

/// Whether `t` carries the anonymous-access options of its scheme.
fn is_anonymous(t: &TargetSpec) -> bool {
    crate::config::anonymous_options(&t.scheme)
        .map(|o| {
            !o.is_empty()
                && o.iter()
                    .all(|(k, v)| t.options.get(*k).map(String::as_str) == Some(*v))
        })
        .unwrap_or(false)
}

/// `oprobe run --from`: the spec's profile with its target resolved again (credentials come
/// from the target or the environment, never from the spec), plus `extra` target flags.
pub fn replay(
    spec: &RunSpec,
    cfg: &ConfigFile,
    extra: &TargetInput,
) -> Result<(Profile, Option<String>), String> {
    let mut ta = extra.clone();
    ta.target = Some(spec.target_ref.clone());
    let (ta, name) = crate::app::profile::resolve_target(&ta, cfg).map_err(err)?;
    let url = ta
        .target
        .clone()
        .ok_or_else(|| "the stored run names no target".to_owned())?;
    let mut p = spec.resolved_profile();
    let mut tgt = TargetSpec::from_url(&url).map_err(err)?;
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
    p.target = tgt;
    crate::app::profile::apply_flags(
        &mut p,
        &ta,
        &WorkloadInput::default(),
        &KnobInput::default(),
        &CostInput::default(),
    )
    .map_err(err)?;
    p.validate().map_err(err)?;
    Ok((p, name))
}

impl RunForm {
    /// The form that repeats a run with spec `spec`: its fields show the spec's values.
    pub fn from_spec(spec: &RunSpec) -> Self {
        let p = spec.resolved_profile();
        let o = &p.target.options;
        let stored = p.cost.stored_gib;
        let default_stored = crate::config::defaults().cost.stored_gib;
        RunForm {
            target: spec.target_ref.clone(),
            preset: p.workload.preset.slug().to_owned(),
            duration: p.workload.duration.to_string(),
            concurrency: p.knobs.concurrency.to_string(),
            io_concurrency: p.knobs.io_concurrency.to_string(),
            chunk: p.knobs.chunk_size.to_string(),
            endpoint: o.get("endpoint").cloned().unwrap_or_default(),
            region: o.get("region").cloned().unwrap_or_default(),
            label: spec.label.clone().unwrap_or_default(),
            stored_gib: if (stored - default_stored).abs() < 1e-9 {
                String::new()
            } else {
                format!("{stored}")
            },
            anonymous: is_anonymous(&p.target),
            spec: Some(spec.clone()),
        }
    }

    /// What the fields changed in the spec they edit, as `--set` overrides.
    pub fn overrides(&self) -> Result<Overrides, String> {
        let Some(spec) = &self.spec else {
            return Ok(Overrides::default());
        };
        let was = Self::from_spec(spec);
        let mut o = Overrides::default();
        let pairs: [(&str, &str, &str); 9] = [
            ("preset", &was.preset, &self.preset),
            ("duration", &was.duration, &self.duration),
            ("concurrency", &was.concurrency, &self.concurrency),
            ("io_concurrency", &was.io_concurrency, &self.io_concurrency),
            ("chunk_size", &was.chunk, &self.chunk),
            ("endpoint", &was.endpoint, &self.endpoint),
            ("region", &was.region, &self.region),
            ("label", &was.label, &self.label),
            ("stored_gib", &was.stored_gib, &self.stored_gib),
        ];
        for (key, before, now) in pairs {
            if before.trim() != now.trim() {
                let v = if now.trim().is_empty() {
                    "none"
                } else {
                    now.trim()
                };
                o.set(key, v).map_err(|e| e.message.clone())?;
            }
        }
        Ok(o)
    }

    /// The spec this form runs: the edited spec of the run it repeats.
    pub fn to_spec(&self) -> Result<Option<RunSpec>, String> {
        let Some(spec) = &self.spec else {
            return Ok(None);
        };
        let mut next = spec
            .try_with(&self.overrides()?)
            .map_err(|e| e.message.clone())?;
        if let Some(t) = opt(&self.target)
            && t != next.target_ref
        {
            next.target_ref = t;
        }
        Ok(Some(next))
    }

    /// Resolve into a request for the engine.
    pub fn prepare(
        &self,
        cfg: &ConfigFile,
        store: Option<RunStore>,
        trace_max_requests: usize,
    ) -> Result<RunRequest, String> {
        let mut extra = launch_target_args();
        let (profile, target_name) = match self.to_spec()? {
            Some(spec) => {
                extra.anonymous = self.anonymous;
                replay(&spec, cfg, &extra)?
            }
            None => self.fresh_profile(cfg, extra)?,
        };
        let mut budgets = crate::guard::Budgets::default();
        cfg.defaults.budgets.apply(&mut budgets).map_err(err)?;
        Ok(RunRequest {
            budgets,
            profile,
            target_name,
            store,
            trace_max_requests,
        })
    }

    /// A new run from the fields alone (flags and `[defaults]`, as `oprobe run` builds it).
    fn fresh_profile(
        &self,
        cfg: &ConfigFile,
        extra: TargetInput,
    ) -> Result<(Profile, Option<String>), String> {
        let t = TargetInput {
            target: opt(&self.target),
            endpoint: opt(&self.endpoint),
            region: opt(&self.region),
            anonymous: self.anonymous,
            ..extra
        };
        let (t, target_name) = crate::app::profile::resolve_target(&t, cfg).map_err(err)?;
        let d = &cfg.defaults;
        let w = WorkloadInput {
            preset: opt(&self.preset).or_else(|| d.preset.clone()),
            duration: opt(&self.duration).or_else(|| d.duration.clone()),
            ..Default::default()
        };
        let k = KnobInput {
            concurrency: num(&self.concurrency, "Concurrency")?.or(d.concurrency),
            io_concurrency: num(&self.io_concurrency, "I/O concurrency")?,
            chunk_size: opt(&self.chunk),
            ..Default::default()
        };
        let stored_gib = match opt(&self.stored_gib) {
            None => None,
            Some(v) => Some(
                v.parse::<f64>()
                    .ok()
                    .filter(|g| g.is_finite() && *g >= 0.0)
                    .ok_or_else(|| "Stored data must be a number of GiB".to_owned())?,
            ),
        };
        let c = CostInput {
            stored_gib,
            ..Default::default()
        };
        let mut profile = crate::app::profile::build_profile(&t, &w, &k, &c).map_err(err)?;
        if let Some(l) = opt(&self.label) {
            profile.name = Some(l);
        }
        Ok((profile, target_name))
    }
}

/// Shell-like words: whitespace separated, `'…'` and `"…"` quote.
pub fn split_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                any = true;
            }
            None if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            None => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_targets_resolve_without_credentials() {
        let f = RunForm {
            target: "lab://demo".into(),
            duration: "1s".into(),
            ..Default::default()
        };
        let req = f.prepare(&ConfigFile::default(), None, 1000).unwrap();
        assert_eq!(req.profile.target.url.as_deref(), Some("lab://demo"));
        assert!(
            RunForm {
                target: "lab://nope".into(),
                ..Default::default()
            }
            .prepare(&ConfigFile::default(), None, 1)
            .is_err()
        );
    }

    #[test]
    fn bad_numbers_are_reported() {
        let f = RunForm {
            target: "memory://".into(),
            concurrency: "lots".into(),
            ..Default::default()
        };
        let e = f.prepare(&ConfigFile::default(), None, 1).err().unwrap();
        assert!(e.contains("Concurrency"), "{e}");
    }

    #[test]
    fn words() {
        assert_eq!(
            split_words("oprobe run 's3://a b' --concurrency 8"),
            vec!["oprobe", "run", "s3://a b", "--concurrency", "8"]
        );
    }

    /// A spec with everything the 3.x App used to drop when re-running.
    fn rich_spec() -> RunSpec {
        let mut p = Profile::from_target(TargetSpec::from_url("s3://bench/oprobe").unwrap());
        p.target
            .options
            .insert("endpoint".into(), "https://minio.local:9000".into());
        p.target.options.insert("region".into(), "eu-west-1".into());
        p.target
            .options
            .insert("secret_access_key".into(), "hunter2".into());
        p.workload.preset = crate::config::Preset::SmallFiles;
        p.workload.duration = crate::util::HumanDuration::secs(7);
        p.workload.warmup = crate::util::HumanDuration::secs(3);
        p.workload.rate = Some(250.0);
        p.workload.max_ops = Some(5000);
        p.workload.seed = 4242;
        p.workload.dataset = Some(crate::config::DatasetSpec {
            objects: 999,
            object_size: crate::util::ByteSize::kib(48),
            ..Default::default()
        });
        p.knobs.concurrency = 12;
        p.knobs.http_version = crate::net::HttpVersion::H2;
        p.cost.stored_gib = 5.0;
        p.name = Some("nightly".into());
        RunSpec::new(&p, "s3://bench/oprobe")
    }

    #[test]
    fn a_spec_round_trips_through_the_form_without_loss() {
        let spec = rich_spec();
        let f = RunForm::from_spec(&spec);
        assert_eq!(f.endpoint, "https://minio.local:9000");
        assert_eq!(f.region, "eu-west-1");
        assert_eq!(f.preset, "small-files");
        assert_eq!(f.duration, "7s");
        assert_eq!(f.concurrency, "12");
        assert_eq!(f.label, "nightly");
        assert_eq!(f.stored_gib, "5");
        // Untouched: the very same spec (nothing dropped, nothing added).
        assert!(f.overrides().unwrap().is_empty());
        assert_eq!(f.to_spec().unwrap().unwrap(), spec);
        // One edit: only that key changes.
        let mut g = f.clone();
        g.concurrency = "6".into();
        assert_eq!(
            g.overrides().unwrap().entries(),
            vec![("concurrency".to_owned(), "6".to_owned())]
        );
        let edited = g.to_spec().unwrap().unwrap();
        let (a, b) = (spec.resolved_profile(), edited.resolved_profile());
        assert_eq!(b.knobs.concurrency, 6);
        assert_eq!(a.workload, b.workload);
        assert_eq!(a.target, b.target);
        assert_eq!(a.cost, b.cost);
        assert_eq!(b.knobs.http_version, crate::net::HttpVersion::H2);
        assert_eq!(edited.workload_hash(), spec.workload_hash());
        assert_eq!(edited.seed, 4242);
        // Back into a form: the edit shows, the rest is as it was.
        let back = RunForm::from_spec(&edited);
        assert_eq!(back.concurrency, "6");
        assert_eq!(back.endpoint, f.endpoint);
        assert!(g.clone().with_bad("concurrency", "x").overrides().is_err());
    }

    #[test]
    fn a_replay_keeps_the_workload_and_resolves_credentials_again() {
        let spec = rich_spec();
        let f = RunForm::from_spec(&spec);
        let req = f.prepare(&ConfigFile::default(), None, 10).unwrap();
        let p = &req.profile;
        assert_eq!(p.workload.warmup, crate::util::HumanDuration::secs(3));
        assert_eq!(p.workload.rate, Some(250.0));
        assert_eq!(p.workload.max_ops, Some(5000));
        assert_eq!(p.workload.seed, 4242);
        assert_eq!(p.workload.effective_dataset().objects, 999);
        assert_eq!(
            p.target.options.get("endpoint").map(String::as_str),
            Some("https://minio.local:9000")
        );
        // The redacted secret is not replayed.
        assert_ne!(
            p.target
                .options
                .get("secret_access_key")
                .map(String::as_str),
            Some(REDACTED)
        );
        assert_eq!(p.cost.stored_gib, 5.0);
    }

    impl RunForm {
        fn with_bad(mut self, k: &str, v: &str) -> Self {
            if k == "concurrency" {
                self.concurrency = v.into();
            }
            self
        }
    }
}
