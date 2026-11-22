//! `oprobe target sample TARGET` / MCP `sample_objects` (design v4 §9): a read-only look at
//! what a target holds — the first-level prefixes with their object counts and bytes, the size
//! distribution and a few example keys — so an agent on a read-only server can pick an
//! `--existing PREFIX` to measure instead of hitting a dead end. `doctor` includes one.

use std::collections::BTreeMap;

use anyhow::Result;
use futures::TryStreamExt;
use opendal::EntryMode;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::input::{CostInput, KnobInput, SampleRequest, WorkloadInput};
use crate::config::TargetSpec;
use crate::output::{CommandOutput, NextStep};
use crate::report::style::{bold, dim};
use crate::util::fmt_bytes;

/// Objects listed by default.
pub const DEFAULT_LIMIT: usize = 1000;
/// Example keys kept.
const EXAMPLES: usize = 10;

/// Objects and bytes under one first-level prefix.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PrefixStat {
    /// The prefix, relative to the target root, ending in `/` (`""`: objects at the root).
    pub prefix: String,
    pub objects_count: u64,
    pub total_bytes: u64,
    /// Median object size under the prefix.
    pub median_bytes: u64,
}

/// One size class of the distribution.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SizeBucket {
    /// Upper bound of the class (objects of at most this size; the last class is open).
    pub max_bytes: u64,
    pub objects_count: u64,
}

/// Object sizes of the sample.
#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
pub struct SizeDistribution {
    pub min_bytes: u64,
    pub p50_bytes: u64,
    pub p90_bytes: u64,
    pub max_bytes: u64,
    pub mean_bytes: u64,
    pub buckets: Vec<SizeBucket>,
}

/// An example object.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SampleEntry {
    pub key: String,
    pub size_bytes: u64,
}

/// Result of `oprobe target sample` (MCP `sample_objects`).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ObjectSample {
    /// The target as given.
    pub target: String,
    /// The listed prefix, relative to the target root (`""`: the root).
    pub prefix: String,
    /// Objects examined (at most `limit_count`).
    pub objects_count: u64,
    pub total_bytes: u64,
    /// The listing stopped at the limit: counts are a sample, not the whole target.
    pub truncated: bool,
    pub limit_count: u64,
    /// First-level prefixes under `prefix`, most objects first.
    pub prefixes: Vec<PrefixStat>,
    pub sizes: SizeDistribution,
    /// A few keys (relative to the target root).
    pub examples: Vec<SampleEntry>,
    /// The prefix to measure with `--existing` (the one holding the most objects), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_existing: Option<String>,
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// Size classes: 4 KiB, 64 KiB, 1 MiB, 16 MiB, 256 MiB, larger.
const CLASSES: [u64; 5] = [4 << 10, 64 << 10, 1 << 20, 16 << 20, 256 << 20];

/// Build the sample from listed `(key, size)` pairs.
pub fn summarize(
    target: &str,
    prefix: &str,
    entries: &[(String, u64)],
    limit: usize,
    truncated: bool,
) -> ObjectSample {
    let mut sizes: Vec<u64> = entries.iter().map(|(_, s)| *s).collect();
    sizes.sort_unstable();
    let total: u64 = sizes.iter().sum();
    let mut groups: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for (k, s) in entries {
        let rest = k.strip_prefix(prefix).unwrap_or(k);
        let g = match rest.split_once('/') {
            Some((first, _)) => format!("{prefix}{first}/"),
            None => prefix.to_owned(),
        };
        groups.entry(g).or_default().push(*s);
    }
    let mut prefixes: Vec<PrefixStat> = groups
        .into_iter()
        .map(|(p, mut v)| {
            v.sort_unstable();
            PrefixStat {
                prefix: p,
                objects_count: v.len() as u64,
                total_bytes: v.iter().sum(),
                median_bytes: percentile(&v, 0.5),
            }
        })
        .collect();
    prefixes.sort_by(|a, b| {
        b.objects_count
            .cmp(&a.objects_count)
            .then(a.prefix.cmp(&b.prefix))
    });
    let mut buckets: Vec<SizeBucket> = CLASSES
        .iter()
        .map(|m| SizeBucket {
            max_bytes: *m,
            objects_count: 0,
        })
        .collect();
    buckets.push(SizeBucket {
        max_bytes: u64::MAX,
        objects_count: 0,
    });
    for s in &sizes {
        let i = CLASSES.iter().position(|m| s <= m).unwrap_or(CLASSES.len());
        buckets[i].objects_count += 1;
    }
    let suggested_existing = prefixes
        .iter()
        .find(|p| p.objects_count > 0 && p.total_bytes > 0)
        .map(|p| p.prefix.clone());
    ObjectSample {
        target: target.to_owned(),
        prefix: prefix.to_owned(),
        objects_count: sizes.len() as u64,
        total_bytes: total,
        truncated,
        limit_count: limit as u64,
        prefixes,
        sizes: SizeDistribution {
            min_bytes: sizes.first().copied().unwrap_or(0),
            p50_bytes: percentile(&sizes, 0.5),
            p90_bytes: percentile(&sizes, 0.9),
            max_bytes: sizes.last().copied().unwrap_or(0),
            mean_bytes: if sizes.is_empty() {
                0
            } else {
                total / sizes.len() as u64
            },
            buckets,
        },
        examples: entries
            .iter()
            .take(EXAMPLES)
            .map(|(k, s)| SampleEntry {
                key: k.clone(),
                size_bytes: *s,
            })
            .collect(),
        suggested_existing,
    }
}

