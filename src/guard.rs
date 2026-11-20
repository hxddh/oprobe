//! Safety rails (design §10): live budgets, the `--dry-run` plan, `--read-only`, deterministic
//! run prefixes and the preflight check that runs before anything is written.
//!
//! Budgets are enforced by the engine: the recorder feeds every completed operation's requests
//! and bytes to [`Guard::add`] (all phases), the runner's sampler calls [`Guard::tick`]; the
//! first breach cancels the run, and the command fails with `safety.budget_exceeded` (exit 4).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::{OpKind, Profile};
use crate::engine::Cancel;
use crate::errors::{ErrorCode, OprobeError};

/// Limits for one command.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Budgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<f64>,
}

impl Budgets {
    pub fn is_empty(&self) -> bool {
        self.max_cost_usd.is_none()
            && self.max_requests_count.is_none()
            && self.max_bytes.is_none()
            && self.max_duration_s.is_none()
    }
}

/// Which budget stopped the run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BudgetBreach {
    /// `requests`, `bytes`, `duration` or `cost`.
    pub budget: String,
    pub limit: f64,
    pub observed: f64,
}

impl BudgetBreach {
    pub fn message(&self) -> String {
        match self.budget.as_str() {
            "requests" => format!(
                "request budget exceeded: {} requests (limit {})",
                self.observed, self.limit
            ),
            "bytes" => format!(
                "byte budget exceeded: {} transferred (limit {})",
                crate::util::fmt_bytes(self.observed as u64),
                crate::util::fmt_bytes(self.limit as u64)
            ),
            "duration" => format!(
                "duration budget exceeded: {:.1}s (limit {:.1}s)",
                self.observed, self.limit
            ),
            _ => format!(
                "cost budget exceeded: ~${:.4} (limit ${:.4})",
                self.observed, self.limit
            ),
        }
    }

    pub fn to_error(&self) -> anyhow::Error {
        OprobeError::new(ErrorCode::SafetyBudgetExceeded, self.message())
            .with_hint("raise the budget (--max-requests, --max-bytes, --max-duration, --max-cost-usd) or shorten the run")
            .into()
    }
}

/// Live budget enforcement shared by the recorder and the runner.
#[derive(Debug)]
pub struct Guard {
    budgets: Budgets,
    /// Estimated USD per request (conservative: the write-class price) where cost applies.
    usd_per_request: Option<f64>,
    started: Instant,
    requests: AtomicU64,
    bytes: AtomicU64,
    breach: Mutex<Option<BudgetBreach>>,
    tripped: std::sync::atomic::AtomicBool,
    cancel: OnceLock<Cancel>,
}

impl Guard {
    pub fn new(budgets: Budgets) -> Arc<Self> {
        Arc::new(Self {
            budgets,
            usd_per_request: None,
            started: Instant::now(),
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            breach: Mutex::new(None),
            tripped: std::sync::atomic::AtomicBool::new(false),
            cancel: OnceLock::new(),
        })
    }

    /// A guard whose cost budget uses `usd_per_request`.
    pub fn with_pricing(budgets: Budgets, usd_per_request: Option<f64>) -> Arc<Self> {
        let mut g = Self::new(budgets);
        Arc::get_mut(&mut g).expect("fresh").usd_per_request = usd_per_request;
        g
    }

    pub fn budgets(&self) -> &Budgets {
        &self.budgets
    }

    /// Cancel `cancel` when a budget is exceeded.
    pub fn arm(&self, cancel: Cancel) {
        let _ = self.cancel.set(cancel);
    }

    /// Account requests and bytes (hot path: two atomic adds and a few compares).
    #[inline]
    pub fn add(&self, requests: u64, bytes: u64) {
        let r = self.requests.fetch_add(requests, Ordering::Relaxed) + requests;
        let b = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if let Some(max) = self.budgets.max_requests_count
            && r > max
        {
            self.trip("requests", max as f64, r as f64);
        }
        if let Some(max) = self.budgets.max_bytes
            && b > max
        {
            self.trip("bytes", max as f64, b as f64);
        }
        if let (Some(max), Some(p)) = (self.budgets.max_cost_usd, self.usd_per_request) {
            let usd = r as f64 * p;
            if usd > max {
                self.trip("cost", max, usd);
            }
        }
        self.tick();
    }

    /// Check the wall-clock budget.
    #[inline]
    pub fn tick(&self) {
        if let Some(max) = self.budgets.max_duration_s {
            let s = self.started.elapsed().as_secs_f64();
            if s > max {
                self.trip("duration", max, s);
            }
        }
    }

