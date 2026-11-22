//! `oprobe demo`: a zero-flag tour. Runs exactly `oprobe run lab://demo` — the in-process S3
//! lab that throttles above a concurrency limit and turns a few requests into stragglers, with
//! the scenario's default workload ([`crate::lab::Scenario::defaults`]) — then explains the
//! result step by step and suggests `--from` replays, so every follow-up run is comparable.

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::input::{DemoRequest, OutputOptions, Safety};
use super::run::{Measured, RunOutput, measure};
use crate::config::{Profile, TargetSpec};
use crate::errors::{ErrorCode, OprobeError};
pub use crate::lab::{
    DEMO_CONCURRENCY, DEMO_STRAGGLER_EVERY, DEMO_STRAGGLER_MS, DEMO_THROTTLE_ABOVE,
};
use crate::model::Report;
use crate::output::{CommandOutput, NextStep};
use crate::report::style::{bold, cyan, dim};
use crate::report::terminal::{severity_icon, wrap_text};
use crate::util::{HumanDuration, fmt_ms, parse_duration};

/// One narrated step.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DemoStep {
    pub title: String,
    pub lines: Vec<String>,
}

/// Result of `oprobe demo`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DemoOutput {
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The narration, step by step.
    pub steps: Vec<DemoStep>,
    /// Steps to try next (the JSON envelope's `next`): `--from` replays of this run.
    #[serde(skip)]
    #[schemars(skip)]
    pub next_steps: Vec<NextStep>,
    pub report: Report,
    #[serde(skip)]
    #[schemars(skip)]
    run: Option<Box<RunOutput>>,
}

impl CommandOutput for DemoOutput {
    fn formats() -> &'static [crate::output::OutputFormat] {
        crate::output::WITH_MD_HTML
    }

    fn next(&self) -> Vec<NextStep> {
        self.next_steps.clone()
    }

    fn text(&self) -> String {
        let mut o = String::new();
        for (i, s) in self.steps.iter().enumerate() {
            o += &format!(
                "\n{} {}\n",
                cyan(&bold(&format!("{}.", i + 1))),
                bold(&s.title)
            );
            for l in &s.lines {
                o += &format!("   {l}\n");
            }
        }
        o += &format!(
            "\n{}\n",
            bold("Next (replays of this run, so they compare):")
        );
        for n in &self.next_steps {
            o += &format!("   {}\n     {}\n", n.cli, dim(&n.title));
        }
        o
    }
    fn markdown(&self) -> String {
        let mut o = String::from("## oprobe demo\n\n");
        for (i, s) in self.steps.iter().enumerate() {
            o += &format!("{}. **{}**\n", i + 1, s.title);
            for l in &s.lines {
                o += &format!("   {l}\n");
            }
        }
        o += "\nNext (replays of this run, so they compare):\n\n```sh\n";
        for n in &self.next_steps {
            o += &format!("{}   # {}\n", n.cli, n.title);
        }
        o += "```\n";
        o
    }
    fn html(&self) -> Option<String> {
        self.run.as_ref().and_then(|r| r.html())
    }
    /// A demo succeeds whatever it found (it is meant to find things).
    fn exit_code(&self) -> u8 {
        if self.report.run.interrupted {
            crate::errors::exit::INTERRUPTED
        } else {
            0
        }
    }
}

