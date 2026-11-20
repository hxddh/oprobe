//! Run identifiers: `YYYYMMDD-HHMMSS-xxxx` (UTC start time + 16 random bits).

use std::fmt;
use std::time::SystemTime;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A run id such as `20261120-143005-a1b2`. Sorting ids sorts runs by start time.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct RunId(String);

impl RunId {
    /// A fresh id for a run starting now.
    pub fn new() -> Self {
        Self::at(SystemTime::now(), rand::random())
    }

    /// The id of a run started at `t` with random tag `tag`.
    pub fn at(t: SystemTime, tag: u16) -> Self {
        let rfc = crate::util::fmt_rfc3339(t); // 2026-11-20T14:30:05Z
        let d = rfc.replace(['-', ':'], ""); // 20261120T143005Z
        Self(format!("{}-{}-{tag:04x}", &d[..8], &d[9..15]))
    }

    /// Parse an id; `None` unless it has exactly the `YYYYMMDD-HHMMSS-xxxx` shape.
    pub fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        let ok = b.len() == 20
            && b[..8].iter().all(u8::is_ascii_digit)
            && b[8] == b'-'
            && b[9..15].iter().all(u8::is_ascii_digit)
            && b[15] == b'-'
            && b[16..]
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c));
        ok.then(|| Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The UTC start time encoded in the id (second precision).
    pub fn started_at(&self) -> Option<SystemTime> {
        let s = &self.0;
        let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<u32>().ok();
        crate::util::utc_time(
            n(0..4)? as i64,
            n(4..6)?,
            n(6..8)?,
            n(9..11)?,
            n(11..13)?,
            n(13..15)?,
        )
    }

    /// The object prefix (relative to the target root) this run writes under.
    pub fn run_prefix(&self) -> String {
        format!("{}{}/", crate::engine::cleanup::RUN_PREFIX, self.0)
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for RunId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Self::parse(&s).ok_or_else(|| format!("`{s}` is not a run id (YYYYMMDD-HHMMSS-xxxx)"))
    }
}

impl From<RunId> for String {
    fn from(r: RunId) -> String {
        r.0
    }
}

/// Deterministic run prefix for `--idempotency-key K`: a retried command reuses (and cleans
/// up) the same prefix instead of leaving a second copy of its test data behind.
pub fn idempotent_run_prefix(key: &str) -> String {
    let slug: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    format!(
        "{}key-{}-{:08x}/",
        crate::engine::cleanup::RUN_PREFIX,
        slug.trim_matches('-'),
        fnv1a(key.as_bytes()) as u32
    )
}

/// 64-bit FNV-1a: a small stable hash (not cryptographic) for ids and fingerprints.
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_sort_by_time() {
        let t = crate::util::utc_time(2026, 11, 20, 14, 30, 5).unwrap();
        let id = RunId::at(t, 0xa1b2);
        assert_eq!(id.as_str(), "20261120-143005-a1b2");
        assert_eq!(RunId::parse(id.as_str()), Some(id.clone()));
        assert_eq!(id.started_at(), Some(t));
        assert!(RunId::at(t + std::time::Duration::from_secs(1), 0) > id);
        for bad in [
            "",
            "20261120-143005",
            "20261120_143005-a1b2",
            "20261120-143005-A1B2",
        ] {
            assert!(RunId::parse(bad).is_none(), "{bad}");
        }
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"20261120-143005-a1b2\""
        );
        assert_eq!(id.run_prefix(), "oprobe-run-20261120-143005-a1b2/");
    }

    #[test]
    fn idempotency_keys_give_stable_prefixes() {
        let a = idempotent_run_prefix("CI build #42");
        assert_eq!(a, idempotent_run_prefix("CI build #42"));
        assert_ne!(a, idempotent_run_prefix("CI build #43"));
        assert!(a.starts_with("oprobe-run-key-ci-build--42-"), "{a}");
        assert!(a.ends_with('/'));
    }
}