    fn trip(&self, budget: &str, limit: f64, observed: f64) {
        if self.tripped.swap(true, Ordering::AcqRel) {
            return;
        }
        *self.breach.lock().unwrap_or_else(|e| e.into_inner()) = Some(BudgetBreach {
            budget: budget.to_owned(),
            limit,
            observed,
        });
        if let Some(c) = self.cancel.get() {
            c.cancel();
        }
    }

    pub fn tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    pub fn breach(&self) -> Option<BudgetBreach> {
        self.breach
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Requests and bytes accounted so far.
    pub fn usage(&self) -> (u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }

    /// Time until the duration budget runs out.
    pub fn remaining_time(&self) -> Option<Duration> {
        self.budgets
            .max_duration_s
            .map(|m| Duration::from_secs_f64(m).saturating_sub(self.started.elapsed()))
    }
}

/// Estimated USD per request for a target (write-class list price, conservative), when a
/// price table applies.
pub fn usd_per_request(profile: &Profile) -> Option<f64> {
    let t = &profile.target;
    let key = profile.cost.provider.clone().or_else(|| {
        crate::analysis::cost::detect_provider(&t.scheme, t.endpoint().as_deref())
            .map(str::to_owned)
    })?;
    let p = profile
        .cost
        .pricing
        .clone()
        .or_else(|| crate::analysis::cost::preset(&key))?;
    Some(p.write_per_million.max(p.read_per_million) / 1e6)
}

/// `--read-only` (implied by `--existing`): refuse a run that would write or delete.
pub fn check_read_only(profile: &Profile) -> anyhow::Result<()> {
    let ds = profile.workload.effective_dataset();
    if ds.existing_prefix.is_some() {
        // Write operations are removed from the mix of an existing dataset; nothing is written.
        return Ok(());
    }
    let mix = profile.workload.effective_mix();
    let writes = mix.iter().any(|m| m.weight > 0 && m.op.is_write());
    let needs_dataset = mix.iter().any(|m| m.weight > 0 && !m.op.is_write());
    let what = match (needs_dataset, writes) {
        (true, true) => "it would upload a test dataset and run write operations",
        (true, false) => "it would upload a test dataset",
        (false, true) => "its workload writes objects",
        (false, false) => return Ok(()),
    };
    Err(OprobeError::new(
        ErrorCode::SafetyReadOnly,
        format!("read-only: refusing the run, {what}"),
    )
    .with_hint("benchmark existing objects with `--existing PREFIX`, or drop `--read-only`")
    .into())
}

/// What a run would do (`--dry-run`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Plan {
    pub target: String,
    pub preset: String,
    pub run_prefix: String,
    pub read_only: bool,
    pub concurrency_count: u64,
    pub warmup_s: f64,
    pub duration_s: f64,
    /// Estimated logical operations (warmup + window).
    pub estimated_ops_count: u64,
    /// Estimated backend requests (dataset upload + workload + cleanup).
    pub estimated_requests_count: u64,
    /// Estimated bytes transferred (up + down).
    pub estimated_bytes: u64,
    pub objects_written_count: u64,
    pub objects_deleted_count: u64,
    /// Request cost at list prices, where a price table applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    /// Estimated wall time, dataset upload and cleanup included.
    pub eta_s: f64,
    pub budgets: Budgets,
    /// How the estimates were made.
    pub assumptions: Vec<String>,
}