/// List up to `limit` objects under `prefix` of `target` (read-only: one recursive listing).
pub async fn sample(
    target: &TargetSpec,
    target_arg: &str,
    prefix: &str,
    limit: usize,
) -> Result<ObjectSample> {
    let mut target = target.clone();
    crate::engine::operator::prepare_target(&mut target, &Default::default()).await?;
    let base = crate::engine::operator::base_operator(&target)?;
    let admin = crate::engine::operator::preflight_operator(&base, &target)?;
    let prefix = prefix.trim_start_matches('/');
    let prefix = if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_owned()
    } else {
        format!("{prefix}/")
    };
    let limit = limit.max(1);
    let mut lister = admin.lister_with(&prefix).recursive(true).await?;
    let mut entries = Vec::new();
    let mut truncated = false;
    while let Some(e) = lister.try_next().await? {
        if e.metadata().mode() != EntryMode::FILE {
            continue;
        }
        if entries.len() >= limit {
            truncated = true;
            break;
        }
        entries.push((e.path().to_owned(), e.metadata().content_length()));
    }
    Ok(summarize(target_arg, &prefix, &entries, limit, truncated))
}

/// The CLI arguments (and MCP arguments) naming a target, for next steps.
fn quoted(t: &str) -> String {
    crate::model::spec::shell_quote(t)
}

impl CommandOutput for ObjectSample {
    fn text(&self) -> String {
        let mut o = format!(
            "{} {}{}\n",
            bold("oprobe target sample"),
            self.target,
            if self.prefix.is_empty() {
                String::new()
            } else {
                format!(" · {}", self.prefix)
            }
        );
        if self.objects_count == 0 {
            o += "  no objects found\n";
            return o;
        }
        o += &format!(
            "  {} object(s), {}{}\n",
            self.objects_count,
            fmt_bytes(self.total_bytes),
            if self.truncated {
                dim(&format!(" (first {} listed)", self.limit_count))
            } else {
                String::new()
            }
        );
        let s = &self.sizes;
        o += &format!(
            "  sizes: min {} · p50 {} · p90 {} · max {}\n",
            fmt_bytes(s.min_bytes),
            fmt_bytes(s.p50_bytes),
            fmt_bytes(s.p90_bytes),
            fmt_bytes(s.max_bytes)
        );
        o += &format!("  {}\n", bold("prefixes"));
        for p in self.prefixes.iter().take(10) {
            o += &format!(
                "    {:<40} {:>7} objects  {:>10}  median {}\n",
                if p.prefix.is_empty() {
                    "(root)"
                } else {
                    &p.prefix
                },
                p.objects_count,
                fmt_bytes(p.total_bytes),
                fmt_bytes(p.median_bytes)
            );
        }
        o += &format!("  {}\n", bold("examples"));
        for e in self.examples.iter().take(5) {
            o += &format!("    {}  {}\n", e.key, dim(&fmt_bytes(e.size_bytes)));
        }
        if let Some(n) = self.next().first() {
            o += &format!("  next: {}\n", bold(&n.cli));
        }
        o
    }

