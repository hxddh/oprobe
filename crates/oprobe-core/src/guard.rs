//! Safety rails (design §10): live budgets, the `--dry-run` plan, `--read-only`, deterministic
//! run prefixes and the preflight check that runs before anything is written.
//!
//! Budgets are enforced on the wire. Every operator a run talks to its target with before
//! cleanup (reachability probes, preflight, dataset preparation, warm-up, the window and
//! verification) sends its requests through [`crate::engine::gate`]: a request is admitted
//! by [`Guard::admit`] when it is sent (so retries and re-sent requests count, each with its
//! body), response bytes are counted by [`Guard::add_bytes`] as they stream, and a request the
//! budget has no room for is never sent. In-process services (`memory://`, `fs://`) have no
//! HTTP: the recorder feeds their completed calls to [`Guard::add`]. The runner's sampler
//! calls [`Guard::tick`]. Cleanup (deleting the run prefix, aborting multipart uploads) is
//! never counted and never refused. The first breach cancels the run, which cuts what is in
//! flight, and the command fails with `safety.budget_exceeded` (exit 4).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    pub max_requests_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<f64>,
}

impl Budgets {
    /// Every limit `self` leaves unset taken from `defaults`.
    pub fn or(self, defaults: &Budgets) -> Self {
        Self {
            max_requests_count: self.max_requests_count.or(defaults.max_requests_count),
            max_bytes: self.max_bytes.or(defaults.max_bytes),
            max_duration_s: self.max_duration_s.or(defaults.max_duration_s),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.max_requests_count.is_none()
            && self.max_bytes.is_none()
            && self.max_duration_s.is_none()
    }
}

/// Which budget stopped the run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BudgetBreach {
    /// `requests`, `bytes` or `duration`.
    pub budget: String,
    pub limit: f64,
    pub observed: f64,
    /// The body of the request the byte budget refused because it did not fit in what was
    /// left (the budget itself was not spent yet: `observed` is below `limit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_request_bytes: Option<u64>,
}

impl BudgetBreach {
    pub fn message(&self) -> String {
        match (self.budget.as_str(), self.refused_request_bytes) {
            ("requests", _) => format!(
                "request budget spent: {} requests (limit {})",
                self.observed, self.limit
            ),
            // Not "spent": a 4 MiB budget with 1 B used refused an 8 MiB part.
            ("bytes", Some(body)) => format!(
                "the next request ({}) does not fit the byte budget: {} of {} used",
                crate::util::fmt_bytes(body),
                crate::util::fmt_bytes(self.observed as u64),
                crate::util::fmt_bytes(self.limit as u64)
            ),
            ("bytes", None) => format!(
                "byte budget spent: {} transferred (limit {})",
                crate::util::fmt_bytes(self.observed as u64),
                crate::util::fmt_bytes(self.limit as u64)
            ),
            _ => format!(
                "duration budget exceeded: {:.1}s (limit {:.1}s)",
                self.observed, self.limit
            ),
        }
    }

    pub fn to_error(&self) -> anyhow::Error {
        OprobeError::new(ErrorCode::SafetyBudgetExceeded, self.message())
            .with_hint(
                "raise the budget (--max-requests, --max-bytes, --max-duration) or shorten the run",
            )
            .into()
    }
}

/// Live budget enforcement shared by the recorder and the runner.
#[derive(Debug)]
pub struct Guard {
    budgets: Budgets,
    started: Instant,
    requests: AtomicU64,
    bytes: AtomicU64,
    breach: Mutex<Option<BudgetBreach>>,
    /// A budget is spent: no further work starts.
    tripped: std::sync::atomic::AtomicBool,
    /// A budget was exceeded (more than its limit was used).
    exceeded: std::sync::atomic::AtomicBool,
    /// A spent budget stopped work that was still to be done ([`Guard::note_cut_short`]).
    cut_short: std::sync::atomic::AtomicBool,
    /// The measurement currently enforced (re-armed by each session sharing this guard).
    cancel: Mutex<Option<Cancel>>,
}

impl oprobe_trace::Budget for Guard {
    fn add(&self, requests: u64, bytes: u64) {
        Guard::add(self, requests, bytes);
    }
}

