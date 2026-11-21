//! From what the user typed (the New Run sheet, a re-run, a finding's verification command) to
//! a run request, exactly as the CLI resolves flags: configured target names, `[defaults]`,
//! `lab://scenario` (an in-process fault-injection server).

use std::time::Duration;

use crate::cli::args::{CostArgs, KnobArgs, TargetArgs, WorkloadArgs};
use crate::lab::LabConfig;
use crate::model::Report;
use crate::store::{ConfigFile, RunStore};
use crate::ui::bridge::RunRequest;

/// The New Run sheet's values (text as typed).
#[derive(Clone, Debug, Default)]
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
    pub anonymous: bool,
    /// Knob flags beyond the form's fields (a verification command's).
    pub knobs: Option<KnobArgs>,
}

/// The lab's scenarios: `demo` (throttling + stragglers), `throttle`, `slow`, `clean`.
pub fn lab_faults(scenario: &str) -> Option<LabConfig> {
    let base = LabConfig {
        enable_tls: true,
        enable_http2: false,
        throttle_status_code: 503,
        base_ttfb_delay: Duration::from_millis(2),
        ..Default::default()
    };
    Some(match scenario {
        "" | "demo" => LabConfig {
            straggler_every_n: 23,
            straggler_delay: Duration::from_millis(120),
            throttle_every_n_requests: Some(37),
            ..base
        },
        "throttle" => LabConfig {
            throttle_every_n_requests: Some(9),
            ..base
        },
        "slow" => LabConfig {
            base_ttfb_delay: Duration::from_millis(25),
            straggler_every_n: 11,
            straggler_delay: Duration::from_millis(250),
            ..base
        },
        "clean" => base,
        _ => return None,
    })
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

impl RunForm {
    /// Resolve into a request for the engine.
    pub fn prepare(
        &self,
        cfg: &ConfigFile,
        store: Option<RunStore>,
        trace_max_requests: usize,
    ) -> Result<RunRequest, String> {
        let target = self.target.trim();
        let (lab, display) = match target.strip_prefix("lab://") {
            Some(scn) => {
                let scn = scn.trim_matches('/');
                let f = lab_faults(scn).ok_or_else(|| {
                    format!("unknown lab scenario `{scn}` (demo, throttle, slow, clean)")
                })?;
                let shown = if scn.is_empty() { "demo" } else { scn };
                (Some(f), Some(format!("lab://{shown}")))
            }
            None => (None, None),
        };
        let t = TargetArgs {
            target: if lab.is_some() {
                Some("memory://".into())
            } else {
                opt(target)
            },
            endpoint: opt(&self.endpoint),
            region: opt(&self.region),
            anonymous: self.anonymous,
            ..Default::default()
        };
        let (t, target_name) = if lab.is_some() {
            (t, None)
        } else {
            crate::cli::resolve_target(&t, cfg).map_err(|e| format!("{e:#}"))?
        };
        let mut w = WorkloadArgs {
            preset: opt(&self.preset),
            duration: opt(&self.duration),
            ..Default::default()
        };
        let mut k = self.knobs.clone().unwrap_or_default();
        if let Some(c) = num(&self.concurrency, "Concurrency")? {
            k.concurrency = Some(c);
        }
        if let Some(c) = num(&self.io_concurrency, "I/O concurrency")? {
            k.io_concurrency = Some(c);
        }
        if let Some(c) = opt(&self.chunk) {
            k.chunk_size = Some(c);
        }
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
        let profile = crate::cli::build_profile(&t, &w, &k, &CostArgs::default())
            .map_err(|e| format!("{e:#}"))?;
        let mut budgets = crate::guard::Budgets::default();
        cfg.defaults
            .budgets
            .apply(&mut budgets)
            .map_err(|e| format!("{e:#}"))?;
        Ok(RunRequest {
            budgets,
            profile,
            target_name,
            display,
            label: opt(&self.label),
            lab,
            store,
            trace_max_requests,
        })
    }

    /// The form that repeats `report`'s run (target, workload and knobs).
    pub fn from_report(r: &Report) -> Self {
        let target = r
            .target
            .name
            .clone()
            .unwrap_or_else(|| r.target.display.clone());
        let k = &r.knobs.requested;
        RunForm {
            target,
            preset: r.workload.preset.clone(),
            duration: format!("{}s", r.workload.duration_s),
            concurrency: k.concurrency_count.to_string(),
            io_concurrency: k.io_concurrency_count.to_string(),
            chunk: crate::util::ByteSize(k.chunk_bytes).to_string(),
            label: r.run.label.clone().unwrap_or_default(),
            ..Default::default()
        }
    }

    /// A finding's verification command (`oprobe run [TARGET] --flag …`), on top of the form
    /// that repeats `r` (the run's target when the command names none).
    pub fn from_command(cmd: &str, r: &Report) -> Result<Self, String> {
        let words = split_words(cmd);
        let cli = <crate::cli::Cli as clap::Parser>::try_parse_from(words)
            .map_err(|e| format!("cannot run `{cmd}`: {}", e.kind()))?;
        let Some(crate::cli::args::Command::Run(a)) = cli.command else {
            return Err(format!("`{cmd}` is not an `oprobe run` command"));
        };
        let mut f = Self::from_report(r);
        if let Some(t) = a.target.target {
            f.target = t;
        }
        if let Some(p) = a.workload.preset {
            f.preset = p;
        }
        if let Some(d) = a.workload.duration {
            f.duration = d;
        }
        f.concurrency = a
            .knobs
            .concurrency
            .map(|c| c.to_string())
            .unwrap_or(f.concurrency);
        f.io_concurrency = a
            .knobs
            .io_concurrency
            .map(|c| c.to_string())
            .unwrap_or(f.io_concurrency);
        if let Some(c) = &a.knobs.chunk_size {
            f.chunk = c.clone();
        }
        f.label = "verify".into();
        f.knobs = Some(a.knobs);
        Ok(f)
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
        assert!(req.lab.is_some());
        assert_eq!(req.display.as_deref(), Some("lab://demo"));
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
    fn words_and_commands() {
        assert_eq!(
            split_words("oprobe run 's3://a b' --concurrency 8"),
            vec!["oprobe", "run", "s3://a b", "--concurrency", "8"]
        );
        let r = Report {
            spec: Default::default(),
            capacity: Default::default(),
            schema: crate::model::REPORT_SCHEMA.into(),
            run: Default::default(),
            target: crate::model::TargetInfo {
                display: "memory://".into(),
                ..Default::default()
            },
            workload: Default::default(),
            knobs: Default::default(),
            sla: Default::default(),
            verdict: Default::default(),
            summary: Default::default(),
            stages: Default::default(),
            apis: vec![],
            verbs: vec![],
            connections: Default::default(),
            network: Default::default(),
            resilience: Default::default(),
            bytes: Default::default(),
            cache: Default::default(),
            integrity: Default::default(),
            client: Default::default(),
            open_loop: None,
            timeline: vec![],
            heatmap: Default::default(),
            findings: vec![],
            cost: Default::default(),
            slowest: vec![],
            cleanup: Default::default(),
            warnings: vec![],
            tune: None,
        };
        let f = RunForm::from_command("oprobe run --concurrency 32 --http h2", &r).unwrap();
        assert_eq!(f.target, "memory://");
        assert_eq!(f.concurrency, "32");
        assert!(f.knobs.unwrap().http.is_some());
        assert!(RunForm::from_command("oprobe runs list", &r).is_err());
    }
}
