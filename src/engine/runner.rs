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
use super::operator::{admin_operator, base_operator, measured_operator};
use super::ops::{OpCtx, error_class, execute, verb};
use crate::config::{Knobs, OpKind, OpSpec, Profile};
use crate::guard::Guard;
use crate::trace::{OpCompletion, OpScope, OpenLoopStats, Phase, Recorder, Snapshot, TraceRows};

/// Cooperative cancellation shared with Ctrl-C handlers and the TUI.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }
    /// Resolves once [`Cancel::cancel`] has been called (polled every 50 ms).
    pub async fn cancelled(&self) {
        while !self.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(50)).await;
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
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            run_prefix: None,
            guard: None,
            trace_max_requests: crate::trace::DEFAULT_MAX_REQUESTS,
            preflight: true,
        }
    }
}

pub struct Session {
    pub profile: Profile,
    pub base: Operator,
    pub admin: Operator,
    pub run_prefix: String,
    pub dataset: Arc<Dataset>,
    pub mix: Vec<OpSpec>,
    pub warnings: Vec<String>,
    pool: Arc<PayloadPool>,
    written: Arc<Mutex<Vec<(String, u32)>>>,
    pub cancel: Cancel,
    /// The service's full capability (limits OpenDAL applies, e.g. the minimum part size).
    pub capability: opendal::Capability,
    trial: AtomicU64,
    guard: Option<Arc<Guard>>,
    trace_max_requests: usize,
    last_trace: Mutex<Option<TraceRows>>,
}

impl Session {
    /// Connect to the target and prepare the dataset.
    pub async fn open(profile: Profile, cancel: Cancel, progress: ProgressFn) -> Result<Self> {
        Self::open_with(profile, cancel, progress, SessionOptions::default()).await
    }

