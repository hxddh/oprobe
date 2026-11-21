//! Agent-facing commands: `oprobe mcp`, `oprobe schema [NAME]`, `oprobe capabilities`,
//! `oprobe explain RUNREF|FINDING_CODE` (implemented in [`crate::mcp`]).

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::{CapabilitiesArgs, ExplainArgs, McpArgs, SchemaArgs};
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::guard::Budgets;
use crate::mcp::explain::{ExplainOutput, explain_subject};
use crate::mcp::{McpConfig, catalog, schemas};
use crate::output::{CommandOutput, OutputFormat};
use crate::util::{ByteSize, parse_duration};

/// Result of `oprobe schema NAME`: the schema document itself.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(transparent)]
struct SchemaDoc(serde_json::Value);

impl CommandOutput for SchemaDoc {
    fn formats() -> &'static [crate::output::OutputFormat] {
        crate::output::WITH_MD
    }
    fn enveloped(&self) -> bool {
        false
    }

    fn text(&self) -> String {
        serde_json::to_string_pretty(&self.0).unwrap_or_default()
    }
    fn markdown(&self) -> String {
        format!("```json\n{}\n```\n", self.text())
    }
}

pub fn cmd_schema(a: SchemaArgs, ctx: &Ctx) -> Result<u8> {
    match a.name.as_deref() {
        None => ctx.out.finish(&schemas::SchemaList::current()),
        Some(name) => {
            let name = name.trim_end_matches(".json");
            let Some(s) = schemas::schema(name) else {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("unknown schema `{name}`"),
                )
                .with_hint(format!("names: {}", schemas::names().join(", ")))
                .into());
            };
            ctx.out.finish(&SchemaDoc(s))
        }
    }
}

pub fn cmd_capabilities(_a: CapabilitiesArgs, ctx: &Ctx) -> Result<u8> {
    ctx.out
        .finish(&crate::mcp::capabilities::Capabilities::current())
}

pub fn cmd_explain(a: ExplainArgs, ctx: &Ctx) -> Result<u8> {
    if a.list {
        return ctx.out.finish(&ExplainOutput::FindingCodes {
            codes: catalog::all(),
        });
    }
    let subject = a.subject.as_deref().unwrap_or_default();
    let out = explain_subject(&ctx.store(), subject, a.top)?;
    ctx.out.finish(&out)
}

fn server_budgets(a: &McpArgs) -> Result<Budgets> {
    let bad = |flag: &str, e: anyhow::Error| -> anyhow::Error {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("invalid {flag}: {e:#}"),
        )
        .into()
    };
    if let Some(c) = a.max_cost_usd {
        anyhow::ensure!(
            c.is_finite() && c >= 0.0,
            OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                "--max-cost-usd must be >= 0"
            )
        );
    }
    Ok(Budgets {
        max_cost_usd: a.max_cost_usd,
        max_requests_count: a.max_requests,
        max_bytes: a
            .max_bytes
            .as_deref()
            .map(|v| v.parse::<ByteSize>().map(|b| b.0))
            .transpose()
            .map_err(|e| bad("--max-bytes", e))?,
        max_duration_s: a
            .max_duration
            .as_deref()
            .map(|v| parse_duration(v).map(|d| d.as_secs_f64()))
            .transpose()
            .map_err(|e| bad("--max-duration", e))?,
    })
}

/// What `oprobe mcp --http` serves (printed once it listens).
#[derive(Clone, Debug, Serialize, JsonSchema)]
struct McpHttpOutput {
    url: String,
    allow_write: bool,
}

impl CommandOutput for McpHttpOutput {
    fn text(&self) -> String {
        format!(
            "oprobe MCP server (Streamable HTTP) on {}{}\n",
            self.url,
            if self.allow_write {
                " · writes allowed"
            } else {
                " · read-only"
            }
        )
    }
}

/// Whether every address `addr` resolves to is a loopback address.
async fn loopback_only(addr: &str) -> Result<bool> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(addr)
        .await
        .map_err(|e| {
            OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("invalid --http address `{addr}`: {e}"),
            )
            .with_hint("e.g. --http 127.0.0.1:8765")
        })?
        .collect();
    Ok(!addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback()))
}

/// `--http` serves without authentication: loopback only unless `--http-allow-remote`.
async fn check_http_bind(addr: &str, allow_remote: bool, ctx: &Ctx) -> Result<()> {
    if loopback_only(addr).await? {
        return Ok(());
    }
    if !allow_remote {
        return Err(OprobeError::new(
            ErrorCode::SafetyRefused,
            format!("refusing to serve MCP on non-loopback address `{addr}`: the server has no authentication"),
        )
        .with_hint("bind 127.0.0.1:PORT (tunnel it if needed), or pass --http-allow-remote to accept the risk")
        .into());
    }
    ctx.out.warn(format!(
        "serving MCP on non-loopback address {addr} without authentication: anyone who can reach it can run tools with the configured credentials"
    ));
    Ok(())
}

pub async fn cmd_mcp(a: McpArgs, ctx: &Ctx) -> Result<u8> {
    let cfg = McpConfig {
        allow_write: a.allow_write,
        budgets: server_budgets(&a)?,
        allow_targets: a.allow_targets.clone(),
        paths: ctx.paths.clone(),
        max_concurrent_runs: a.max_concurrent_runs,
    };
    // Surface an invalid config file now rather than on the first tool call.
    ctx.config()?;
    match &a.http {
        None => {
            if ctx.out.format == OutputFormat::Ndjson {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    "`oprobe mcp` speaks JSON-RPC on stdout; do not combine it with -o ndjson",
                )
                .into());
            }
            crate::mcp::serve_stdio(cfg).await?;
        }
        Some(addr) => {
            check_http_bind(addr, a.http_allow_remote, ctx).await?;
            let out = &ctx.out;
            let allow_write = a.allow_write;
            let mut printed = Ok(exit::OK);
            crate::mcp::serve_http(cfg, addr, |local| {
                printed = out.finish(&McpHttpOutput {
                    url: format!("http://{local}/mcp"),
                    allow_write,
                });
            })
            .await?;
            printed?;
        }
    }
    Ok(exit::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_binding_is_loopback_unless_allowed() {
        assert!(loopback_only("127.0.0.1:0").await.unwrap());
        assert!(loopback_only("[::1]:0").await.unwrap());
        assert!(!loopback_only("0.0.0.0:0").await.unwrap());
        assert!(loopback_only("not an address").await.is_err());
        let ctx = Ctx::new(
            crate::output::Output::silent(),
            crate::store::Paths {
                config_file: "c.toml".into(),
                data_dir: "d".into(),
            },
        );
        let e = check_http_bind("0.0.0.0:0", false, &ctx).await.unwrap_err();
        assert_eq!(
            crate::errors::classify(&e, "s3").code,
            ErrorCode::SafetyRefused
        );
        check_http_bind("0.0.0.0:0", true, &ctx).await.unwrap();
        check_http_bind("127.0.0.1:0", false, &ctx).await.unwrap();
    }
}
