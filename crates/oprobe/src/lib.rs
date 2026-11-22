//! The `oprobe` front ends: the command line ([`cli`]) and the app (`ui`, feature `app`), thin
//! adapters over [`oprobe_core::app`].
//!
//! The library target exists for the integration tests. To embed oprobe, use `oprobe-trace`.
//! Everything of `oprobe-core` is re-exported at this crate's root.

#![doc(hidden)]

pub use oprobe_core::*;

pub mod cli;
pub mod launch;
#[cfg(feature = "app")]
pub mod ui;

/// The UI set the nsh hosts load (`nsh_core` requires it at the crate root).
#[cfg(feature = "app")]
#[nsh_core::prelude::native_module("oprobe/native")]
fn native(registry: &mut nsh_core::prelude::Registry) {
    registry.register::<ui::App>();
}
