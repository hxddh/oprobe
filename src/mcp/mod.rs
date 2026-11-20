//! Agent surface of oprobe: the MCP server (`oprobe mcp`), JSON Schemas of every machine
//! output (`oprobe schema`), self-description (`oprobe capabilities`) and explanations
//! (`oprobe explain`).
//!
//! The MCP server ([`server::OprobeMcp`]) is built on the official Rust SDK (`rmcp`): stdio by
//! default, Streamable HTTP with `--http ADDR`. stdout is the protocol channel in stdio mode;
//! nothing else in this module writes to it.
//!
//! Safety: the server is read-only unless started with `--allow-write` (runs that upload a
//! dataset, tune and cleanup need it; `memory://` is always allowed), server-level budgets cap
//! every run it starts (a request can only lower them), and `--allow-target` restricts the
//! targets tools may touch.

pub mod capabilities;
pub mod catalog;
pub mod explain;
mod jobs;
pub mod prompts;
pub mod resources;
pub mod schemas;
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
}

pub use server::{OprobeMcp, serve_http, serve_stdio};
