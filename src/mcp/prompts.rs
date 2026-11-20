//! MCP prompts: guided workflows built from the tools.

use serde_json::{Map, Value};

use crate::errors::{ErrorCode, OprobeError};

/// One prompt argument.
pub struct Arg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// A prompt.
pub struct PromptSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub args: &'static [Arg],
}

const TARGET: Arg = Arg {
    name: "target",
    description: "Configured target name or storage URL (s3://bucket/prefix, memory://)",
    required: true,
};

/// Every prompt, in `prompts/list` order.
pub const PROMPTS: &[PromptSpec] = &[
    PromptSpec {
        name: "diagnose-slow-reads",
        title: "Diagnose slow reads",
        description: "Find out why reads from a target are slow: preflight, a budgeted read-heavy run, findings and outlier attribution.",
        args: &[
            TARGET,
            Arg {
                name: "prefix",
                description: "Existing objects to read (read-only run); omit to generate a small dataset",
                required: false,
            },
            Arg {
                name: "run",
                description: "An existing run to diagnose instead of starting one",
                required: false,
            },
        ],
    },
    PromptSpec {
        name: "tune-for-p99",
        title: "Tune for p99 latency",
        description: "Find a client configuration that keeps p99 latency under a limit without throttling.",
        args: &[
            TARGET,
            Arg {
                name: "p99_ms",
                description: "p99 objective in milliseconds",
                required: true,
            },
            Arg {
                name: "preset",
                description: "Workload preset (parquet, checkpoint, small-files, metadata, mixed)",
                required: false,
            },
        ],
    },
    PromptSpec {
        name: "ci-gate-setup",
        title: "Set up a CI regression gate",
        description: "Create a baseline run and a CI job that fails on latency, throughput, error or cost regressions.",
        args: &[
            TARGET,
            Arg {
                name: "ci",
                description: "CI system (github-actions, gitlab, other); default github-actions",
                required: false,
            },
        ],
    },
];

fn arg<'a>(a: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    a.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

/// Render a prompt: its description and one user message.
pub fn render(name: &str, a: &Map<String, Value>) -> anyhow::Result<(String, String)> {
    let p = PROMPTS.iter().find(|p| p.name == name).ok_or_else(|| {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("unknown prompt `{name}`"),
        )
    })?;
    for x in p.args.iter().filter(|x| x.required) {
        if arg(a, x.name).is_none() {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("prompt `{name}` needs the `{}` argument", x.name),
            )
            .into());
        }
    }
    let target = arg(a, "target").unwrap_or("memory://");
    let text = match name {
        "diagnose-slow-reads" => {
            let start = match (arg(a, "run"), arg(a, "prefix")) {
                (Some(run), _) => format!(
                    "The run to diagnose is `{run}`: skip to step 4 with it."
                ),
                (None, Some(prefix)) => format!(
                    "Read the existing objects under `{prefix}` (set `existing: \"{prefix}\"`, which is read-only)."
                ),
                (None, None) => "No existing prefix was given: use the `small-files` or `parquet` preset with a small generated dataset (this needs a server started with --allow-write unless the target is memory://).".to_owned(),
            };
            format!(
                "Diagnose why reads from `{target}` are slow, using the oprobe MCP tools.\n\n\
                 {start}\n\n\
                 1. Call `doctor` with target `{target}` and stop if it is not ready; report the failing check and its hint.\n\
                 2. Call `plan_run` with a 15s duration and a request budget (`max_requests: 20000`); check `allowed` and `budget_warnings`.\n\
                 3. Call `start_run` with the same arguments; poll `get_run` until the status is final.\n\
                 4. Call `explain_run` on the run and summarize the verdict and the top findings with their evidence numbers.\n\
                 5. Call `analyze_run` (default: slowest 1%) and say which attributes (cold connections, remote IP, retries, a stage) explain the tail.\n\
                 6. Propose one knob change from the findings' advice, run again with it, and use `compare_runs` to confirm it helped.\n\
                 Never raise budgets on your own; ask first."
            )
        }
        "tune-for-p99" => {
            let p99 = arg(a, "p99_ms").unwrap_or("100");
            let preset = arg(a, "preset").unwrap_or("parquet");
            format!(
                "Find a client configuration for `{target}` (preset `{preset}`) whose p99 latency stays under {p99} ms without throttling.\n\n\
                 1. `doctor` the target.\n\
                 2. `plan_run` a 10s baseline and `start_run` it; `explain_run` it.\n\
                 3. `start_tune` with objective `latency`, `max_trials: 12` and a `max_duration` budget; tell the user the plan's ETA first. It needs a server started with --allow-write unless the target is memory:// or `existing` is set.\n\
                 4. Poll `get_run` until done; report the winning knobs from `report.tune` (trials, improvement_pct) and whether p99 meets {p99} ms.\n\
                 5. Confirm with a normal run using the winning knobs and `compare_runs` against the baseline."
            )
        }
        _ => {
            let ci = arg(a, "ci").unwrap_or("github-actions");
            format!(
                "Set up a CI regression gate for `{target}` on {ci}.\n\n\
                 1. `doctor` the target, then create a baseline: `start_run` with label `baseline`, a fixed preset, duration and concurrency, and budgets.\n\
                 2. `explain_run` it; the baseline should have no critical findings.\n\
                 3. Write a CI job that runs `oprobe run {target} --preset <same> -d <same> -c <same> --label $COMMIT -o json --max-requests N` and then `oprobe compare baseline last --junit gate.xml --markdown gate.md`; `oprobe compare` exits 3 on a regression.\n\
                 4. Keep secrets in the CI secret store (credentials come from the environment), and add `oprobe cleanup {target}` as an always-run step.\n\
                 5. Explain the thresholds (`--max-p99-increase`, `--max-throughput-drop`, …) and validate the JSON outputs with `oprobe schema compare`."
            )
        }
    };
    Ok((p.description.to_owned(), text))
}
