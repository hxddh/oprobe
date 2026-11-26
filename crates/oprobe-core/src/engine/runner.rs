//! Benchmark session: prepares the dataset once, runs one or many measurement windows
//! (closed- or open-loop) with different knobs, verifies integrity and cleans up.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use opendal::Operator;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use tokio::sync::Semaphore;
use tokio::task::{JoinError, JoinSet};

use super::cleanup::{CleanupSummary, delete_prefix, new_run_prefix};
use super::dataset::{Dataset, PayloadPool, Picker};
use super::gate::Gate;
use super::operator::{admin_operator, admin_operator_with, base_operator, measured_operator_with};
use super::ops::{OpCtx, error_class, execute, verb};
use super::upload::Aborts;
use crate::config::{Knobs, OpKind, OpSpec, Profile, TargetSpec};
use crate::guard::Guard;
use crate::trace::{OpCompletion, OpScope, OpenLoopStats, Phase, Recorder, Snapshot, TraceRows};

/// Cooperative cancellation shared with signal handlers, the app and the budget guard.
/// Cancelling stops new work, and the [`super::gate`] cuts the requests in flight.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<CancelState>);

#[derive(Debug, Default)]
struct CancelState {
    cancelled: AtomicBool,
    wake: tokio::sync::Notify,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.wake.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    /// Resolves once [`Cancel::cancel`] has been called (at once: every waiter is woken).
    pub async fn cancelled(&self) {
        let woken = self.0.wake.notified();
        tokio::pin!(woken);
        // Registered before the flag is read: a cancellation in between still wakes it.
        woken.as_mut().enable();
        if !self.is_cancelled() {
            woken.await;
        }
    }
}

/// Turn a worker task failure into an error instead of losing it (a panicking worker would
/// otherwise silently shrink the load and skew the results).
fn worker_result(r: std::result::Result<(), JoinError>) -> Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.is_panic() => {
            let p = e.into_panic();
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            anyhow::bail!("a benchmark worker panicked: {msg}")
        }
        Err(e) => Err(anyhow::anyhow!("a benchmark worker failed: {e}")),
    }
}

async fn join_workers(mut set: JoinSet<()>) -> Result<()> {
    let mut first_err = None;
    while let Some(r) = set.join_next().await {
        if let Err(e) = worker_result(r)
            && first_err.is_none()
        {
            first_err = Some(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// Write slots of in-flight open-loop operations: a slot (scratch key) is held by exactly one
/// operation at a time, so concurrent writes never race on the same key (which would make the
/// sampled integrity check compare against the wrong payload).
struct SlotPool(Mutex<std::collections::VecDeque<u64>>);

impl SlotPool {
    fn new(n: u64) -> Arc<Self> {
        Arc::new(Self(Mutex::new((0..n.max(1)).collect())))
    }
    fn take(self: &Arc<Self>) -> SlotLease {
        let slot = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .expect("slot pool holds more slots than permits");
        SlotLease(self.clone(), slot)
    }
}

/// Returns its slot to the pool when dropped (also on panic / cancellation).
struct SlotLease(Arc<SlotPool>, u64);

impl Drop for SlotLease {
    fn drop(&mut self) {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(self.1);
    }
}

/// Options for one measurement window.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub duration: Duration,
    pub warmup: Duration,
    pub max_ops: Option<u64>,
    pub rate: Option<f64>,
    /// Capture await-tree call stacks of slow operations.
    pub trace_slow_ops: bool,
}

impl RunOptions {
    pub fn from_profile(p: &Profile) -> Self {
        Self {
            duration: p.workload.duration.get(),
            warmup: p.workload.warmup.get(),
            max_ops: p.workload.max_ops,
            rate: p.workload.rate,
            trace_slow_ops: true,
        }
    }
}

/// Live progress events for CLIs/TUIs.
#[derive(Clone, Debug)]
pub enum Progress {
    Message(String),
    Snapshot(Box<Snapshot>),
}

pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

/// How a [`Session`] is opened (safety rails and trace settings).
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Object prefix the run writes under (default: `oprobe-run-<new run id>/`).
    pub run_prefix: Option<String>,
    /// Budgets enforced live (requests, bytes, cost, duration).
    pub guard: Option<Arc<Guard>>,
    /// Trace rows kept per measurement window (`trace.max_requests`).
    pub trace_max_requests: usize,
    /// List (and, when the run writes, write + delete a tiny object) before anything else.
    pub preflight: bool,
    /// Warmed [`opendal::layers::TailCutLayer`] reused across candidate trials of one prove.
    pub tail_cut: Option<opendal::layers::TailCutLayer>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            run_prefix: None,
            guard: None,
            trace_max_requests: crate::trace::DEFAULT_MAX_REQUESTS,
            preflight: true,
            tail_cut: None,
        }
    }
}

pub struct Session {
    pub profile: Profile,
    /// The operator's service target, normalized (the region of an S3 bucket detected).
    pub target: TargetSpec,
    pub base: Operator,
    /// Dataset preparation and verification: behind the run's gate (budgets, cancellation).
    pub admin: Operator,
    /// Cleanup (deleting test data): outside the gate, so nothing stops it.
    pub cleaner: Operator,
    pub run_prefix: String,
    pub dataset: Arc<Dataset>,
    pub mix: Vec<OpSpec>,
    pub warnings: Vec<String>,
    pool: Arc<PayloadPool>,
    written: Arc<Mutex<Vec<(String, u32)>>>,
    pub cancel: Cancel,
    /// Budgets and cancellation, enforced on every request before cleanup.
    gate: Gate,
    /// The upload tasks of this session (the multipart uploads it has to abort).
    aborts: Aborts,
    /// The service's full capability (limits OpenDAL applies, e.g. the minimum part size).
    pub capability: opendal::Capability,
    trial: AtomicU64,
    guard: Option<Arc<Guard>>,
    trace_max_requests: usize,
    last_trace: Mutex<Option<TraceRows>>,
    /// Where the next window streams its rows (the run file), taken by [`Session::run`].
    row_sink: Mutex<Option<Arc<dyn crate::trace::RowSink>>>,
    tail_cut: Option<opendal::layers::TailCutLayer>,
}