    /// [`Session::open`] with safety rails: preflight before any write, budgets armed on
    /// `cancel`, a chosen run prefix.
    pub async fn open_with(
        mut profile: Profile,
        cancel: Cancel,
        progress: ProgressFn,
        opts: SessionOptions,
    ) -> Result<Self> {
        profile.target = profile.target.normalized()?;
        profile.validate()?;
        let base = base_operator(&profile.target)?;
        let admin = admin_operator(&base, &profile.target)?;
        let run_prefix = opts.run_prefix.clone().unwrap_or_else(new_run_prefix);
        if let Some(g) = &opts.guard {
            g.arm(cancel.clone());
        }
        let spec = profile.workload.effective_dataset();
        let mut mix = profile.workload.effective_mix();
        let mut warnings = Vec::new();
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
        if profile.target.is_network() {
            super::operator::check_reachable(&base, &profile.target).await?;
        }
        let needs_objects = mix.iter().any(|m| !m.op.is_write() && m.weight > 0);
        if opts.preflight {
            let writes = spec.existing_prefix.is_none()
                && (needs_objects || mix.iter().any(|m| m.weight > 0 && m.op.is_write()));
            // Fail fast: few retries, short timeouts (the admin operator retries for minutes).
            let quick = super::operator::preflight_operator(&base, &profile.target)?;
            crate::guard::preflight(&quick, &run_prefix, writes).await?;
        }
        let p2 = progress.clone();
        let dataset = Dataset::prepare(
            &admin,
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
        let max_write = mix
            .iter()
            .filter_map(|m| match &m.op {
                OpKind::Write { size } | OpKind::MultipartWrite { size } => Some(size.as_usize()),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let capability = base.info().capability();
        Ok(Self {
            profile,
            base,
            admin,
            run_prefix,
            dataset: Arc::new(dataset),
            mix,
            warnings,
            pool: Arc::new(PayloadPool::new(max_write)),
            written: Arc::new(Mutex::new(Vec::new())),
            cancel,
            capability,
            trial: AtomicU64::new(0),
            guard: opts.guard,
            trace_max_requests: opts.trace_max_requests,
            last_trace: Mutex::new(None),
        })
    }

    /// Trace rows of the last [`Session::run`] (taken: a second call returns `None`).
    pub fn take_trace(&self) -> Option<TraceRows> {
        self.last_trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
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
        if let Some(g) = &self.guard {
            recorder.set_guard(g.clone());
        }
        let tree = opts
            .trace_slow_ops
            .then(|| await_tree::Registry::new(await_tree::Config::default()));
        if let Some(t) = &tree {
            recorder.set_tree_registry(t.clone());
        }
        let op = measured_operator(
            &self.base,
            &self.profile.target,
            knobs,
            &recorder,
            tree.clone(),
        )?;
        let ctx = Arc::new(OpCtx {
            op,
            picker: Picker::new(self.dataset.objects.len(), self.profile.workload.access),
            dataset: self.dataset.clone(),
            knobs: knobs.clone(),
            pool: self.pool.clone(),
            verify: self.profile.workload.verify,
            recorder: recorder.clone(),
            scratch_prefix: format!("{}writes/t{trial:03}-", self.run_prefix),
            written: self.written.clone(),
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
            self.drive(
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
            .await?;
            recorder.set_phase(Phase::Verify);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        stop.store(true, Ordering::Release);
        let _ = sampler.await;
        result?;
        let mut snap = recorder.snapshot();
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
        let seed = self.profile.workload.seed ^ (trial << 32) ^ if measured { 0x5eed } else { 0 };

        match opts.rate {
            None => {
                let mut set = JoinSet::new();
                for w in 0..knobs.concurrency.max(1) {
                    let ctx = ctx.clone();
                    let cum = cum.clone();
                    let stop = should_stop.clone();
                    let close = close_window.clone();
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
                                &kind,
                                &mut rng,
                                slot,
                                Instant::now(),
                                in_window,
                                &rec,
                                tree.as_ref(),
                            )
                            .await;
                        }
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
                let interval = Duration::from_secs_f64(1.0 / rate.max(0.001));
                // Bounded backlog: an arrival more than `timeout` behind schedule could only
                // time out; such arrivals are dropped (counted as missed) instead of queued.
                let max_queue_delay = knobs.timeout.get().max(Duration::from_secs(1));
                let t0 = Instant::now();
                let mut next = t0;
                let mut rng = SmallRng::seed_from_u64(seed);
                let mut missed = 0u64;
                let mut issued = 0u64;
                let mut max_lag = Duration::ZERO;
                let mut set = JoinSet::new();
                'arrivals: loop {
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
                        if should_stop() {
                            break 'arrivals;
                        }
                    }
                    // Wait for a free slot (wrk2-style): the arrival stays queued and its latency
                    // keeps counting from the intended start, so saturation shows in latency.
                    let permit = loop {
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
                    let behind = Instant::now().saturating_duration_since(next);
                    if behind > max_queue_delay {
                        let skip = ((behind - max_queue_delay).as_secs_f64()
                            / interval.as_secs_f64())
                        .ceil()
                        .max(1.0);
                        missed += skip as u64;
                        next += interval.mul_f64(skip);
                        if next >= deadline {
                            break;
                        }
                    }
                    max_lag = max_lag.max(next.elapsed());
                    started.fetch_add(1, Ordering::Relaxed);
                    issued += 1;
                    let kind = pick_kind(&mut rng, &cum);
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
                            &kind,
                            &mut op_rng,
                            lease.1,
                            intended,
                            in_window,
                            &rec,
                            tree.as_ref(),
                        )
                        .await;
                        drop(lease);
                        drop(permit);
                    });
                    next += interval;
                }
                let window_end = Instant::now().min(deadline);
                close_window();
                // Arrivals that were due inside the window but never got a slot.
                if next < window_end && !max_ops.is_some_and(|m| issued >= m) {
                    missed +=
                        ((window_end - next).as_secs_f64() / interval.as_secs_f64()).ceil() as u64;
                }
                join_workers(set).await?;
                if measured {
                    recorder.set_open_loop(OpenLoopStats {
                        target_rate: rate,
                        achieved_rate: issued as f64
                            / window_end
                                .saturating_duration_since(t0)
                                .as_secs_f64()
                                .max(1e-3),
                        missed,
                        max_schedule_lag_ms: max_lag.as_secs_f64() * 1000.0,
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

    /// Delete everything this session wrote (unless disabled) and verify.
    pub async fn close(&self) -> CleanupSummary {
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
        let mut s = delete_prefix(&self.admin, &self.run_prefix).await;
        if self.dataset.preexisting {
            s.kept_dataset = Some(self.dataset.prefix.clone());
        }
        s
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_one(
    ctx: &Arc<OpCtx>,
    kind: &OpKind,
    rng: &mut SmallRng,
    slot: u64,
    intended: Instant,
    measured: bool,
    rec: &Arc<Recorder>,
    tree: Option<&await_tree::Registry>,
) {
    // `measured`: the caller decided, when it issued the operation, that it belongs to the
    // measurement window.
    let scope = OpScope::new_at(measured && rec.is_measuring(), intended, Some(rec.clone()));
    let measured = scope.measured;
    let name = verb(kind);
    rec.op_started_at(scope.id, intended);
    let mut key = String::new();
    let fut = scope.run(execute(ctx, kind, rng, slot, &mut key));
    let res = match tree {
        Some(reg) => {
            scope.set_tree_registry(reg.clone());
            reg.register(scope.id, name).instrument(fut).await
        }
        None => fut.await,
    };
    let latency_us = intended.elapsed().as_micros() as u64;
    let (lr, lw, error) = match res {
        Ok(o) => (o.logical_read, o.logical_write, None),
        Err(e) => {
            let status = scope.with(|d| d.requests.last().map(|r| r.status));
            (0, 0, Some(error_class(&e, status)))
        }
    };
    rec.complete_op(
        measured,
        OpCompletion {
            verb: name,
            op_id: scope.id,
            started: intended,
            latency_us,
            logical_read: lr,
            logical_write: lw,
            error,
            key,
            data: scope.take(),
        },
    );
}
