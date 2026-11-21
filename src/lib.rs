//! oprobe — a white-box microscope for cloud object storage built on Apache OpenDAL.
//!
//! Every backend request issued through OpenDAL is traced at the transport level (queue wait,
//! DNS/TCP/proxy/TLS on fresh connections, request send, time-to-first-byte, transfer and
//! consumer stall) and attributed to the logical operation that caused it. On top of that:
//! bottleneck diagnosis, a measured cost model, a closed-loop tuner and a CI regression gate.

pub mod analysis;
pub mod cli;
pub mod compat;
pub mod config;
pub mod engine;
pub mod errors;
pub mod events;
pub mod export;
pub mod guard;
pub mod input;
pub mod lab;
pub mod layers;
pub mod mcp;
pub mod model;
pub mod net;
pub mod output;
pub mod report;
pub mod service_options;
pub mod store;
pub mod trace;
pub mod tune;
#[cfg(feature = "tui")]
pub mod ui;

/// The UI set the NSH hosts load (`nsh_core` requires this at the crate root).
#[cfg(feature = "tui")]
#[nsh_core::prelude::native_module("oprobe/native")]
fn native(registry: &mut nsh_core::prelude::Registry) {
    registry.register::<ui::App>();
}
pub mod util;
