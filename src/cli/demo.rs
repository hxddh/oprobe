//! `oprobe demo`: a zero-flag tour. Starts an in-process S3 lab that throttles above a
//! concurrency limit and turns a few requests into stragglers, measures it like any bucket,
//! then explains the result step by step and shows how to measure a real bucket next.

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::{DemoArgs, OutputArgs, SafetyArgs};
use super::run::{Measured, RunOutput, measure};
use crate::config::{DatasetSpec, Preset};
use crate::errors::{ErrorCode, OprobeError};
use crate::lab::{LabConfig, LabServer};
use crate::model::Report;
use crate::output::CommandOutput;
use crate::report::style::{bold, cyan, dim};
use crate::report::terminal::{severity_icon, wrap_text};
use crate::util::{ByteSize, HumanDuration, fmt_ms, parse_duration};

/// Requests in flight above which the lab answers `503 SlowDown`.
pub const DEMO_THROTTLE_ABOVE: usize = 24;
/// Workers of the demo run (more than the lab accepts at once).
pub const DEMO_CONCURRENCY: usize = 32;
/// Every Nth request is a straggler.
pub const DEMO_STRAGGLER_EVERY: usize = 40;
/// Extra delay of a straggler.
pub const DEMO_STRAGGLER_MS: u64 = 250;

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
    /// Commands to try next.
    pub next_commands: Vec<String>,
    pub report: Report,
    #[serde(skip)]
    #[schemars(skip)]
    run: Option<Box<RunOutput>>,
}

impl CommandOutput for DemoOutput {
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
        o += &format!("\n{}\n", bold("Now try your bucket:"));
        for c in &self.next_commands {
            o += &format!("   {c}\n");
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
        o += "\nNow try your bucket:\n\n```sh\n";
        for c in &self.next_commands {
            o += &format!("{c}\n");
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

/// The lab's fault profile for the demo.
pub fn demo_lab() -> LabConfig {
    LabConfig {
        enable_http2: true,
        base_ttfb_delay: std::time::Duration::from_millis(3),
        straggler_every_n: DEMO_STRAGGLER_EVERY,
        straggler_delay: std::time::Duration::from_millis(DEMO_STRAGGLER_MS),
        max_inflight_before_503: Some(DEMO_THROTTLE_ABOVE),
        throttle_status_code: 503,
        ..Default::default()
    }
}

/// Plain-language narration of a demo run.
pub fn narrate(r: &Report, run_id: &str) -> (Vec<DemoStep>, Vec<String>) {
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
        if let Some(c) = f.verification.as_ref().and_then(|v| v.command.as_ref()) {
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
            "{} requests were throttled and retried ({} of backoff in total): that is time and \
             request cost your client pays for asking too much at once.",
            r.resilience.throttled_responses,
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
    let next = vec![
        "oprobe target add prod s3://YOUR-BUCKET/oprobe --region YOUR-REGION".into(),
        "oprobe run prod".into(),
        dim("(or just `oprobe run` for a guided choice of the credentials on this machine)"),
    ];
    (steps, next)
}

fn wrap(s: &str) -> Vec<String> {
    wrap_text(s, 74)
}

pub async fn cmd_demo(a: DemoArgs, ctx: &Ctx) -> Result<u8> {
    let duration = parse_duration(&a.duration).map_err(|e| {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("--duration: {e:#}"),
        )
    })?;
    // TODO(B2 lab::Scenario): once `lab://demo` resolves through `lab::Scenario`, run that
    // target (`super::resolve_profile` on "lab://demo") instead of configuring the lab here.
    ctx.out.info(format!(
        "{} starting a local S3 lab with throttling above {DEMO_THROTTLE_ABOVE} requests in flight \
         and 1-in-{DEMO_STRAGGLER_EVERY} stragglers…",
        bold("oprobe demo")
    ));
    let lab = LabServer::spawn("127.0.0.1:0", demo_lab()).await?;
    let mut profile = lab.profile("demo");
    profile.name = Some("demo".into());
    profile.workload.preset = Preset::SmallFiles;
    profile.workload.duration = HumanDuration(duration);
    profile.workload.warmup = HumanDuration(std::time::Duration::from_millis(500));
    profile.workload.dataset = Some(DatasetSpec {
        objects: 64,
        object_size: ByteSize::kib(64),
        ..DatasetSpec::default()
    });
    profile.knobs.concurrency = DEMO_CONCURRENCY;
    let output = OutputArgs {
        label: Some("demo".into()),
        ..Default::default()
    };
    let out = match measure(
        ctx,
        profile,
        Some("lab://demo".into()),
        &SafetyArgs::default(),
        &output,
    )
    .await?
    {
        Measured::Run(r) => r,
        Measured::Plan(p) => return ctx.out.finish(p.as_ref()),
    };
    drop(lab);
    let (steps, next_commands) = narrate(&out.report, &out.run_id);
    ctx.out.finish(&DemoOutput {
        run_id: out.run_id.clone(),
        path: out.path.clone(),
        steps,
        next_commands,
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
        assert!(text.contains("Check it: oprobe run prod -c 16"), "{text}");
        assert!(text.contains("oprobe runs show 20261120-143005-a1b2"));
        assert!(next[0].starts_with("oprobe target add"));
        assert_eq!(steps.len(), 5);
    }
}
