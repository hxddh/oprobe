//! The `oprobe` front ends: the command line ([`cli`]) and the app, thin adapters over
//! [`oprobe_core::app`].
//!
//! The library target exists for the integration tests. To embed oprobe, use `oprobe-trace`.
//! Everything of `oprobe-core` is re-exported at this crate's root.

#![doc(hidden)]

pub use oprobe_core::*;

pub mod app_stub;
pub mod cli;
