//! Distributed load (design v5 §8). Owned by stream SCALE.
//!
//! The foundation only defines the worker set a [`crate::app::RunRequest`] carries
//! (`--workers host:port,…` or `local:N`); a run with workers is refused until SCALE adds the
//! coordinator and `oprobe worker`.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::errors::{ErrorCode, OprobeError};

/// One remote worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkerAddr {
    /// `host:port` of `oprobe worker --listen`.
    pub addr: String,
    /// Share of the load (concurrency, rate and dataset partitions), relative to the others.
    #[serde(default = "unit")]
    pub weight: f64,
}

fn unit() -> f64 {
    1.0
}

/// The workers of a distributed run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkerSet {
    #[serde(default)]
    pub workers: Vec<WorkerAddr>,
    /// `local:N`: spawn N local worker processes instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<u32>,
    /// The shared token (`--token-file`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<PathBuf>,
}

impl WorkerSet {
    /// Parse `--workers`: `local:N` or `host:port[,host:port…]`.
    pub fn parse(v: &str) -> Result<Self, OprobeError> {
        let v = v.trim();
        let bad = |m: String| {
            OprobeError::new(ErrorCode::UsageInvalidArgument, m)
                .with_hint("use host:port,host:port or local:N")
        };
        if let Some(n) = v.strip_prefix("local:") {
            let n: u32 = n
                .trim()
                .parse()
                .ok()
                .filter(|n| *n >= 1)
                .ok_or_else(|| bad(format!("invalid worker count in `{v}`")))?;
            return Ok(Self {
                local: Some(n),
                ..Self::default()
            });
        }
        let workers: Vec<WorkerAddr> = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|a| {
                if a.rsplit_once(':')
                    .is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok())
                {
                    Ok(WorkerAddr {
                        addr: a.to_owned(),
                        weight: 1.0,
                    })
                } else {
                    Err(bad(format!("invalid worker address `{a}`")))
                }
            })
            .collect::<Result<_, _>>()?;
        if workers.is_empty() {
            return Err(bad("no workers given".into()));
        }
        Ok(Self {
            workers,
            ..Self::default()
        })
    }

    /// Refuse distributed runs until the coordinator exists.
    pub fn check_supported(&self) -> Result<(), OprobeError> {
        Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            "distributed runs (--workers) are not available in this build",
        )
        .with_hint("run without --workers"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worker_sets() {
        assert_eq!(WorkerSet::parse("local:3").unwrap().local, Some(3));
        let w = WorkerSet::parse("a:1, b:2").unwrap();
        assert_eq!(w.workers.len(), 2);
        assert!(WorkerSet::parse("nope").is_err());
        assert!(WorkerSet::parse("local:0").is_err());
        assert!(w.check_supported().is_err());
    }
}