    fn markdown(&self) -> String {
        let mut o = format!("## Objects of `{}`\n\n", self.target);
        if self.objects_count == 0 {
            o += "No objects found.\n";
            return o;
        }
        o += &format!(
            "{} object(s), {}{}.\n\n| Prefix | Objects | Bytes | Median |\n|---|---:|---:|---:|\n",
            self.objects_count,
            fmt_bytes(self.total_bytes),
            if self.truncated {
                format!(" (first {} listed)", self.limit_count)
            } else {
                String::new()
            }
        );
        for p in &self.prefixes {
            o += &format!(
                "| `{}` | {} | {} | {} |\n",
                if p.prefix.is_empty() { "/" } else { &p.prefix },
                p.objects_count,
                fmt_bytes(p.total_bytes),
                fmt_bytes(p.median_bytes)
            );
        }
        o
    }

    fn formats() -> &'static [crate::output::OutputFormat] {
        crate::output::WITH_MD
    }

    fn next(&self) -> Vec<NextStep> {
        let t = &self.target;
        match &self.suggested_existing {
            Some(p) => vec![
                NextStep::new(
                    format!("measure the objects under {p} (read-only)"),
                    format!("oprobe run {} --existing {} -d 10s", quoted(t), quoted(p)),
                )
                .with_mcp(
                    "start_run",
                    serde_json::json!({ "target": t, "existing": p, "duration": "10s" }),
                ),
                NextStep::new(
                    "plan it first (nothing touches the target)",
                    format!(
                        "oprobe run {} --existing {} -d 10s --dry-run",
                        quoted(t),
                        quoted(p)
                    ),
                )
                .with_mcp(
                    "plan_run",
                    serde_json::json!({ "target": t, "existing": p, "duration": "10s" }),
                ),
            ],
            None => vec![
                NextStep::new(
                    "nothing to read here: plan a run with a generated dataset",
                    format!("oprobe run {} --dry-run", quoted(t)),
                )
                .with_mcp("plan_run", serde_json::json!({ "target": t })),
            ],
        }
    }
}

/// `oprobe target sample TARGET`.
pub async fn run(ctx: &Ctx, a: SampleRequest) -> Result<ObjectSample> {
    let (profile, _) = super::profile::resolve_profile(
        ctx,
        &a.target,
        &WorkloadInput::default(),
        &KnobInput::default(),
        &CostInput::default(),
    )?;
    let target = profile.target.normalized()?;
    let arg = a.target.target.clone().unwrap_or_else(|| target.display());
    let s = sample(&target, &arg, a.prefix.as_deref().unwrap_or(""), a.limit).await?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_group_by_first_level_prefix() {
        let e: Vec<(String, u64)> = vec![
            ("a/1.parquet".into(), 10 << 20),
            ("a/2.parquet".into(), 12 << 20),
            ("a/x/3.parquet".into(), 8 << 20),
            ("b/log.txt".into(), 100),
            ("top.bin".into(), 5000),
        ];
        let s = summarize("s3://b", "", &e, 1000, false);
        assert_eq!(s.objects_count, 5);
        assert_eq!(s.prefixes[0].prefix, "a/");
        assert_eq!(s.prefixes[0].objects_count, 3);
        assert_eq!(s.suggested_existing.as_deref(), Some("a/"));
        assert_eq!(s.sizes.min_bytes, 100);
        assert_eq!(s.sizes.max_bytes, 12 << 20);
        assert_eq!(
            s.sizes.buckets.iter().map(|b| b.objects_count).sum::<u64>(),
            5
        );
        let n = s.next();
        assert!(n[0].cli.contains("--existing a/"), "{n:?}");
        assert_eq!(n[0].mcp.as_ref().unwrap().arguments["existing"], "a/");
        let empty = summarize("memory://", "", &[], 10, false);
        assert!(empty.suggested_existing.is_none());
        assert_eq!(empty.next()[0].mcp.as_ref().unwrap().tool, "plan_run");
    }
}