impl Session {
    /// Connect to the target and prepare the dataset.
    pub async fn open(profile: Profile, cancel: Cancel, progress: ProgressFn) -> Result<Self> {
        Self::open_with(profile, cancel, progress, SessionOptions::default()).await
    }

    /// [`Session::open`] with safety rails: preflight before any write, budgets armed on
    /// `cancel`, a chosen run prefix.
    pub async fn open_with(
        profile: Profile,
        cancel: Cancel,
        progress: ProgressFn,
        opts: SessionOptions,
    ) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(crate::errors::OprobeError::new(
                crate::errors::ErrorCode::RunInterrupted,
                "run interrupted",
            )
            .into());
        }
        profile.validate()?;
        let mut target = profile.operator.target()?;
        super::operator::prepare_target(&mut target).await?;
        let base = base_operator(&target)?;
        let run_prefix = opts.run_prefix.clone().unwrap_or_else(new_run_prefix);
        if let Some(g) = &opts.guard {
            g.arm(cancel.clone());
        }
        let gate = Gate::new(opts.guard.clone(), cancel.clone());
        let admin = admin_operator_with(&base, &target, Some(&gate))?;
        let cleaner = admin_operator(&base, &target)?;
        let aborts = Aborts::new();
        let spec = profile.workload.effective_dataset();
        let mut mix = profile.workload.effective_mix();
        // The mix runs on what the service can do.
        let measured_cap = base.info().capability();
        let mut warnings = super::workloads::adapt_mix(&mut mix, &measured_cap, &target.scheme);
        if spec.existing_prefix.is_some() && mix.iter().any(|m| m.op.is_write()) {
            mix.retain(|m| !m.op.is_write());
            warnings.push("existing_prefix datasets are read-only: write operations were removed from the mix".into());
        }
        anyhow::ensure!(
            mix.iter().any(|m| m.weight > 0),
            "{}",
            if spec.existing_prefix.is_some() {
                "the dataset is read-only (existing prefix), but every read operation in the mix has weight 0: give a read/stat/list operation a weight > 0"
            } else {
                "the workload mix needs at least one operation with weight > 0"
            }
        );
        if target.is_network() {
            let http = &profile.operator.http;
            super::operator::check_reachable_with(&base, &target, http, Some(&gate)).await?;
            super::operator::check_protocol(&base, &target, http, Some(&gate)).await?;
        }
        let needs_objects = mix.iter().any(|m| m.op.needs_dataset() && m.weight > 0);
        if opts.preflight {
            let writes = spec.existing_prefix.is_none()
                && (needs_objects || mix.iter().any(|m| m.weight > 0 && m.op.is_write()));
            // Fail fast: few retries, short timeouts (the admin operator retries for minutes).
            let quick = super::operator::preflight_operator(&base, &target, Some(&gate))?;
            crate::guard::preflight(&quick, &run_prefix, writes)
                .await
                .map_err(|e| super::operator::explain_preflight(e, &target))?;
        }
        let p2 = progress.clone();
        let dataset = Dataset::prepare(
            super::dataset::Via {
                admin: &admin,
                cleaner: &cleaner,
                aborts: &aborts,
            },
            &spec,
            profile.workload.seed,
            &run_prefix,
            needs_objects,
            &cancel,
            &move |m| p2(Progress::Message(m)),
        )
        .await?;
        if let Some(OpKind::Read { size: Some(sz) }) = mix
            .iter()
            .map(|m| &m.op)
            .find(|o| matches!(o, OpKind::Read { size: Some(_) }))
            && let Some(min) = dataset.objects.iter().map(|o| o.size).min()
            && sz.0 > min
        {
            warnings.push(format!("range size {sz} exceeds the smallest object ({min} B); those reads become whole-object reads"));
        }
        let max_write = super::ops::max_payload(mix.iter().map(|m| &m.op));
        let capability = measured_cap;
        Ok(Self {
            profile,
            target,
            base,
            admin,
            cleaner,
            run_prefix,
            dataset: Arc::new(dataset),
            mix,
            warnings,
            pool: Arc::new(PayloadPool::new(max_write)),
            written: Arc::new(Mutex::new(Vec::new())),
            cancel,
            gate,
            aborts,
            capability,
            trial: AtomicU64::new(0),
            guard: opts.guard,
            trace_max_requests: opts.trace_max_requests,
            last_trace: Mutex::new(None),
            row_sink: Mutex::new(None),
            tail_cut: opts.tail_cut,
        })
    }

    /// Trace rows of the last [`Session::run`] (taken: a second call returns `None`).
    pub fn take_trace(&self) -> Option<TraceRows> {
        self.last_trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Stream every row of the next [`Session::run`] window to `sink` (the run file, written
    /// while the run is in progress); the in-memory sample then keeps at most
    /// [`crate::store::runfile::STREAM_MEMORY_ROWS`] rows.
    pub fn stream_rows(&self, sink: Arc<dyn crate::trace::RowSink>) {
        *self.row_sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    /// The budget guard, when the session has one.
    pub fn guard(&self) -> Option<&Arc<Guard>> {
        self.guard.as_ref()
    }

    /// Run warmup + one measurement window with `knobs` and return the final snapshot.
    pub async fn run(
        &self,
        knobs: &Knobs,
        opts: &RunOptions,
        progress: Option<ProgressFn>,
    ) -> Result<Snapshot> {
        let trial = self.trial.fetch_add(1, Ordering::Relaxed);
        let recorder = Recorder::new();
        recorder.set_trace_limit(self.trace_max_requests);
        if let Some(sink) = self
            .row_sink
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            recorder.set_row_sink(sink);
            recorder.set_trace_limit(
                self.trace_max_requests
                    .min(crate::store::runfile::STREAM_MEMORY_ROWS),
            );
        }
        // HTTP requests are accounted at the gate, as they are sent. An in-process service has
        // none: its calls are accounted by the recorder, as they complete.
        if let Some(g) = &self.guard
            && !self.target.is_network()
        {
            recorder.set_guard(g.clone());
        }
        let tree = opts
            .trace_slow_ops
            .then(|| await_tree::Registry::new(await_tree::Config::default()));
        if let Some(t) = &tree {
            recorder.set_tree_registry(t.clone());
        }
        let mut spec = self.profile.operator.clone();
        knobs.apply_to(&mut spec);
        let op = measured_operator_with(
            &self.base,
            &self.target,
            &spec,
            &recorder,
            tree.clone(),
            self.tail_cut.clone(),
            Some(&self.gate),
        )
        .await?;
        let ctx = Arc::new(OpCtx {
            op,
            picker: Picker::new(self.dataset.objects.len(), self.profile.workload.access),
            dataset: self.dataset.clone(),
            spec,
            pool: self.pool.clone(),
            verify: self.profile.workload.verify,
            recorder: recorder.clone(),
            scratch_prefix: format!("{}writes/t{trial:03}-", self.run_prefix),
            written: self.written.clone(),
            state: super::workloads::WorkloadState::new(self.capability, &self.mix),
            aborts: self.aborts.clone(),
            cancel: self.cancel.clone(),
        });

        // Background sampler: slow-op call stacks, client CPU/RSS, live snapshots.
        let stop = Arc::new(AtomicBool::new(false));
        let sampler = {
            let stop = stop.clone();
            let rec = recorder.clone();
            let progress = progress.clone();
            let guard = self.guard.clone();
            tokio::spawn(async move {
                let mut cpu = super::procstat::CpuMeter::new();
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                let mut n = 0u64;
                while !stop.load(Ordering::Acquire) {
                    tick.tick().await;
                    n += 1;
                    if let Some(g) = &guard {
                        g.tick();
                    }
                    rec.sample_trees(rec.current_p90_us().max(2_000));
                    rec.sample_series();
                    if n.is_multiple_of(50)
                        && let Some((pct, rss)) = cpu.sample()
                    {
                        rec.update_client(pct, rss);
                    }
                    if n.is_multiple_of(12)
                        && let Some(cb) = &progress
                    {
                        cb(Progress::Snapshot(Box::new(rec.snapshot())));
                    }
                }
            })
        };

        let result = async {
            if !opts.warmup.is_zero() && !self.cancel.is_cancelled() {
                recorder.set_phase(Phase::Warmup);
                self.drive(
                    &ctx,
                    knobs,
                    opts,
                    opts.warmup,
                    None,
                    false,
                    trial,
                    &recorder,
                    tree.as_ref(),
                )
                .await?;
            }
            recorder.set_phase(Phase::Measure);
            let probe = super::selfobs::SelfProbe::start();
            let measured = self
                .drive(
                    &ctx,
                    knobs,
                    opts,
                    opts.duration,
                    opts.max_ops,
                    true,
                    trial,
                    &recorder,
                    tree.as_ref(),
                )
                .await;
            let obs = probe.finish(recorder.observer_per_op_us()).await;
            measured?;
            recorder.set_phase(Phase::Verify);
            Ok::<_, anyhow::Error>(obs)
        }
        .await;
        stop.store(true, Ordering::Release);
        let _ = sampler.await;
        // Operations that were dropped unfinished abort their uploads on their own tasks:
        // nothing of this window is in flight once they are done.
        self.aborts.settle().await;
        let obs = result?;
        let mut snap = recorder.snapshot();
        snap.self_obs = Some(obs);
        snap.phase = Phase::Done;
        snap.interrupted = self.cancel.is_cancelled();
        snap.effective_knobs = super::stack::effective_knobs(&self.capability, knobs);
        *self.last_trace.lock().unwrap_or_else(|e| e.into_inner()) = Some(recorder.take_trace());
        if let Some(cb) = &progress {
            cb(Progress::Snapshot(Box::new(snap.clone())));
        }
        Ok(snap)
    }

    /// Run one window. When `measured`, the recorder's window is closed at the deadline (or
    /// when the run stops early) so rates cover exactly the window, while operations that
    /// started inside it drain and are still recorded.
    #[allow(clippy::too_many_arguments)]
    async fn drive(
        &self,
        ctx: &Arc<OpCtx>,
        knobs: &Knobs,
        opts: &RunOptions,
        window: Duration,
        max_ops: Option<u64>,
        measured: bool,
        trial: u64,
        recorder: &Arc<Recorder>,
        tree: Option<&await_tree::Registry>,
    ) -> Result<()> {
        let deadline = Instant::now() + window;
        let started = Arc::new(AtomicU64::new(0));
        let cum: Vec<(u32, OpKind)> = {
            let mut acc = 0;
            self.mix
                .iter()
                .filter(|m| m.weight > 0)
                .map(|m| {
                    acc += m.weight;
                    (acc, m.op.clone())
                })
                .collect()
        };
        anyhow::ensure!(
            !cum.is_empty(),
            "the workload mix has no operation with weight > 0"
        );
        let total_w = cum.last().map(|c| c.0).unwrap_or(1);
        let cum = Arc::new(cum);
        let pick_kind = move |rng: &mut SmallRng, cum: &Vec<(u32, OpKind)>| -> OpKind {
            let x = rng.gen_range(0..total_w);
            cum.iter()
                .find(|(c, _)| x < *c)
                .map(|(_, k)| k.clone())
                .unwrap_or_else(|| cum[0].1.clone())
        };
        let should_stop = {
            let started = started.clone();
            let cancel = self.cancel.clone();
            move || {
                cancel.is_cancelled()
                    || Instant::now() >= deadline
                    || max_ops
                        .map(|m| started.load(Ordering::Relaxed) >= m)
                        .unwrap_or(false)
            }
        };
        let close_window = {
            let rec = recorder.clone();
            move || {
                if measured {
                    rec.close_window(Instant::now().min(deadline));
                }
            }
        };
        // The cancellation stopped work the window still had to do (before its deadline and
        // its last operation): a budget spent then is breached, not just spent.
        let cut_short = {
            let started = started.clone();
            let cancel = self.cancel.clone();
            let guard = self.guard.clone();
            move || {
                if cancel.is_cancelled()
                    && Instant::now() < deadline
                    && !max_ops.is_some_and(|m| started.load(Ordering::Relaxed) >= m)
                    && let Some(g) = &guard
                {
                    g.note_cut_short();
                }
            }
        };
        let seed = self.profile.workload.seed ^ (trial << 32) ^ if measured { 0x5eed } else { 0 };

        let rate = opts.rate.or_else(|| {
            self.profile
                .workload
                .shape
                .is_some()
                .then(|| self.profile.workload.rate.unwrap_or(0.0))
        });
        match rate {
            None => {
                let mut set = JoinSet::new();
                for w in 0..knobs.concurrency.max(1) {
                    let ctx = ctx.clone();
                    let cum = cum.clone();
                    let stop = should_stop.clone();
                    let close = close_window.clone();
                    let cut = cut_short.clone();
                    let started = started.clone();
                    let rec = recorder.clone();
                    let tree = tree.cloned();
                    set.spawn(async move {
                        let mut rng =
                            SmallRng::seed_from_u64(seed ^ (w as u64).wrapping_mul(0x9E37_79B9));
                        let mut seq = 0u64;
                        while !stop() {
                            // Claim a ticket first so `max_ops` is exact under concurrency.
                            let n = started.fetch_add(1, Ordering::Relaxed);
                            if max_ops.is_some_and(|m| n >= m) {
                                break;
                            }
                            // Window membership is decided here, when the operation is issued:
                            // a ticket under `max_ops` belongs to the window by definition (the
                            // first worker to run out of tickets closes it while others are
                            // still starting theirs).
                            let in_window = measured && (max_ops.is_some() || rec.window_open());
                            let kind = pick_kind(&mut rng, &cum);
                            let slot = (w as u64) * 4 + seq % 4;
                            seq += 1;
                            run_one(
                                &ctx,
                                &Job::Mix(kind),
                                &mut rng,
                                slot,
                                Instant::now(),
                                false,
                                in_window,
                                &rec,
                                tree.as_ref(),
                            )
                            .await;
                            // In-process services (memory://) complete without yielding: let
                            // the runtime run the sampler and cancellation between operations.
                            tokio::task::yield_now().await;
                        }
                        cut();
                        // The first worker to stop ends the window; the rest drain.
                        close();
                    });
                }
                join_workers(set).await?;
                close_window();
            }
            Some(rate) => {
                let conc = knobs.concurrency.max(1);
                let cap = Arc::new(Semaphore::new(conc));
                let slots = SlotPool::new(conc as u64 * 4);
                let wl = &self.profile.workload;
                let t0 = Instant::now();
                let horizon = deadline.saturating_duration_since(t0);
                let mut src = Source {
                    clock: super::schedule::ArrivalClock::new(
                        wl.arrivals,
                        wl.shape.clone(),
                        rate,
                        horizon,
                    ),
                    rng: SmallRng::seed_from_u64(seed ^ 0xA771_7A15),
                };
                // Bounded backlog: an arrival more than `timeout` behind schedule could only
                // time out; such arrivals are dropped (missed, censored) instead of queued.
                let max_queue_delay = knobs.timeout.get().max(Duration::from_secs(1));
                // A constant pace never dispatches faster than itself (GCRA): arrivals that fell
                // behind — the timer woke late, coalesced on macOS, or the process stalled — go
                // out at most `CATCH_UP_BURST` at once and then at the nominal spacing, keeping
                // their intended start (the lag shows in latency; what the window cannot fit
                // is missed). Released all at once, the backlog of a 100 ms stall at 250/s was a
                // burst of 25 requests: more than a service's token bucket allows, so `--rate`
                // advice throttled although the pace was well below the limit. Poisson and
                // shaped schedules keep their own bursts.
                let mut pacer = (wl.arrivals == super::schedule::Arrivals::Constant
                    && wl.shape.is_none()
                    && rate.is_finite()
                    && rate > 0.0)
                    .then(|| Pacer::new(Duration::from_secs_f64(1.0 / rate)));
                let mut rng = SmallRng::seed_from_u64(seed);
                let mut missed = 0u64;
                let mut issued = 0u64;
                let mut max_lag = Duration::ZERO;
                let mut set = JoinSet::new();
                let missed_arrival = |at: Instant, now: Instant| {
                    if measured {
                        recorder.record_missed_arrival(
                            at,
                            now.saturating_duration_since(at).as_micros() as u64,
                        );
                    }
                };
                let mut pending = src.next(horizon).map(|d| t0 + d);
                // `pending` holds the next arrival until it is dispatched, so the arrivals still
                // due when the loop stops are counted as missed below.
                'arrivals: while let Some(mut next) = pending {
                    if next >= deadline
                        || self.cancel.is_cancelled()
                        || max_ops.is_some_and(|m| issued >= m)
                    {
                        break;
                    }
                    // Never sleep past the window or through a cancellation: wake at the next
                    // arrival, the deadline, or every 100 ms to check for Ctrl-C / Stop.
                    loop {
                        let now = Instant::now();
                        if now >= next {
                            break;
                        }
                        let wake = next.min(deadline).min(now + Duration::from_millis(100));
                        tokio::time::sleep_until(wake.into()).await;
                        // (An arrival due before the deadline still starts when the timer wakes
                        // a little after it.)
                        if self.cancel.is_cancelled() {
                            break 'arrivals;
                        }
                    }
                    // Wait for a free slot (wrk2-style): the arrival stays queued and its latency
                    // keeps counting from the intended start, so saturation shows in latency.
                    let permit = loop {
                        if let Ok(p) = cap.clone().try_acquire_owned() {
                            break p;
                        }
                        let now = Instant::now();
                        if now >= deadline || self.cancel.is_cancelled() {
                            break 'arrivals;
                        }
                        let wake = deadline.min(now + Duration::from_millis(100));
                        tokio::select! {
                            p = cap.clone().acquire_owned() => {
                                break p.context("open-loop slot semaphore closed")?;
                            }
                            _ = tokio::time::sleep_until(wake.into()) => {}
                        }
                    };
                    while let Some(r) = set.try_join_next() {
                        worker_result(r)?;
                    }
                    if let Some(pacer) = pacer.as_mut() {
                        if let Some(allowed) = pacer.earliest() {
                            loop {
                                let now = Instant::now();
                                if now >= allowed {
                                    break;
                                }
                                if now >= deadline || self.cancel.is_cancelled() {
                                    drop(permit);
                                    break 'arrivals;
                                }
                                let wake =
                                    allowed.min(deadline).min(now + Duration::from_millis(100));
                                tokio::time::sleep_until(wake.into()).await;
                            }
                        }
                        pacer.dispatched(Instant::now());
                    }
                    // Arrivals too far behind are dropped: each is missed, with its wait so far
                    // as the lower bound of its latency.
                    let now = Instant::now();
                    while now.saturating_duration_since(next) > max_queue_delay {
                        missed += 1;
                        missed_arrival(next, now);
                        match src.next(horizon) {
                            Some(d) if t0 + d < deadline => next = t0 + d,
                            Some(d) => {
                                pending = Some(t0 + d);
                                break 'arrivals;
                            }
                            None => {
                                pending = None;
                                break 'arrivals;
                            }
                        }
                    }
                    max_lag = max_lag.max(next.elapsed());
                    started.fetch_add(1, Ordering::Relaxed);
                    issued += 1;
                    let job = Job::Mix(pick_kind(&mut rng, &cum));
                    let mut op_rng = SmallRng::seed_from_u64(rng.r#gen());
                    let lease = slots.take();
                    let ctx = ctx.clone();
                    let rec = recorder.clone();
                    let tree = tree.cloned();
                    let intended = next;
                    // Decided at dispatch: the task may start after the window closed.
                    let in_window = measured && rec.window_open();
                    set.spawn(async move {
                        run_one(
                            &ctx,
                            &job,
                            &mut op_rng,
                            lease.1,
                            intended,
                            true,
                            in_window,
                            &rec,
                            tree.as_ref(),
                        )
                        .await;
                        drop(lease);
                        drop(permit);
                    });
                    pending = src.next(horizon).map(|d| t0 + d);
                }
                if pending.is_some() {
                    cut_short();
                }
                let window_end = Instant::now().min(deadline);
                close_window();
                // Arrivals that were due inside the window but never started: censored at
                // (window end − intended start).
                if !max_ops.is_some_and(|m| issued >= m) {
                    while let Some(at) = pending.filter(|t| *t < window_end) {
                        missed += 1;
                        missed_arrival(at, window_end);
                        pending = src.next(horizon).map(|d| t0 + d);
                    }
                }
                join_workers(set).await?;
                if measured {
                    recorder.set_open_loop(OpenLoopStats {
                        target_rate: src.mean_rate(horizon, issued + missed),
                        achieved_rate: issued as f64
                            / window_end
                                .saturating_duration_since(t0)
                                .as_secs_f64()
                                .max(1e-3),
                        missed,
                        max_schedule_lag_ms: max_lag.as_secs_f64() * 1000.0,
                        arrivals: src.label(),
                        intended_count: issued + missed,
                        ..Default::default()
                    });
                }
            }
        }
        Ok(())
    }

    /// Re-read sampled written objects and compare CRC32C (via the untraced admin operator).
    pub async fn verify_writes(&self, recorder: Option<&Recorder>) -> (u64, u64) {
        let items: Vec<(String, u32)> = self.written.lock().map(|w| w.clone()).unwrap_or_default();
        let (mut ok, mut bad) = (0, 0);
        for (path, crc) in items {
            if let Ok(buf) = self.admin.read(&path).await {
                let got = buf
                    .clone()
                    .fold(0u32, |a, c| crate::util::crc32c_append(a, &c));
                if got == crc {
                    ok += 1;
                } else {
                    bad += 1;
                }
                if let Some(r) = recorder {
                    r.record_integrity(got == crc);
                }
            }
        }
        (ok, bad)
    }

    /// Delete everything this session wrote (unless disabled) and verify. Multipart uploads
    /// still being aborted are awaited first, whatever the cleanup setting: an upload in
    /// progress is never test data to keep.
    pub async fn close(&self) -> CleanupSummary {
        self.aborts.settle().await;
        let mut s = self.delete_test_data().await;
        if let Some(e) = self.aborts.failure() {
            s.error = Some(match s.error.take() {
                Some(other) => format!("{other}; {e}"),
                None => e,
            });
        }
        s
    }

    async fn delete_test_data(&self) -> CleanupSummary {
        if !self.profile.workload.cleanup {
            return CleanupSummary {
                enabled: false,
                prefix: self.run_prefix.clone(),
                kept_dataset: Some(self.dataset.prefix.clone()),
                ..Default::default()
            };
        }
        if self.dataset.read_only {
            // Read-only benchmark of existing data: nothing was written.
            return CleanupSummary {
                enabled: true,
                prefix: self.run_prefix.clone(),
                verified: true,
                kept_dataset: Some(self.dataset.prefix.clone()),
                ..Default::default()
            };
        }
        let mut s = delete_prefix(&self.cleaner, &self.run_prefix).await;
        if self.dataset.preexisting {
            s.kept_dataset = Some(self.dataset.prefix.clone());
        }
        s
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_one(
    ctx: &Arc<OpCtx>,
    job: &Job,
    rng: &mut SmallRng,
    slot: u64,
    intended: Instant,
    open_loop: bool,
    measured: bool,
    rec: &Arc<Recorder>,
    tree: Option<&await_tree::Registry>,
) {
    // The operation starts now: the timeline, the trace row and the request offsets count from
    // here (W10: 9.0 stamped an open-loop operation at its intended start, so a backlog piled
    // every operation into the first seconds). Its latency still counts from the intended
    // start (no coordinated omission), and the `schedule` stage — how late it started — is
    // queue time.
    let started = Instant::now();
    let schedule_us =
        open_loop.then(|| started.saturating_duration_since(intended).as_micros() as u64);
    // `measured`: the caller decided, when it issued the operation, that it belongs to the
    // measurement window.
    let scope = OpScope::new_at(measured && rec.is_measuring(), started, Some(rec.clone()));
    let measured = scope.measured;
    let name = match job {
        Job::Mix(kind) => verb(kind),
    };
    rec.op_started_at(scope.id, started);
    let mut key = String::new();
    let fut = scope.run(async {
        match job {
            Job::Mix(kind) => execute(ctx, kind, rng, slot, &mut key).await,
        }
    });
    let res = match tree {
        Some(reg) => {
            scope.set_tree_registry(reg.clone());
            reg.register(scope.id, name).instrument(fut).await
        }
        None => fut.await,
    };
    let latency_us = intended.elapsed().as_micros() as u64;
    // An operation that ended in an error after the run was cancelled was stopped (the gate
    // refused or cut its requests): it did not fail, and it did not complete. It is not an
    // operation of the window.
    let measured = measured && !(res.is_err() && ctx.cancel.is_cancelled());
    let (lr, lw, error) = match res {
        Ok(o) => (o.logical_read, o.logical_write, None),
        Err(e) => {
            let status = scope.with(|d| d.requests.last().map(|r| r.status));
            (0, 0, Some(error_class(&e, status)))
        }
    };
    let data = scope.take();
    let name = if name == "write"
        && data.requests.iter().any(|r| {
            matches!(
                r.api,
                crate::trace::Api::UploadPart
                    | crate::trace::Api::CreateMultipart
                    | crate::trace::Api::CompleteMultipart
            )
        }) {
        "write.multipart"
    } else {
        name
    };
    rec.complete_op(
        measured,
        OpCompletion {
            verb: name,
            op_id: scope.id,
            started,
            latency_us,
            logical_read: lr,
            logical_write: lw,
            error,
            key,
            data,
            schedule_us,
        },
    );
}

/// What one arrival runs: an operation of the mix.
enum Job {
    Mix(OpKind),
}

/// Where open-loop arrivals come from: a rate (constant, Poisson, shaped); the operation is
/// picked from the mix at dispatch.
struct Source {
    clock: super::schedule::ArrivalClock,
    rng: SmallRng,
}

impl Source {
    /// The next arrival (offset from the window start) before `horizon`.
    fn next(&mut self, horizon: Duration) -> Option<Duration> {
        self.clock.next(&mut self.rng, horizon)
    }

    fn label(&self) -> String {
        self.clock.label()
    }

    /// The mean intended rate over the window (the target rate of a shaped schedule).
    fn mean_rate(&self, window: Duration, _intended: u64) -> f64 {
        let steps = 200u32;
        if window.is_zero() {
            return self.clock.rate_at(Duration::ZERO);
        }
        let dt = window / steps;
        (0..steps)
            .map(|i| self.clock.rate_at(dt * i + dt / 2))
            .sum::<f64>()
            / f64::from(steps)
    }
}

/// Arrivals that fell behind a constant pace go out at most this many at once.
const CATCH_UP_BURST: u32 = 4;

/// The dispatch shaper of a constant pace (GCRA): the next dispatch may happen at the
/// theoretical arrival time less `CATCH_UP_BURST − 1` gaps. On-time arrivals (one per gap) are
/// never held; a backlog left by a late timer or a stall leaves in a burst of at most
/// `CATCH_UP_BURST`, then at the nominal spacing.
#[derive(Clone, Copy, Debug)]
struct Pacer {
    gap: Duration,
    tat: Option<Instant>,
}

impl Pacer {
    fn new(gap: Duration) -> Self {
        Self { gap, tat: None }
    }

    /// The earliest time the next dispatch may happen (`None`: now).
    fn earliest(&self) -> Option<Instant> {
        let tolerance = self.gap * (CATCH_UP_BURST - 1);
        self.tat.map(|t| t.checked_sub(tolerance).unwrap_or(t))
    }

    fn dispatched(&mut self, now: Instant) {
        self.tat = Some(self.tat.map_or(now, |t| t.max(now)) + self.gap);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paced_backlog_leaves_in_a_small_burst_then_at_the_pace() {
        let gap = Duration::from_millis(4);
        let mut p = Pacer::new(gap);
        let t = Instant::now();
        // On time: one arrival per gap is never held.
        for k in 0..10u32 {
            let now = t + gap * k;
            assert!(p.earliest().is_none_or(|e| e <= now), "{k}");
            p.dispatched(now);
        }
        // A 100 ms stall leaves 25 arrivals due at once: 4 go now, then one per gap.
        let woke = t + gap * 10 + Duration::from_millis(100);
        let mut at = Vec::new();
        for _ in 0..25 {
            let when = p.earliest().map_or(woke, |e| e.max(woke));
            p.dispatched(when);
            at.push(when);
        }
        assert!(at[..4].iter().all(|w| *w == woke), "{at:?}");
        for w in at[4..].windows(2) {
            assert_eq!(w[1] - w[0], gap);
        }
    }
    use crate::config::Arrivals;

    fn quiet() -> ProgressFn {
        Arc::new(|_| {})
    }

    fn profile(url: &str) -> Profile {
        let mut p = Profile::from_url(url);
        p.workload.preset = crate::config::Preset::ListStat;
        p.workload.dataset = Some(crate::config::DatasetSpec {
            objects: 4,
            object_size: crate::util::ByteSize::kib(4),
            ..Default::default()
        });
        p.workload.verify = false;
        p.workload.concurrency = 1;
        p
    }

    fn opts(ms: u64, rate: Option<f64>) -> RunOptions {
        RunOptions {
            duration: Duration::from_millis(ms),
            warmup: Duration::from_millis(if rate.is_none() { 100 } else { 0 }),
            max_ops: None,
            rate,
            trace_slow_ops: false,
        }
    }

    #[tokio::test]
    async fn closed_loop_runs_say_so() {
        let p = profile("memory://");
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = Knobs::of(&s.profile);
        let snap = s.run(&knobs, &opts(150, None), None).await.unwrap();
        assert!(snap.summary.ops > 0);
        // Every operation is in the dimensions (aggregates, not sampled rows).
        let dims = crate::model::Dimensions::from_stats(snap.dimensions.as_ref());
        let ops: u64 = dims.prefixes.iter().map(|p| p.ops_count).sum();
        assert_eq!(ops, snap.summary.ops);
        assert!(!dims.hot_keys.is_empty());
        assert!(snap.open_loop.is_none());
        let sched = crate::model::ScheduleStats::from_open_loop(snap.open_loop.as_ref(), 0.15);
        assert!(!sched.open_loop);
    }

    #[tokio::test]
    async fn an_overloaded_open_loop_counts_censored_missed_arrivals() {
        // One slot against a 20 ms server at 500 arrivals/s: most arrivals never start.
        let lab = crate::lab::LabServer::spawn(
            "127.0.0.1:0",
            crate::lab::LabConfig {
                base_ttfb_delay: Duration::from_millis(20),
                throttle_status_code: 503,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut p = lab.profile("b");
        p.workload = profile("memory://").workload;
        p.workload.concurrency = 1;
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = Knobs::of(&s.profile);
        let snap = s.run(&knobs, &opts(600, Some(500.0)), None).await.unwrap();
        let ol = snap.open_loop.clone().expect("open-loop stats");
        assert_eq!(ol.arrivals, "constant");
        assert!(ol.missed > 100, "{ol:?}");
        assert_eq!(ol.censored_count, ol.missed);
        assert_eq!(ol.intended_count, snap.summary.ops + ol.missed, "{ol:?}");
        // The intended count matches the schedule (500/s × 0.6 s), not what the client managed.
        assert!((ol.intended_count as i64 - 300).abs() <= 3, "{ol:?}");
        let stage = ol.schedule.expect("schedule stage");
        assert_eq!(stage.count, snap.summary.ops);
        assert!(stage.max > 50.0, "{stage:?}");
        // Censored values are part of the latency distribution: the tail shows the backlog.
        assert_eq!(snap.summary.latency.count, snap.summary.ops + ol.missed);
        assert!(
            snap.summary.latency.p99 > 300.0,
            "{:?}",
            snap.summary.latency
        );
        let missed_series: u64 = snap.timeline.iter().map(|t| t.missed_arrivals).sum();
        assert_eq!(missed_series, ol.missed);
        let sched = crate::model::ScheduleStats::from_open_loop(Some(&ol), snap.elapsed_s);
        assert!(sched.open_loop && sched.backlogged());
        assert!(sched.missed_arrivals_ratio > 0.3);
        // The slot wait is queueing, not client work.
        let v = &snap.verbs[0];
        assert!(
            v.breakdown.queue_ms > v.breakdown.client_ms,
            "{:?}",
            v.breakdown
        );
        // Most arrivals never ran, and their latency is only known to be at least their wait:
        // the run fails its checks with no limit set (9.1 looked at the completed operations
        // alone and passed).
        let sla = crate::config::Sla {
            max_error_rate: 1.0,
            ..Default::default()
        };
        let verdict = crate::analysis::diagnosis::check_sla(&snap, &sla);
        assert_eq!(verdict.breaches.len(), 1, "{verdict:?}");
        assert!(
            verdict.breaches[0].contains("scheduled arrivals never completed"),
            "{verdict:?}"
        );
        let stalled = crate::analysis::censoring::StalledOpenLoop::of(
            snap.open_loop.as_ref(),
            snap.summary.latency.p99,
        )
        .expect("a stalled open loop");
        assert_eq!(
            (stalled.censored_count, stalled.arrivals_count),
            (ol.missed, ol.intended_count)
        );
        assert_eq!(stalled.p99_at_least_ms, snap.summary.latency.p99);
    }

    /// W10: an open loop that falls behind stamps each operation when it actually starts, so
    /// the timeline spreads them over the window; the lateness stays in the latency as queue.
    #[tokio::test]
    async fn a_backlogged_open_loop_stamps_operations_when_they_start() {
        // One slot against a 20 ms server at 100 arrivals/s: about 45 operations a second
        // start, ever later behind their schedule.
        let lab = crate::lab::LabServer::spawn(
            "127.0.0.1:0",
            crate::lab::LabConfig {
                base_ttfb_delay: Duration::from_millis(20),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut p = lab.profile("b");
        p.workload = profile("memory://").workload;
        p.workload.concurrency = 1;
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = Knobs::of(&s.profile);
        let snap = s
            .run(&knobs, &opts(2_000, Some(100.0)), None)
            .await
            .unwrap();
        let ops: Vec<u64> = snap.timeline.iter().map(|t| t.ops_count).collect();
        assert!(ops.len() >= 2, "{ops:?}");
        // 9.0: every started operation was due in the first half second: [n, 0].
        assert!(ops[1] * 2 >= ops[0], "{ops:?}");
        // The lateness is in the latency (from the intended start), as queue time.
        let stage = snap.open_loop.as_ref().unwrap().schedule.unwrap();
        assert!(stage.max > 500.0, "{stage:?}");
        assert!(
            snap.summary.latency.max >= stage.max,
            "{:?}",
            snap.summary.latency
        );
        let v = &snap.verbs[0];
        assert!(
            v.breakdown.queue_ms > v.breakdown.backend_ms,
            "{:?}",
            v.breakdown
        );
    }

    #[tokio::test]
    async fn poisson_and_shaped_arrivals_follow_their_schedule() {
        let mut p = profile("memory://");
        p.workload.arrivals = Arrivals::Poisson;
        p.workload.concurrency = 8;
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = Knobs::of(&s.profile);
        let snap = s.run(&knobs, &opts(1000, Some(400.0)), None).await.unwrap();
        let ol = snap.open_loop.unwrap();
        assert_eq!(ol.arrivals, "poisson");
        assert!(
            (ol.intended_count as f64 - 400.0).abs() < 400.0 * 0.15,
            "{ol:?}"
        );
        // 8 slots keep up with 400/s against memory://: no backlog (the `schedule_backlog`
        // threshold is 1 %). An arrival due in the last timer tick before the window closes may
        // still be censored on a coarse-timer platform (Windows: ~15 ms ticks).
        assert!(
            ol.missed as f64 <= ol.intended_count as f64 * 0.01,
            "{ol:?}"
        );

        let mut p = profile("memory://");
        p.workload.shape = Some("ramp:0..400/1s".parse().unwrap());
        p.workload.concurrency = 8;
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = Knobs::of(&s.profile);
        let o = RunOptions::from_profile(&s.profile);
        assert!(o.rate.is_none());
        let snap = s.run(&knobs, &opts(1000, None), None).await.unwrap();
        let ol = snap.open_loop.unwrap();
        assert_eq!(ol.arrivals, "ramp");
        assert!((ol.intended_count as i64 - 200).abs() <= 5, "{ol:?}");
        assert!((ol.target_rate - 200.0).abs() < 5.0, "{ol:?}");
    }

    #[tokio::test]
    async fn lab_protocols_and_the_http2_preflight() {
        use crate::net::HttpVersion;
        let mut ok = Vec::new();
        for (url, http, microscope) in [
            ("lab://clean", HttpVersion::H2, false),
            ("lab://clean?proto=https", HttpVersion::Auto, false),
            ("lab://clean?proto=h2", HttpVersion::H2, false),
        ] {
            let mut p = profile(url);
            p.operator.http.version = http;
            p.workload.concurrency = 4;
            let s = Session::open(p, Cancel::new(), quiet())
                .await
                .unwrap_or_else(|e| panic!("{url} {http:?} {microscope}: {e:#}"));
            let knobs = Knobs::of(&s.profile);
            let snap = s.run(&knobs, &opts(300, None), None).await.unwrap();
            assert_eq!(snap.summary.failed_ops, 0, "{url} {http:?}");
            // (OpenDAL's own client negotiates HTTP/1.1 unless HTTP/2 is asked for.)
            if http == HttpVersion::H2 || microscope {
                assert!(
                    snap.connections.http2_requests > 0,
                    "{url} {http:?} {microscope}"
                );
            }
            // Warmup took the dials; the cold start keeps their cost.
            let cold = snap.connections.cold_start.as_ref().expect("cold start");
            assert!(
                cold.connections_count > 0 && cold.connect.p50 > 0.0,
                "{cold:?}"
            );
            if url.contains("https") || url.contains("proto=h2") {
                assert!(!snap.connections.tls_version.is_empty(), "{url}");
            }
            ok.push(url);
        }
        // A server without HTTP/2 is refused up front with a hint, not failed request by request.
        let mut p = profile("lab://clean?proto=h1");
        p.operator.http.version = HttpVersion::H2;
        let e = Session::open(p, Cancel::new(), quiet())
            .await
            .err()
            .expect("refused");
        let c = crate::errors::classify(&e, "s3");
        assert_eq!(c.code, crate::errors::ErrorCode::NetHttp, "{e:#}");
        assert!(c.hint.unwrap_or_default().contains("proto=h2c"));
        assert!(crate::lab::LabProto::parse("h3").is_err());
    }
}
