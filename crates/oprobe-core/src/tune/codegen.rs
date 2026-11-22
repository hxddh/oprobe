//! Code from tuned knobs: the operator spec with the knobs applied, as `op.toml` and as Rust
//! ([`crate::operator::diff`] renders both).

use crate::config::{Knobs, Profile};

/// The profile with `knobs` applied, as TOML (secrets redacted), under a `# header` comment.
pub fn profile_toml(profile: &Profile, knobs: &Knobs, header: &str) -> anyhow::Result<String> {
    let mut p = profile.clone();
    knobs.apply(&mut p);
    Ok(format!("# {header}\n{}", p.to_toml()?))
}

/// The operator of `profile` with `knobs` applied, as Rust builder code.
pub fn rust_snippet(profile: &Profile, knobs: &Knobs) -> String {
    let mut op = profile.operator.redacted();
    knobs.apply_to(&mut op);
    crate::operator::diff::to_rust(&op)
}
