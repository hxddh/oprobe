//! Agent surface of oprobe: the MCP server (`oprobe mcp`), JSON Schemas of every machine
//! output (`oprobe schema`), self-description (`oprobe capabilities`) and explanations
//! (`oprobe explain`).
//!
//! The MCP server ([`server::OprobeMcp`]) is built on the official Rust SDK (`rmcp`): stdio by
//! default, Streamable HTTP with `--http ADDR`. stdout is the protocol channel in stdio mode;
//! nothing else in this module writes to it.
//!
//! Safety (design v5 §5): the server is read-only unless started with `--allow-write` (runs that
//! upload a dataset, tune, cleanup, `save_target` and `apply_profile` need it; `memory://` and
//! `lab://` are always allowed); server budgets cap every run it starts (50k requests, 2 min,
//! $1, 5 GiB unless its flags say otherwise; a call can only lower them); a call cannot point a
//! credentialed target elsewhere ([`security`]); `--allow-target` restricts the targets tools
//! may touch; text from storage services is escaped and marked `untrusted`; the HTTP transport
//! requires a bearer token.

pub mod capabilities;
pub use crate::analysis::catalog;
pub use crate::app::explain;
pub mod ask;
mod jobs;
pub mod prompts;
pub mod resources;
pub mod responses;
pub mod schemas;
pub mod security;
pub mod server;
pub mod tools;

use crate::guard::Budgets;
use crate::store::Paths;

/// Server-wide settings (`oprobe mcp` flags).
#[derive(Clone, Debug)]
pub struct McpConfig {
    /// Tools that write test data are enabled.
    pub allow_write: bool,
    /// Budgets every run gets (requests may only lower them).
    pub budgets: Budgets,
    /// Allowed targets: configured names or URL prefixes (empty: any).
    pub allow_targets: Vec<String>,
    /// Config file and run store.
    pub paths: Paths,
    /// Runs executing at the same time.
    pub max_concurrent_runs: usize,
    /// `probe_endpoint` and endpoint overrides may reach loopback, private and link-local
    /// addresses (`--allow-private-endpoints`).
    pub allow_private_endpoints: bool,
}

impl McpConfig {
    /// A read-only server with the default budgets ([`Budgets::mcp_default`]) on `paths`.
    pub fn new(paths: Paths) -> Self {
        Self {
            allow_write: false,
            budgets: Budgets::mcp_default(),
            allow_targets: Vec::new(),
            paths,
            max_concurrent_runs: 1,
            allow_private_endpoints: false,
        }
    }
}

pub use server::{HttpOptions, OprobeMcp, serve_http, serve_stdio};
