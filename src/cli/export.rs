//! `oprobe runs export RUNREF --format json|ndjson|csv|har|otlp|md|html [--out PATH]
//! [--endpoint URL]`.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::{ExportArgs, ExportFormat};
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
    pub bytes: u64,
    pub ops_count: u64,
    pub requests_count: u64,
}

impl CommandOutput for ExportOutput {
    fn text(&self) -> String {
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

/// Render `run` in `format` (OTLP: traces, metrics or both as one JSON array).
fn render(run: &Run, a: &ExportArgs) -> Result<String> {
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
            let v = match a.signal.as_str() {
                "traces" => export::otlp::traces(run),
                "metrics" => export::otlp::metrics(run),
                _ => serde_json::json!([export::otlp::traces(run), export::otlp::metrics(run)]),
            };
            serde_json::to_string(&v)? + "\n"
        }
        ExportFormat::Md => crate::report::markdown::render(run),
        ExportFormat::Html => crate::report::html::render(run),
    })
}

pub async fn cmd_export(a: ExportArgs, ctx: &Ctx) -> Result<u8> {
    let run = ctx.store().load(&a.run)?;
    let base = || ExportOutput {
        run_id: run.id().to_owned(),
        format: format!("{:?}", a.format).to_ascii_lowercase(),
        path: None,
        endpoint: None,
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
        for signal in ["traces", "metrics"] {
            if a.signal != "all" && a.signal != signal {
                continue;
            }
            let body = if signal == "traces" {
                export::otlp::traces(&run)
            } else {
                export::otlp::metrics(&run)
            };
            bytes += serde_json::to_vec(&body)?.len() as u64;
            export::otlp::push(ep, signal, &body)
                .await
                .map_err(|e| OprobeError::new(ErrorCode::NetUnreachable, format!("{e:#}")))?;
        }
        return ctx.out.finish(&ExportOutput {
            endpoint: Some(ep.clone()),
            bytes,
            ..base()
        });
    }
    let text = render(&run, &a)?;
    match &a.out {
        Some(p) => {
            crate::report::write_file(p, &text)?;
            ctx.out.finish(&ExportOutput {
                path: Some(p.display().to_string()),
                bytes: text.len() as u64,
                ..base()
            })
        }
        None => {
            // The export itself is the result on stdout.
            let mut out = std::io::stdout().lock();
            // A closed pipe (`| head`) is the reader's choice, not a failure.
            match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
                Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e.into()),
                _ => Ok(0),
            }
        }
    }
}