/// Move `counter` to `room(current)` atomically: `Ok(previous)` when there was room,
/// `Err(current)` when `room` refused. (The standard library's own method for this was renamed
/// between the minimum supported Rust and current stable; this loop builds on both.)
fn take_room(counter: &AtomicU64, room: impl Fn(u64) -> Option<u64>) -> Result<u64, u64> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = room(current).ok_or(current)?;
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(previous) => return Ok(previous),
            Err(seen) => current = seen,
        }
    }
}

impl Guard {
    /// The byte budget, if any.
    pub fn max_bytes(&self) -> Option<u64> {
        self.budgets.max_bytes
    }

    pub fn new(budgets: Budgets) -> Arc<Self> {
        Arc::new(Self {
            budgets,
            started: Instant::now(),
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            breach: Mutex::new(None),
            tripped: std::sync::atomic::AtomicBool::new(false),
            exceeded: std::sync::atomic::AtomicBool::new(false),
            cut_short: std::sync::atomic::AtomicBool::new(false),
            cancel: Mutex::new(None),
        })
    }

    /// Cancel `cancel` when a budget is exceeded. A guard shared by several measurements
    /// (one aggregate budget for `run --repeat`, `matrix`) is re-armed by each of them; a
    /// guard that has already tripped cancels at once.
    pub fn arm(&self, cancel: Cancel) {
        if self.tripped() {
            cancel.cancel();
        }
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel);
    }

    /// Account the requests and bytes of a completed in-process call (hot path: two atomic
    /// adds and a few compares). A budget stops the run when it is spent: `--max-requests 500`
    /// allows 500 requests, not 501. Spending it exactly is not a breach by itself: only
    /// exceeding it, or work it stopped ([`Guard::note_cut_short`]), is.
    #[inline]
    pub fn add(&self, requests: u64, bytes: u64) {
        let r = self.requests.fetch_add(requests, Ordering::Relaxed) + requests;
        let b = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if let Some(max) = self.budgets.max_requests_count
            && r >= max
        {
            self.trip(Self::spent("requests", max, r), r > max, true);
        }
        if let Some(max) = self.budgets.max_bytes
            && b >= max
        {
            self.trip(Self::spent("bytes", max, b), b > max, true);
        }
        self.tick();
    }

    /// Admit one HTTP request with a body of `body_bytes`, as it is about to be sent: it is
    /// counted now, whatever becomes of it. A request the budgets have no room for is refused
    /// (`Err`: it must not be sent), which is work the budget stopped: the run is cancelled
    /// and the budget breached. The request that spends a budget exactly is admitted and
    /// cancels nothing: what is in flight is within the budget and may finish.
    ///
    /// Once the byte budget is spent, every request is refused, one without a body too: its
    /// response would be cut at its first chunk, after the target has done the work.
    pub fn admit(&self, body_bytes: u64) -> Result<(), BudgetBreach> {
        self.tick();
        let mut spent = None;
        match self.budgets.max_bytes {
            Some(max) => {
                let room = |b: u64| {
                    let fits = b.checked_add(body_bytes).filter(|n| *n <= max);
                    fits.filter(|_| b < max)
                };
                match take_room(&self.bytes, room) {
                    Ok(b) if b + body_bytes == max => spent = Some(("bytes", max)),
                    Ok(_) => {}
                    Err(b) => {
                        let mut breach = Self::spent("bytes", max, b);
                        // The budget has room, but not for this body.
                        breach.refused_request_bytes = (b < max).then_some(body_bytes);
                        return Err(self.refuse(breach));
                    }
                }
            }
            None => {
                self.bytes.fetch_add(body_bytes, Ordering::Relaxed);
            }
        }
        match self.budgets.max_requests_count {
            Some(max) => {
                let room = |r: u64| (r < max).then_some(r + 1);
                match take_room(&self.requests, room) {
                    Ok(r) if r + 1 == max => spent = Some(("requests", max)),
                    Ok(_) => {}
                    Err(r) => {
                        // The request is not sent: its body is not transferred either.
                        self.bytes.fetch_sub(body_bytes, Ordering::AcqRel);
                        return Err(self.refuse(Self::spent("requests", max, r)));
                    }
                }
            }
            None => {
                self.requests.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some((budget, max)) = spent {
            self.trip(Self::spent(budget, max, max), false, false);
        }
        Ok(())
    }

    /// `budget` with `observed` of its `limit` used.
    fn spent(budget: &str, limit: u64, observed: u64) -> BudgetBreach {
        BudgetBreach {
            budget: budget.to_owned(),
            limit: limit as f64,
            observed: observed as f64,
            refused_request_bytes: None,
        }
    }

    /// Count `bytes` of a response body as they arrive. Bytes past the budget were
    /// transferred all the same (they are counted), but the stream must stop there (`Err`):
    /// the overshoot is at most the chunks in flight.
    pub fn add_bytes(&self, bytes: u64) -> Result<(), BudgetBreach> {
        let b = self.bytes.fetch_add(bytes, Ordering::AcqRel) + bytes;
        if let Some(max) = self.budgets.max_bytes
            && b >= max
        {
            let breach = Self::spent("bytes", max, b);
            if b > max {
                return Err(self.trip(breach, true, true));
            }
            self.trip(breach, false, false);
        }
        Ok(())
    }

    /// A budget with no room left refused work: the run stops and the budget is breached.
    fn refuse(&self, breach: BudgetBreach) -> BudgetBreach {
        self.note_cut_short();
        self.trip(breach, false, true)
    }

    /// Check the wall-clock budget.
    #[inline]
    pub fn tick(&self) {
        if let Some(max) = self.budgets.max_duration_s {
            let s = self.started.elapsed().as_secs_f64();
            if s > max {
                self.trip(
                    BudgetBreach {
                        budget: "duration".to_owned(),
                        limit: max,
                        observed: s,
                        refused_request_bytes: None,
                    },
                    true,
                    true,
                );
            }
        }
    }

    /// A budget is spent as `breach` says (`exceeded`: more than its limit was used). `stop`
    /// cancels the armed measurement: always, except for the HTTP request that spends a budget
    /// exactly. Returns the breach on record: the first one, which `breach` is when no budget
    /// was spent before.
    fn trip(&self, breach: BudgetBreach, exceeded: bool, stop: bool) -> BudgetBreach {
        if exceeded {
            self.exceeded.store(true, Ordering::Release);
        }
        let on_record = {
            let mut first = self.breach.lock().unwrap_or_else(|e| e.into_inner());
            // Under the lock: whoever reads `tripped` as set finds the breach.
            self.tripped.store(true, Ordering::Release);
            first.get_or_insert(breach).clone()
        };
        if stop
            && let Some(c) = self
                .cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
        {
            c.cancel();
        }
        on_record
    }

    /// A budget is spent: nothing more may start (work it stops cancels the measurement).
    pub fn tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    /// Work that was still to be done was stopped (the cancellation cut a measurement short,
    /// or another measurement wants to start): a spent budget is then breached.
    pub fn note_cut_short(&self) {
        self.cut_short.store(true, Ordering::Release);
    }

    /// The breach of a spent budget that was exceeded or stopped work; `None` while no budget
    /// is spent, and for a budget spent exactly by work that was done anyway.
    pub fn breach(&self) -> Option<BudgetBreach> {
        if !self.exceeded.load(Ordering::Acquire) && !self.cut_short.load(Ordering::Acquire) {
            return None;
        }
        self.breach
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// A measurement wants to start: the breach that refuses it when a budget is spent.
    pub fn demand(&self) -> Option<BudgetBreach> {
        self.tick();
        if self.tripped() {
            self.note_cut_short();
        }
        self.breach()
    }

    /// Requests and bytes accounted so far.
    pub fn usage(&self) -> (u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
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
    /// Estimated wall time, dataset upload and cleanup included.
    pub eta_s: f64,
    pub budgets: Budgets,
    /// How the estimates were made.
    pub assumptions: Vec<String>,
    /// The target as given (URL or `lab://` scenario), for the run command.
    #[serde(default)]
    pub target_ref: String,
    /// The command that runs this plan (`oprobe run …`, every non-default setting).
    #[serde(default)]
    pub command: String,
    /// Things that would run, but probably not as meant (e.g. warmup ≥ window).
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Build the plan of a run without touching the target.
pub fn plan(profile: &Profile, run_prefix: &str, read_only: bool, budgets: &Budgets) -> Plan {
    let w = &profile.workload;
    let k = &crate::config::Knobs::of(profile);
    let ds = w.effective_dataset();
    let mut mix = w.effective_mix();
    if ds.existing_prefix.is_some() {
        mix.retain(|m| !m.op.is_write());
    }
    let network = profile.target().is_network();
    let total_w: u64 = mix.iter().map(|m| m.weight as u64).sum::<u64>().max(1);
    // Unset (0): OpenDAL reads in one request, and writes multipart parts of ~8 MiB.
    let read_chunk = match k.chunk_size.0 {
        0 => u64::MAX,
        c => c,
    };
    let chunk = match k.chunk_size.0 {
        0 => 8 << 20,
        c => c,
    };
    let obj = ds.object_size.0;
    // Requests and bytes of one operation of each kind.
    let per_op = |op: &OpKind| -> (f64, f64, bool) {
        match op {
            OpKind::Read { size } => {
                let len = size.map(|s| s.0.min(obj)).unwrap_or(obj);
                (len.div_ceil(read_chunk).max(1) as f64, len as f64, false)
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
        // Bytes still take time in process: a copy at ~1 GiB/s per operation.
        let transfer = bytes_per_op / (1024.0 * 1024.0 * 1024.0);
        assumptions.push(
            "in-process operations take ~0.2 ms plus transfer at 1 GiB/s per operation".to_owned(),
        );
        0.0002 + transfer
    };
    let window = w.warmup.get().as_secs_f64() + w.duration.get().as_secs_f64();
    let conc = k.concurrency.max(1) as f64;
    let mut ops = match w.expected_arrivals(std::time::Duration::from_secs_f64(window)) {
        Some(n) => n,
        None => conc * window / op_latency_s,
    };
    // The warm-up and the window, as long as they run: `max_ops` ends the window early
    // (9.3.3's ETA counted the whole window whatever `max_ops` said).
    let mut window = window;
    if let Some(m) = w.max_ops {
        ops = ops.min(m as f64 + conc * w.warmup.get().as_secs_f64() / op_latency_s);
        if w.expected_arrivals(w.duration.get()).is_none() {
            window = window.min(w.warmup.get().as_secs_f64() + m as f64 * op_latency_s / conc);
        }
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
        target: profile.target().display(),
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
        eta_s: prepare_s + window + cleanup_s,
        budgets: budgets.clone(),
        assumptions,
        target_ref: target_ref(profile),
        command: crate::model::RunSpec::new(profile, target_ref(profile)).command(),
        warnings: {
            let mut w = workload_warnings(profile);
            w.extend(budget_warnings(
                budgets,
                requests,
                bytes,
                prepare_s + window + cleanup_s,
            ));
            w
        },
    }
}

/// The budgets a plan would spend before it finishes (the run would stop there, exit 4).
pub(crate) fn budget_warnings(b: &Budgets, requests: u64, bytes: u64, eta_s: f64) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(max) = b.max_requests_count
        && requests >= max
    {
        out.push(format!(
            "~{requests} requests reach the request budget ({max}): the run would stop early (raise --max-requests or shorten the run)"
        ));
    }
    if let Some(max) = b.max_bytes
        && bytes >= max
    {
        out.push(format!(
            "~{} transferred reach the byte budget ({}): the run would stop early (raise --max-bytes or shorten the run)",
            crate::util::fmt_bytes(bytes),
            crate::util::fmt_bytes(max)
        ));
    }
    if let Some(max) = b.max_duration_s
        && eta_s > max
    {
        out.push(format!(
            "an ETA of {eta_s:.0}s exceeds the duration budget ({max:.0}s): the run would stop early (raise --max-duration or shorten the run)"
        ));
    }
    out
}

impl Budgets {
    /// `≤ 500 requests · ≤ 1.0 GiB · ≤ 60s`, or `None` without a budget.
    pub fn line(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(n) = self.max_requests_count {
            parts.push(format!("≤ {n} requests"));
        }
        if let Some(n) = self.max_bytes {
            parts.push(format!("≤ {}", crate::util::fmt_bytes(n)));
        }
        if let Some(s) = self.max_duration_s {
            parts.push(format!("≤ {s:.0}s"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// The target of a profile as typed: its URL (or `lab://` scenario) without secrets, else its
/// display form.
pub fn target_ref(profile: &Profile) -> String {
    profile
        .target()
        .url
        .as_deref()
        .map(crate::util::redact_url)
        .unwrap_or_else(|| profile.target().display())
}

/// Warnings about a workload that would run but probably not as meant (AG-9): a warmup at
/// least as long as the measurement window spends most of the run unmeasured.
pub fn workload_warnings(profile: &Profile) -> Vec<String> {
    warmup_warning(profile).into_iter().collect()
}

/// The warning of a warmup at least as long as the measurement window, with the setting that
/// shortens it (`--set warmup=…`).
pub fn warmup_warning(profile: &Profile) -> Option<String> {
    let w = &profile.workload;
    let (warmup, window) = (w.warmup.get(), w.duration.get());
    (!warmup.is_zero() && warmup >= window && w.max_ops.is_none()).then(|| {
        format!(
            "the warmup ({}) is as long as the measurement window ({}) or longer: most of the \
             run is not measured; shorten it with `--set warmup=DURATION` or lengthen the window \
             with `-d`",
            w.warmup, w.duration
        )
    })
}

/// Outcome of [`preflight`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Preflight {
    /// Listing the target root worked.
    pub list: bool,
    /// A tiny object could be written and deleted under the run prefix.
    pub write_delete: Option<bool>,
}

/// How long the probe object's delete may take once the preflight has failed or was stopped
/// (the target may be unreachable, which is often why).
const PROBE_DELETE_TIMEOUT: Duration = Duration::from_secs(30);

/// Credentials and permissions, before any write: list the root, then (when the run writes)
/// write and delete a 1-byte object under the run prefix. Fails fast with a classified error.
///
/// `admin` is behind the run's gate (the probes count against the budgets and stop when the
/// run is cancelled); `cleaner` is not: the probe object is deleted through it, whatever became
/// of its write. A gate that closes between the write and the delete (the write spent the
/// request budget, or Ctrl-C) would otherwise leave the object, and nothing cleans up after a
/// run that never opened.
pub async fn preflight(
    admin: &opendal::Operator,
    cleaner: &opendal::Operator,
    run_prefix: &str,
    write_probe: bool,
) -> anyhow::Result<Preflight> {
    let classify = |what: &str, e: opendal::Error| -> anyhow::Error {
        // Typed from the OpenDAL error (a credential that cannot be loaded is
        // `auth.no_credentials`).
        // An error that says nothing more than "unexpected" is a failing service or network
        // path when OpenDAL marks it temporary.
        let code = crate::errors::opendal_code(&e).unwrap_or(if e.is_temporary() {
            ErrorCode::NetUnreachable
        } else {
            ErrorCode::Internal
        });
        let hint = if code == ErrorCode::AuthNoCredentials {
            "set credentials for the operator (OpenDAL's credential chain for the service), or send unsigned requests to a public bucket with --set options.skip_signature=true"
        } else {
            "check the credentials and permissions of the operator"
        };
        OprobeError::new(code, format!("preflight: cannot {what}: {e}"))
            .with_hint(hint)
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
        if let Err(e) = admin.write(&key, vec![0u8]).await {
            // A write that failed may still have been stored (its answer was lost or cut).
            let _ = tokio::time::timeout(PROBE_DELETE_TIMEOUT, cleaner.delete(&key)).await;
            return Err(classify("write a test object", e));
        }
        cleaner
            .delete(&key)
            .await
            .map_err(|e| classify("delete a test object", e))?;
        out.write_delete = Some(true);
    }
    Ok(out)
}

#[cfg(test)]
mod sec_tests {
    use super::*;

    #[test]
    fn request_budget_trips_once_and_cancels() {
        let g = Guard::new(Budgets {
            max_requests_count: Some(10),
            ..Default::default()
        });
        let c = Cancel::new();
        g.arm(c.clone());
        g.add(9, 0);
        assert!(!g.tripped());
        // The tenth request spends the budget: the run stops at the limit, not one past it.
        g.add(1, 0);
        assert!(g.tripped());
        assert!(c.is_cancelled());
        // Spent exactly, by work that was done anyway: not a breach (yet).
        assert!(g.breach().is_none());
        // A request past the limit (one in flight when it tripped) is.
        g.add(1, 0);
        let b = g.breach().unwrap();
        assert_eq!(b.budget, "requests");
        g.add(100, 0);
        assert_eq!(g.breach().unwrap().observed, 10.0);
        // Work stopped by a spent budget is a breach even at the limit.
        let g = Guard::new(Budgets {
            max_requests_count: Some(10),
            ..Default::default()
        });
        g.add(10, 0);
        assert!(g.tripped() && g.breach().is_none());
        g.note_cut_short();
        assert_eq!(g.breach().unwrap().observed, 10.0);
        // As is a measurement that wants to start once it is spent.
        let g = Guard::new(Budgets {
            max_requests_count: Some(10),
            ..Default::default()
        });
        g.add(10, 0);
        assert!(g.demand().is_some());
        assert_eq!(b.message(), "request budget spent: 10 requests (limit 10)");
        let e = b.to_error();
        assert_eq!(
            crate::errors::classify(&e, "s3").code,
            ErrorCode::SafetyBudgetExceeded
        );
    }

    /// A body that does not fit in what is left of the byte budget is refused with what
    /// happened: 9.2 said "byte budget spent: 1 B transferred (limit 4.0 MiB)".
    #[test]
    fn a_request_that_does_not_fit_the_byte_budget_says_so() {
        let g = Guard::new(Budgets {
            max_bytes: Some(4 << 20),
            ..Default::default()
        });
        g.admit(1).unwrap();
        let b = g.admit(8 << 20).unwrap_err();
        assert_eq!(
            b.message(),
            "the next request (8.0 MiB) does not fit the byte budget: 1 B of 4.0 MiB used"
        );
        assert_eq!(g.usage(), (1, 1), "the refused request is not counted");
        assert_eq!(g.breach(), Some(b.clone()), "work the budget stopped");
        // The breach reads back, and one written by 9.2 (without the field) still does.
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["refused_request_bytes"], 8 << 20);
        assert_eq!(serde_json::from_value::<BudgetBreach>(json).unwrap(), b);
        let old: BudgetBreach =
            serde_json::from_str(r#"{"budget":"bytes","limit":10.0,"observed":10.0}"#).unwrap();
        assert_eq!(old.refused_request_bytes, None);
        assert_eq!(
            old.message(),
            "byte budget spent: 10 B transferred (limit 10 B)"
        );
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("refused_request_bytes")
                .is_none()
        );

        // Spent exactly: the request that spends it is admitted, and nothing after it is,
        // a request without a body included (9.2 sent that one, and cut its response).
        let g = Guard::new(Budgets {
            max_bytes: Some(100),
            ..Default::default()
        });
        let c = Cancel::new();
        g.arm(c.clone());
        g.admit(0).unwrap();
        g.admit(100).unwrap();
        assert!(g.tripped() && g.breach().is_none() && !c.is_cancelled());
        let b = g.admit(0).unwrap_err();
        assert_eq!(
            b.message(),
            "byte budget spent: 100 B transferred (limit 100 B)"
        );
        assert_eq!(g.usage(), (2, 100));
        assert!(c.is_cancelled());
        assert_eq!(g.breach(), Some(b));
    }

    #[test]
    fn a_shared_guard_aggregates_and_rearms() {
        let g = Guard::new(Budgets {
            max_requests_count: Some(10),
            ..Default::default()
        });
        let first = Cancel::new();
        g.arm(first.clone());
        g.add(6, 0);
        let second = Cancel::new();
        g.arm(second.clone());
        g.add(6, 0);
        assert!(g.tripped(), "12 requests over two measurements exceed 10");
        assert!(second.is_cancelled());
        assert!(!first.is_cancelled());
        let third = Cancel::new();
        g.arm(third.clone());
        assert!(
            third.is_cancelled(),
            "a spent budget stops the next measurement"
        );
    }

    #[test]
    fn read_only_refuses_generated_datasets_but_not_existing_ones() {
        let mut p = Profile::from_url("memory://");
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

    /// The warmup warning names the setting that fixes it (U6).
    #[test]
    fn the_warmup_warning_names_its_setting() {
        let mut p = Profile::from_url("memory://");
        p.workload.warmup = crate::util::HumanDuration(Duration::from_secs(5));
        p.workload.duration = crate::util::HumanDuration(Duration::from_secs(2));
        let w = workload_warnings(&p);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("--set warmup="), "{w:?}");
        assert_eq!(plan(&p, "x/", false, &Budgets::default()).warnings, w);
        p.workload.warmup = crate::util::HumanDuration(Duration::from_secs(1));
        assert!(workload_warnings(&p).is_empty());
    }

    #[test]
    fn plan_counts_dataset_writes_and_cleanup() {
        let mut p = Profile::from_url("memory://");
        p.workload.max_ops = Some(100);
        p.workload.warmup = crate::util::HumanDuration(Duration::ZERO);
        let pl = plan(&p, "oprobe-run-x/", false, &Budgets::default());
        let ds = p.workload.effective_dataset();
        assert!(pl.objects_written_count >= ds.objects as u64);
        assert_eq!(pl.objects_deleted_count, pl.objects_written_count);
        assert!(pl.estimated_ops_count <= 100);
        assert!(pl.estimated_requests_count >= ds.objects as u64);
        assert!(pl.eta_s > 0.0);
        // N5: 100 operations end the window long before its 10 s.
        assert!(pl.eta_s < 1.0, "{}", pl.eta_s);
        assert!(pl.warnings.iter().all(|w| !w.contains("budget")), "{pl:?}");
    }

    /// 7.0.0 planned 30 000 GiB in 12 s for large in-process objects: bytes take time too.
    #[test]
    fn an_in_process_plan_counts_the_transfer_time() {
        let mut p = Profile::from_url("memory://");
        p.workload.preset = crate::config::Preset::SmallFiles;
        p.workload.dataset = Some(crate::config::DatasetSpec {
            objects: 4,
            object_size: crate::util::ByteSize(64 << 20),
            ..Default::default()
        });
        p.workload.mix = vec![crate::config::OpSpec {
            weight: 1,
            op: OpKind::Read { size: None },
        }];
        p.workload.concurrency = 4;
        p.workload.warmup = crate::util::HumanDuration(Duration::ZERO);
        p.workload.duration = crate::util::HumanDuration(Duration::from_secs(10));
        let pl = plan(&p, "oprobe-run-x/", false, &Budgets::default());
        // 4 workers at ~1 GiB/s each for 10 s: tens of GiB, not tens of thousands.
        let gib = pl.estimated_bytes as f64 / (1u64 << 30) as f64;
        assert!(gib < 100.0 && gib > 10.0, "{gib} GiB: {pl:?}");
        assert!(
            pl.assumptions.iter().any(|a| a.contains("transfer")),
            "{:?}",
            pl.assumptions
        );
    }

    /// 6.3.0 planned 6 541 requests under `--max-requests 500` without a word.
    #[test]
    fn a_plan_warns_when_it_would_spend_a_budget() {
        let mut p = Profile::from_url("memory://");
        p.workload.max_ops = Some(100);
        let free = plan(&p, "oprobe-run-x/", false, &Budgets::default());
        let budgets = Budgets {
            max_requests_count: Some(free.estimated_requests_count / 2),
            max_bytes: Some(free.estimated_bytes / 2),
            max_duration_s: Some(free.eta_s / 2.0),
        };
        let pl = plan(&p, "oprobe-run-x/", false, &budgets);
        assert_eq!(pl.budgets, budgets);
        for flag in ["--max-requests", "--max-bytes", "--max-duration"] {
            assert!(
                pl.warnings.iter().any(|w| w.contains(flag)),
                "{flag}: {pl:?}"
            );
        }
        let roomy = Budgets {
            max_requests_count: Some(free.estimated_requests_count * 2),
            max_bytes: Some(free.estimated_bytes * 2 + 1),
            max_duration_s: Some(free.eta_s * 2.0),
        };
        let pl = plan(&p, "oprobe-run-x/", false, &roomy);
        assert!(pl.warnings.iter().all(|w| !w.contains("budget")), "{pl:?}");
    }
}