/// Build the plan of a run without touching the target.
pub fn plan(profile: &Profile, run_prefix: &str, read_only: bool, budgets: &Budgets) -> Plan {
    let w = &profile.workload;
    let k = &profile.knobs;
    let ds = w.effective_dataset();
    let mut mix = w.effective_mix();
    if ds.existing_prefix.is_some() {
        mix.retain(|m| !m.op.is_write());
    }
    let network = profile.target.is_network();
    let total_w: u64 = mix.iter().map(|m| m.weight as u64).sum::<u64>().max(1);
    let chunk = k.chunk_size.0.max(1);
    let obj = ds.object_size.0;
    // Requests and bytes of one operation of each kind.
    let per_op = |op: &OpKind| -> (f64, f64, bool) {
        match op {
            OpKind::Read { size } => {
                let len = size.map(|s| s.0.min(obj)).unwrap_or(obj);
                (len.div_ceil(chunk).max(1) as f64, len as f64, false)
            }
            OpKind::ParquetScan {
                columns,
                column_size,
                footer_size,
                suffix,
            } => (
                if *suffix { 1.0 } else { 2.0 } + *columns as f64,
                footer_size.0 as f64 + *columns as f64 * column_size.0 as f64,
                false,
            ),
            OpKind::Write { size } => (1.0, size.0 as f64, true),
            OpKind::MultipartWrite { size } => {
                let parts = size.0.div_ceil(chunk).max(1);
                (
                    if parts > 1 { parts as f64 + 2.0 } else { 1.0 },
                    size.0 as f64,
                    true,
                )
            }
            OpKind::Stat => (1.0, 0.0, false),
            OpKind::List { .. } => (1.0, 4096.0, false),
            OpKind::ListPaged { limit } => {
                let per_part = (ds.objects / ds.partitions.max(1)).max(1) as f64;
                (
                    (per_part / (*limit).max(1) as f64).ceil().max(1.0),
                    4096.0,
                    false,
                )
            }
            // One batch request (or one request per key where the service has no batches).
            OpKind::Delete { .. } => (1.0, 0.0, true),
            OpKind::Copy => (1.0, 0.0, true),
            OpKind::PresignedRead => (1.0, obj as f64, false),
            OpKind::ConditionalWrite { size } => (1.0, size.0 as f64, true),
            OpKind::MetadataWrite { size, .. } => (1.0, size.0 as f64, true),
        }
    };
    let (mut req_per_op, mut bytes_per_op, mut write_share) = (0.0, 0.0, 0.0);
    for m in &mix {
        let f = m.weight as f64 / total_w as f64;
        let (r, b, wr) = per_op(&m.op);
        req_per_op += f * r;
        bytes_per_op += f * b;
        if wr {
            write_share += f;
        }
    }
    let mut assumptions = Vec::new();
    let op_latency_s = if network {
        let transfer = bytes_per_op / (100.0 * 1024.0 * 1024.0);
        assumptions.push(
            "network operations take ~30 ms plus transfer at 100 MiB/s per operation".to_owned(),
        );
        0.030 + transfer
    } else {
        assumptions.push("in-process operations take ~0.2 ms".to_owned());
        0.0002
    };
    let window = w.warmup.get().as_secs_f64() + w.duration.get().as_secs_f64();
    let conc = k.concurrency.max(1) as f64;
    let mut ops = match w.rate {
        Some(rate) => rate * window,
        None => conc * window / op_latency_s,
    };
    if let Some(m) = w.max_ops {
        ops = ops.min(m as f64 + conc * w.warmup.get().as_secs_f64() / op_latency_s);
    }
    let ops = ops.max(0.0).round() as u64;

    let needs_dataset = ds.existing_prefix.is_none() && mix.iter().any(|m| !m.op.is_write());
    let dataset_objects = if needs_dataset { ds.objects as u64 } else { 0 };
    let dataset_bytes = dataset_objects * obj;
    let dataset_requests = dataset_objects * obj.div_ceil(8 * 1024 * 1024).max(1);
    let scratch = if write_share > 0.0 {
        ((ops as f64 * write_share) as u64).min(k.concurrency as u64 * 4)
    } else {
        0
    };
    let written = dataset_objects + scratch;
    let deleted = if w.cleanup && ds.existing_prefix.is_none() && !ds.persist {
        written
    } else {
        scratch
    };
    let cleanup_requests = if written > 0 {
        2 + deleted.div_ceil(1000)
    } else {
        0
    };
    let requests = dataset_requests + (ops as f64 * req_per_op).round() as u64 + cleanup_requests;
    let bytes = dataset_bytes + (ops as f64 * bytes_per_op).round() as u64;
    let prepare_s = if network {
        dataset_bytes as f64 / (50.0 * 1024.0 * 1024.0) + dataset_objects as f64 * 0.01
    } else {
        dataset_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    };
    if needs_dataset && network {
        assumptions.push("the dataset uploads at ~50 MiB/s".to_owned());
    }
    let cleanup_s = if network {
        cleanup_requests as f64 * 0.1
    } else {
        0.0
    };
    Plan {
        target: profile.target.display(),
        preset: w.preset.slug().to_owned(),
        run_prefix: run_prefix.to_owned(),
        read_only,
        concurrency_count: k.concurrency as u64,
        warmup_s: w.warmup.get().as_secs_f64(),
        duration_s: w.duration.get().as_secs_f64(),
        estimated_ops_count: ops,
        estimated_requests_count: requests,
        estimated_bytes: bytes,
        objects_written_count: written,
        objects_deleted_count: deleted,
        estimated_cost_usd: usd_per_request(profile).map(|p| p * requests as f64),
        eta_s: prepare_s + window + cleanup_s,
        budgets: budgets.clone(),
        assumptions,
    }
}

