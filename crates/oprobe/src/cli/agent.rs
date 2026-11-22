//! Agent-facing commands: `oprobe mcp`, `oprobe schema [NAME]`, `oprobe capabilities`,
//! `oprobe explain RUNREF|FINDING_CODE` (implemented in [`crate::mcp`]).

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::{CapabilitiesArgs, McpArgs, SchemaArgs};
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::guard::Budgets;
use crate::mcp::{McpConfig, schemas};
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
    // Unset flags take the MCP defaults (design v5 §5, S6): flags may raise or lower them,
    // tool calls only lower them.
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
    }
    .or(&Budgets::mcp_default()))
}

/// What `oprobe mcp --http` serves (printed once it listens).
#[derive(Clone, Debug, Serialize, JsonSchema)]
struct McpHttpOutput {
    url: String,
    allow_write: bool,
    /// The file holding the bearer token (`Authorization: Bearer TOKEN`).
    token_file: String,
    tls: bool,
}

impl CommandOutput for McpHttpOutput {
    fn text(&self) -> String {
        format!(
            "oprobe MCP server (Streamable HTTP) on {}{}\n  token: {} (send `Authorization: Bearer TOKEN`)\n",
            self.url,
            if self.allow_write {
                " · writes allowed"
            } else {
                " · read-only"
            },
            self.token_file
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

/// `--http` binds loopback unless `--http-allow-remote`, and a remote bind needs TLS or an
/// explicit `--insecure-remote` (design v5 §5, S2).
async fn check_http_bind(
    addr: &str,
    allow_remote: bool,
    tls: bool,
    insecure_remote: bool,
    ctx: &Ctx,
) -> Result<()> {
    if loopback_only(addr).await? {
        return Ok(());
    }
    if !allow_remote {
        return Err(OprobeError::new(
            ErrorCode::SafetyRefused,
            format!("refusing to serve MCP on non-loopback address `{addr}`"),
        )
        .with_hint("bind 127.0.0.1:PORT (tunnel it if needed), or pass --http-allow-remote with --tls-cert/--tls-key")
        .into());
    }
    if !tls && !insecure_remote {
        return Err(OprobeError::new(
            ErrorCode::SafetyRefused,
            format!("refusing to serve MCP on `{addr}` without TLS: the bearer token would cross the network in clear"),
        )
        .with_hint("pass --tls-cert and --tls-key, or --insecure-remote to accept the risk")
        .into());
    }
    ctx.out.warn(format!(
        "serving MCP on non-loopback address {addr}{}: anyone with the token can run tools with the configured credentials",
        if tls { "" } else { " without TLS" }
    ));
    Ok(())
}

/// The bearer token of `--http`: read from `--token-file`, or generated and written to
/// `<data>/mcp-token` (mode 0600). Returns the token and its file.
fn http_token(a: &McpArgs, ctx: &Ctx) -> Result<(String, std::path::PathBuf)> {
    match &a.token_file {
        Some(p) => {
            let t = std::fs::read_to_string(p)
                .map_err(|e| {
                    OprobeError::new(
                        ErrorCode::UsageInvalidArgument,
                        format!("cannot read --token-file `{}`: {e}", p.display()),
                    )
                })?
                .trim()
                .to_owned();
            if t.len() < 16 || t.chars().any(char::is_whitespace) {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!(
                        "--token-file `{}` must hold one token of at least 16 characters",
                        p.display()
                    ),
                )
                .into());
            }
            Ok((t, p.clone()))
        }
        None => {
            let t = crate::guard::random_token();
            let p = ctx.paths.data_dir.join("mcp-token");
            crate::store::write_secret(&p, &t)?;
            Ok((t, p))
        }
    }
}

fn read_pem(p: &std::path::Path, flag: &str) -> Result<String> {
    std::fs::read_to_string(p).map_err(|e| {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("cannot read {flag} `{}`: {e}", p.display()),
        )
        .into()
    })
}

pub async fn cmd_mcp(a: McpArgs, ctx: &Ctx) -> Result<u8> {
    let cfg = McpConfig {
        allow_write: a.allow_write,
        budgets: server_budgets(&a)?,
        allow_targets: a.allow_targets.clone(),
        paths: ctx.paths.clone(),
        max_concurrent_runs: a.max_concurrent_runs,
        allow_private_endpoints: a.allow_private_endpoints,
    };
    // Surface an invalid config file now rather than on the first tool call.
    let config = ctx.config()?;
    if let Some(w) = config.inline_secrets_warning(&ctx.paths.config_file) {
        ctx.out.warn(w);
    }
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
            let tls = match (&a.tls_cert, &a.tls_key) {
                (Some(c), Some(k)) => Some((read_pem(c, "--tls-cert")?, read_pem(k, "--tls-key")?)),
                _ => None,
            };
            check_http_bind(
                addr,
                a.http_allow_remote,
                tls.is_some(),
                a.insecure_remote,
                ctx,
            )
            .await?;
            let (token, token_file) = http_token(&a, ctx)?;
            let mut http = crate::mcp::HttpOptions::new(token);
            http.max_connections = a.max_connections;
            let scheme = if tls.is_some() { "https" } else { "http" };
            http.tls = tls;
            let out = &ctx.out;
            let allow_write = a.allow_write;
            let mut printed = Ok(exit::OK);
            crate::mcp::serve_http(cfg, addr, http, |local| {
                printed = out.finish(&McpHttpOutput {
                    url: format!("{scheme}://{local}/mcp"),
                    allow_write,
                    token_file: token_file.display().to_string(),
                    tls: scheme == "https",
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
        let refused =
            |e: anyhow::Error| crate::errors::classify(&e, "s3").code == ErrorCode::SafetyRefused;
        assert!(refused(
            check_http_bind("0.0.0.0:0", false, false, false, &ctx)
                .await
                .unwrap_err()
        ));
        // Remote needs TLS, or an explicit --insecure-remote.
        assert!(refused(
            check_http_bind("0.0.0.0:0", true, false, false, &ctx)
                .await
                .unwrap_err()
        ));
        check_http_bind("0.0.0.0:0", true, true, false, &ctx)
            .await
            .unwrap();
        check_http_bind("0.0.0.0:0", true, false, true, &ctx)
            .await
            .unwrap();
        check_http_bind("127.0.0.1:0", false, false, false, &ctx)
            .await
            .unwrap();
    }
}