/// Plain-language narration of a demo run.
pub fn narrate(r: &Report, run_id: &str) -> (Vec<DemoStep>, Vec<NextStep>) {
    let s = &r.summary;
    let mut steps = vec![DemoStep {
        title: "What ran".into(),
        lines: [
            format!(
                "A local S3 lab that answers 503 SlowDown above {DEMO_THROTTLE_ABOVE} requests in flight \
                 and delays 1 in {DEMO_STRAGGLER_EVERY} requests by {DEMO_STRAGGLER_MS} ms."
            ),
            format!(
                "oprobe ran {DEMO_CONCURRENCY} workers against it for {:.0} s: {} operations, {:.0} ops/s, \
                 p50 {}, p99 {}.",
                r.run.window_s,
                s.ops,
                s.ops_per_s,
                fmt_ms(s.latency.p50),
                fmt_ms(s.latency.p99)
            ),
        ]
        .iter()
        .flat_map(|l| wrap(l))
        .collect(),
    }];
    steps.push(DemoStep {
        title: "The verdict".into(),
        lines: wrap(&r.verdict.headline),
    });
    let mut found = Vec::new();
    let mut fs: Vec<_> = r.findings.iter().collect();
    fs.sort_by_key(|f| std::cmp::Reverse(f.severity));
    for f in fs.iter().take(3) {
        found.push(format!("{} {}", severity_icon(f.severity), bold(&f.cause)));
        if !f.evidence.text.is_empty() {
            for l in wrap(&format!("Evidence: {}", f.evidence.text)) {
                found.push(format!("  {}", dim(&l)));
            }
        }
        for l in wrap(&format!("What to do: {}", f.advice)) {
            found.push(format!("  {l}"));
        }
        if let Some(c) = f.verification.as_ref().map(|v| &v.rerun) {
            found.push(format!("  Check it: {c}"));
        }
    }
    if found.is_empty() {
        found.push(
            "No finding crossed its threshold this time (a short or lucky run); the numbers below \
             still show the throttling and stragglers."
                .into(),
        );
    }
    steps.push(DemoStep {
        title: "What oprobe found".into(),
        lines: found,
    });
    let mut shares: Vec<(&str, f64)> = r.stages.shares.parts().to_vec();
    shares.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top: Vec<String> = shares
        .iter()
        .take(3)
        .filter(|p| p.1 >= 1.0)
        .map(|(n, p)| format!("{n} {p:.0}%"))
        .collect();
    let mut time = vec![format!("Share of operation time: {}.", top.join(", "))];
    if r.resilience.throttled_responses > 0 {
        time.push(format!(
            "{} throttled and retried ({} of backoff in total): that is time and \
             request cost your client pays for asking too much at once.",
            if r.resilience.throttled_responses == 1 {
                "1 request was".to_owned()
            } else {
                format!("{} requests were", r.resilience.throttled_responses)
            },
            fmt_ms(r.resilience.retry_backoff_ms)
        ));
    }
    if s.latency.p99 > s.latency.p50 * 5.0 {
        time.push(format!(
            "The p99 ({}) is {:.0}× the median ({}): stragglers. The full report shows the slowest \
             operation request by request.",
            fmt_ms(s.latency.p99),
            s.latency.p99 / s.latency.p50.max(1e-9),
            fmt_ms(s.latency.p50)
        ));
    }
    steps.push(DemoStep {
        title: "Where the time went".into(),
        lines: time.iter().flat_map(|l| wrap(l)).collect(),
    });
    steps.push(DemoStep {
        title: "Look closer".into(),
        lines: vec![
            format!("oprobe runs show {run_id}                 # the full report"),
            format!("oprobe runs show {run_id} -o html > demo.html   # charts, every request"),
            format!("oprobe runs open {run_id}                 # in the app"),
        ],
    });
    let mut next = vec![
        NextStep::new(
            "replay the demo exactly (same target and workload, so compare is comparable)",
            format!("oprobe run --from {run_id}"),
        )
        .with_mcp("start_run", serde_json::json!({ "from": run_id })),
        NextStep::new(
            "compare the replay with the demo",
            format!("oprobe compare {run_id} last"),
        )
        .with_mcp(
            "compare_runs",
            serde_json::json!({ "baseline": run_id, "candidate": "last" }),
        ),
    ];
    if let Some(f) = fs.iter().find(|f| f.verification.is_some()) {
        next.push(
            NextStep::new(
                "test the top finding: replay with its change, A/B, verdict",
                format!("oprobe verify {run_id} {}", f.code),
            )
            .with_mcp(
                "verify_finding",
                serde_json::json!({ "run": run_id, "code": f.code }),
            ),
        );
    }
    next.push(NextStep::new(
        "the same workload on your bucket",
        format!("oprobe run --from {run_id} s3://YOUR-BUCKET/oprobe --region YOUR-REGION"),
    ));
    (steps, next)
}

fn wrap(s: &str) -> Vec<String> {
    wrap_text(s, 74)
}

/// The profile of `oprobe demo`: `lab://demo` with its scenario defaults (the same as
/// `oprobe run lab://demo`), measured for `duration`.
pub fn demo_profile(duration: std::time::Duration) -> Result<Profile> {
    let def = crate::config::defaults();
    let mut profile = Profile::from_target(TargetSpec::from_url(&def.demo_target)?);
    profile.workload.duration = HumanDuration(duration);
    profile.name = Some("demo".into());
    Ok(profile)
}