/// Outcome of [`preflight`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Preflight {
    /// Listing the target root worked.
    pub list: bool,
    /// A tiny object could be written and deleted under the run prefix.
    pub write_delete: Option<bool>,
}

/// Credentials and permissions, before any write: list the root, then (when the run writes)
/// write and delete a 1-byte object under the run prefix. Fails fast with a classified error.
pub async fn preflight(
    admin: &opendal::Operator,
    run_prefix: &str,
    write_probe: bool,
) -> anyhow::Result<Preflight> {
    let classify = |what: &str, e: opendal::Error| -> anyhow::Error {
        let code = match e.kind() {
            opendal::ErrorKind::PermissionDenied => ErrorCode::AuthDenied,
            opendal::ErrorKind::NotFound => ErrorCode::StorageNotFound,
            opendal::ErrorKind::Unsupported => ErrorCode::StorageUnsupported,
            opendal::ErrorKind::RateLimited => ErrorCode::StorageThrottled,
            opendal::ErrorKind::ConfigInvalid => ErrorCode::ConfigInvalid,
            _ => ErrorCode::from_class(&crate::engine::ops::error_class_of(&e, None)),
        };
        OprobeError::new(code, format!("preflight: cannot {what}: {e}"))
            .with_hint("check credentials and permissions with `oprobe doctor TARGET`")
            .into()
    };
    let mut out = Preflight::default();
    admin
        .list_with("")
        .limit(1)
        .await
        .map_err(|e| classify("list the target", e))?;
    out.list = true;
    if write_probe {
        let key = format!("{run_prefix}.oprobe-preflight");
        admin
            .write(&key, vec![0u8])
            .await
            .map_err(|e| classify("write a test object", e))?;
        admin
            .delete(&key)
            .await
            .map_err(|e| classify("delete a test object", e))?;
        out.write_delete = Some(true);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_budget_trips_once_and_cancels() {
        let g = Guard::new(Budgets {
            max_requests_count: Some(10),
            ..Default::default()
        });
        let c = Cancel::new();
        g.arm(c.clone());
        g.add(10, 0);
        assert!(!g.tripped());
        g.add(1, 0);
        assert!(g.tripped());
        assert!(c.is_cancelled());
        let b = g.breach().unwrap();
        assert_eq!(b.budget, "requests");
        g.add(100, 0);
        assert_eq!(g.breach().unwrap().observed, 11.0);
        let e = b.to_error();
        assert_eq!(
            crate::errors::classify(&e, "s3").code,
            ErrorCode::SafetyBudgetExceeded
        );
    }

    #[test]
    fn cost_budget_uses_the_price_per_request() {
        let g = Guard::with_pricing(
            Budgets {
                max_cost_usd: Some(0.01),
                ..Default::default()
            },
            Some(5e-6),
        );
        g.add(2000, 0);
        assert!(!g.tripped());
        g.add(1, 0);
        assert_eq!(g.breach().unwrap().budget, "cost");
    }

    #[test]
    fn read_only_refuses_generated_datasets_but_not_existing_ones() {
        let mut p = Profile::from_target(crate::config::TargetSpec::from_url("memory://").unwrap());
        let e = check_read_only(&p).unwrap_err();
        assert_eq!(
            crate::errors::classify(&e, "memory").code,
            ErrorCode::SafetyReadOnly
        );
        let mut ds = p.workload.effective_dataset();
        ds.existing_prefix = Some("data/".into());
        p.workload.dataset = Some(ds);
        check_read_only(&p).unwrap();
    }

    #[test]
    fn plan_counts_dataset_writes_and_cleanup() {
        let mut p = Profile::from_target(crate::config::TargetSpec::from_url("memory://").unwrap());
        p.workload.max_ops = Some(100);
        p.workload.warmup = crate::util::HumanDuration(Duration::ZERO);
        let pl = plan(&p, "oprobe-run-x/", false, &Budgets::default());
        let ds = p.workload.effective_dataset();
        assert!(pl.objects_written_count >= ds.objects as u64);
        assert_eq!(pl.objects_deleted_count, pl.objects_written_count);
        assert!(pl.estimated_ops_count <= 100);
        assert!(pl.estimated_requests_count >= ds.objects as u64);
        assert!(pl.estimated_cost_usd.is_none(), "memory has no price table");
        assert!(pl.eta_s > 0.0);
    }
}
