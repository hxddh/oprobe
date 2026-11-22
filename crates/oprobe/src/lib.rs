//! The `oprobe` front ends: the command line ([`cli`]), the MCP server ([`mcp`]) and the
//! terminal and native app (`ui`), all thin adapters over [`oprobe_core::app`].
//!
//! The library target exists for the integration tests and the UI host. To embed oprobe, use
//! `oprobe-trace` (the instrumentation) or `oprobe-core` (the engine and the service layer).
//! Everything of `oprobe-core` is re-exported at this crate's root.

#![doc(hidden)]

pub use oprobe_core::*;

pub mod cli;
pub mod mcp;
#[cfg(feature = "tui")]
pub mod ui;

/// The UI set the NSH hosts load (`nsh_core` requires this at the crate root).
#[cfg(feature = "tui")]
#[nsh_core::prelude::native_module("oprobe/native")]
fn native(registry: &mut nsh_core::prelude::Registry) {
    registry.register::<ui::App>();
}