/// `oprobe demo`: run `lab://demo` and narrate the result.
pub async fn demo(ctx: &Ctx, a: DemoRequest, cancel: crate::engine::Cancel) -> Result<DemoOutput> {
    let duration = parse_duration(&a.duration).map_err(|e| {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("--duration: {e:#}"),
        )
    })?;
    ctx.out.info(format!(
        "{} running {}: a local S3 lab with throttling above {DEMO_THROTTLE_ABOVE} requests in \
         flight and 1-in-{DEMO_STRAGGLER_EVERY} stragglers…",
        bold("oprobe demo"),
        crate::config::defaults().demo_target
    ));
    let profile = demo_profile(duration)?;
    let output = OutputOptions {
        label: Some("demo".into()),
        ..Default::default()
    };
    let out = match measure(ctx, profile, None, &Safety::default(), &output, cancel).await? {
        Measured::Run(r) => r,
        Measured::Plan(_) => unreachable!("the demo is never a dry run"),
    };
    let (steps, next_steps) = narrate(&out.report, &out.run_id);
    Ok(DemoOutput {
        run_id: out.run_id.clone(),
        path: out.path.clone(),
        steps,
        next_steps,
        report: out.report.clone(),
        run: Some(out),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narration_names_the_findings_and_the_next_step() {
        crate::report::style::set(false);
        let r = crate::report::fixture::report();
        let (steps, next) = narrate(&r, "20261120-143005-a1b2");
        let text: String = steps
            .iter()
            .flat_map(|s| s.lines.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("503 SlowDown"), "{text}");
        assert!(text.contains("What to do: Lower the concurrency"), "{text}");
        assert!(
            text.contains("Check it: oprobe run --from 20261120-143005-a1b2 --set concurrency=16"),
            "{text}"
        );
        assert!(text.contains("oprobe runs show 20261120-143005-a1b2"));
        assert_eq!(next[0].cli, "oprobe run --from 20261120-143005-a1b2");
        assert!(next.iter().all(|n| n.cli.contains("20261120-143005-a1b2")));
        assert!(next.iter().any(|n| n.cli.starts_with("oprobe compare")));
        assert_eq!(steps.len(), 5);
    }
}

#[cfg(test)]
mod scenario_tests {
    #[test]
    fn narration_matches_the_lab_demo_scenario() {
        let c = crate::lab::Scenario::Demo.config();
        assert_eq!(c.max_inflight_before_503, Some(super::DEMO_THROTTLE_ABOVE));
        assert_eq!(c.straggler_every_n, super::DEMO_STRAGGLER_EVERY);
        assert_eq!(
            c.straggler_delay.as_millis() as u64,
            super::DEMO_STRAGGLER_MS
        );
    }
}

#[cfg(test)]
mod profile_tests {
    use crate::config::{Preset, Profile, TargetSpec};

    #[test]
    fn the_demo_is_lab_demo_with_its_defaults() {
        let def = crate::config::defaults();
        let demo = super::demo_profile(def.demo_duration.get()).unwrap();
        let run = Profile::from_target(TargetSpec::from_url("lab://demo").unwrap());
        assert_eq!(demo.workload, run.workload);
        assert_eq!(demo.knobs, run.knobs);
        assert_eq!(demo.workload.preset, Preset::SmallFiles);
        let ds = demo.workload.effective_dataset();
        assert_eq!(ds.objects, 64);
        assert_eq!(ds.object_size, crate::util::ByteSize::kib(64));
        assert_eq!(demo.workload.warmup.get().as_millis(), 500);
        assert_eq!(demo.workload.duration, def.demo_duration);
        assert_eq!(demo.knobs.concurrency, super::DEMO_CONCURRENCY);
        let a = crate::model::RunSpec::new(&demo, "lab://demo");
        let b = crate::model::RunSpec::new(&run, "lab://demo");
        assert_eq!(a.workload_hash(), b.workload_hash());
        assert_eq!(a.hash(), b.hash());
        // `lab://parquet` runs the parquet preset.
        let pq = Profile::from_target(TargetSpec::from_url("lab://parquet").unwrap());
        assert_eq!(pq.workload.preset, Preset::Parquet);
        assert_eq!(pq.knobs.concurrency, crate::lab::PARQUET_CONCURRENCY);
    }
}
