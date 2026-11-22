//! `oprobe runs export RUNREF --format json|ndjson|csv|har|otlp|md|html [--out PATH]
//! [--endpoint URL]`.

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::input::{ExportFormat, ExportRequest};
use crate::errors::{ErrorCode, OprobeError};
use crate::export;
use crate::model::Run;
use crate::output::CommandOutput;

/// Result of an export written to a file or pushed to a collector.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ExportOutput {
    pub run_id: String,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Files written (OTLP `--signal all`: one document per signal).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Bytes written.
    #[serde(rename = "written_bytes")]
    pub bytes: u64,
    pub ops_count: u64,
    pub requests_count: u64,
}

impl CommandOutput for ExportOutput {
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![crate::app::next::show(&self.run_id)]
    }

    fn text(&self) -> String {
        if !self.files.is_empty() {
            return format!(
                "exported run {} as {} to {} ({} bytes)\n",
                self.run_id,
                self.format,
                self.files.join(", "),
                self.bytes
            );
        }
        match (&self.path, &self.endpoint) {
            (Some(p), _) => format!(
                "exported run {} as {} to {p} ({} bytes)\n",
                self.run_id, self.format, self.bytes
            ),
            (_, Some(e)) => format!(
                "pushed run {} to {e} ({} operations, {} requests)\n",
                self.run_id, self.ops_count, self.requests_count
            ),
            _ => String::new(),
        }
    }
}

/// One OTLP/JSON document (`ExportTraceServiceRequest` / `ExportMetricsServiceRequest`).
fn otlp(run: &Run, signal: &str) -> serde_json::Value {
    if signal == "metrics" {
        export::otlp::metrics(run)
    } else {
        export::otlp::traces(run)
    }
}

/// Render `run` in `format` (OTLP: the one selected signal).
fn render(run: &Run, a: &ExportRequest) -> Result<String> {
    Ok(match a.format {
        ExportFormat::Json => serde_json::to_string_pretty(run)? + "\n",
        ExportFormat::Ndjson => export::ndjson::to_string(run, true)?,
        ExportFormat::Csv => match a.table.as_str() {
            "requests" | "reqs" => export::csv::requests(run),
            "ops" | "operations" => export::csv::operations(run),
            t => {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("invalid --table `{t}` (requests or ops)"),
                )
                .into());
            }
        },
        ExportFormat::Har => serde_json::to_string_pretty(&export::har::har(run))? + "\n",
        ExportFormat::Otlp => {
            let [signal] = a.signal.signals() else {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    "--signal all writes one OTLP document per signal: pass --out DIR (or --endpoint URL)",
                )
                .with_hint("or pick one: --signal traces | --signal metrics")
                .into());
            };
            serde_json::to_string(&otlp(run, signal))? + "\n"
        }
        ExportFormat::Md => crate::report::markdown::render(run),
        ExportFormat::Html => crate::report::html::render(run),
    })
}

/// What [`export`] produced: a file (or an OTLP push) described by [`ExportOutput`], or the
/// export itself for stdout (no `out`).
#[derive(Clone, Debug)]
pub enum Exported {
    Written(Box<ExportOutput>),
    Stdout(String),
}

/// `oprobe runs export`.
pub async fn export(ctx: &Ctx, a: ExportRequest) -> Result<Exported> {
    let run = ctx.store().load(&a.run)?;
    let base = || ExportOutput {
        run_id: run.id().to_owned(),
        format: format!("{:?}", a.format).to_ascii_lowercase(),
        path: None,
        endpoint: None,
        files: Vec::new(),
        bytes: 0,
        ops_count: run.ops.len() as u64,
        requests_count: run.reqs.len() as u64,
    };
    if let Some(ep) = &a.endpoint {
        if !matches!(a.format, ExportFormat::Otlp) {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                "--endpoint pushes OTLP; use it with --format otlp",
            )
            .into());
        }
        let mut bytes = 0;
        for signal in a.signal.signals() {
            let body = otlp(&run, signal);
            bytes += serde_json::to_vec(&body)?.len() as u64;
            export::otlp::push(ep, signal, &body)
                .await
                .map_err(|e| OprobeError::new(ErrorCode::NetUnreachable, format!("{e:#}")))?;
        }
        return Ok(Exported::Written(Box::new(ExportOutput {
            endpoint: Some(ep.clone()),
            bytes,
            ..base()
        })));
    }
    if matches!(a.format, ExportFormat::Otlp)
        && a.signal == super::input::OtlpSignal::All
        && let Some(dir) = &a.out
    {
        // One valid OTLP document per signal, in the directory `--out` names.
        std::fs::create_dir_all(dir).map_err(|e| {
            OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("cannot create --out directory `{}`: {e}", dir.display()),
            )
        })?;
        let mut files = Vec::new();
        let mut bytes = 0;
        for signal in a.signal.signals() {
            let text = serde_json::to_string(&otlp(&run, signal))? + "\n";
            let p = dir.join(format!("{}.{signal}.otlp.json", run.id()));
            crate::report::write_file(&p, &text)?;
            bytes += text.len() as u64;
            files.push(p.display().to_string());
        }
        return Ok(Exported::Written(Box::new(ExportOutput {
            path: Some(dir.display().to_string()),
            files,
            bytes,
            ..base()
        })));
    }
    let text = render(&run, &a)?;
    match &a.out {
        Some(p) => {
            crate::report::write_file(p, &text)?;
            Ok(Exported::Written(Box::new(ExportOutput {
                path: Some(p.display().to_string()),
                bytes: text.len() as u64,
                ..base()
            })))
        }
        // The export itself is the result on stdout.
        None => Ok(Exported::Stdout(text)),
    }
}
