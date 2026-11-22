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
    /// The non-default load generator (`profile.client`, design v5 §7), started for this
    /// session; `None`: OpenDAL's own HTTP stack.
    client: Option<Arc<crate::clients::Driver>>,
    /// The traffic log replayed instead of the mix (`workload.replay`).
    replay: Option<Arc<super::replay::ReplayLog>>,
    /// Next log event to replay (windows continue where the previous one stopped).
    replay_cursor: Mutex<usize>,
    /// Where the next window streams its rows (the run file), taken by [`Session::run`].
    row_sink: Mutex<Option<Arc<dyn crate::trace::RowSink>>>,
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
        super::operator::prepare_target(&mut profile.target, &profile.knobs).await?;
        let client =
            crate::clients::Driver::start(&profile.client, &profile.target, &profile.knobs).await?;
        let client_warnings = match &client {
            Some(c) => c.adjust(&mut profile),
            None => Vec::new(),
        };
        let base = base_operator(&profile.target)?;
        let admin = admin_operator(&base, &profile.target)?;
        let run_prefix = opts.run_prefix.clone().unwrap_or_else(new_run_prefix);
        if let Some(g) = &opts.guard {
            g.arm(cancel.clone());
        }
        let spec = profile.workload.effective_dataset();
        let mut mix = profile.workload.effective_mix();
        // The mix runs on what the measured client can do.
        let measured_cap = client
            .as_ref()
            .map_or_else(|| base.info().capability(), |c| c.capability());
        let mut warnings =
            super::workloads::adapt_mix(&mut mix, &measured_cap, &profile.target.scheme);
        warnings.extend(client_warnings);
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
            super::operator::check_reachable_with(&base, &profile.target, &profile.knobs).await?;
            super::operator::check_protocol(&base, &profile.target, &profile.knobs).await?;
        }
        let replay = match &profile.workload.replay {
            Some(spec) => {
                let log = super::replay::ReplayLog::load(spec, &run_prefix)?;
                if log.refused_writes_count > 0 {
                    warnings.push(format!(
                        "replay: {} writes/deletes outside the run prefix were refused (map them with --map-prefix FROM={}…)",
                        log.refused_writes_count,
                        super::replay::RUN_PREFIX_TOKEN
                    ));
                }
                Some(Arc::new(log))
            }
            None => None,
        };
        let needs_objects =
            replay.is_none() && mix.iter().any(|m| m.op.needs_dataset() && m.weight > 0);
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
        let capability = measured_cap;
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
            client,
            replay,
            replay_cursor: Mutex::new(0),
            row_sink: Mutex::new(None),
        })
    }

    /// Trace rows of the last [`Session::run`] (taken: a second call returns `None`).
    pub fn take_trace(&self) -> Option<TraceRows> {
        self.last_trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Keep only this worker's share of the replayed log (a distributed run, design v5 §8):
    /// events are split by key, so every key's operations stay in order on one worker. Keys are
    /// compared without `run_prefix` (each worker maps `$RUN/` onto its own prefix).
    pub fn shard_replay(&mut self, run_prefix: &str, index: u32, count: u32) {
        let Some(log) = &self.replay else { return };
        if count <= 1 {
            return;
        }
        let mut log = (**log).clone();
        log.events.retain(|e| {
            let k = e.op.key();
            let k = k.strip_prefix(run_prefix).unwrap_or(k);
            super::replay::shard_of(k, count) == index
        });
        self.replay = Some(Arc::new(log));
    }

    /// The non-default load generator of this session (design v5 §7).
    pub fn client(&self) -> Option<&Arc<crate::clients::Driver>> {
        self.client.as_ref()
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
        if let Some(g) = &self.guard {
            recorder.set_guard(g.clone());
        }
        let tree = opts
            .trace_slow_ops
            .then(|| await_tree::Registry::new(await_tree::Config::default()));
        if let Some(t) = &tree {
            recorder.set_tree_registry(t.clone());
        }
        let op = match &self.client {
            Some(c) => c.operator(&self.profile.target, knobs, &recorder)?,
            None => measured_operator(
                &self.base,
                &self.profile.target,
                knobs,
                &recorder,
                tree.clone(),
            )?,
        };
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
            state: super::workloads::WorkloadState::new(self.capability, &self.mix),
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
        let seed = self.profile.workload.seed ^ (trial << 32) ^ if measured { 0x5eed } else { 0 };

        let rate = opts.rate.or_else(|| {
            (self.profile.workload.shape.is_some() || self.replay.is_some())
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
                let wl = &self.profile.workload;
                let t0 = Instant::now();
                let horizon = deadline.saturating_duration_since(t0);
                let mut src = match &self.replay {
                    Some(log) => {
                        let idx = *self.replay_cursor.lock().unwrap_or_else(|e| e.into_inner());
                        Source::Replay {
                            log,
                            idx,
                            base: log.events.get(idx).map_or(Duration::ZERO, |e| e.at),
                        }
                    }
                    None => Source::Clock {
                        clock: super::schedule::ArrivalClock::new(
                            wl.arrivals,
                            wl.shape.clone(),
                            rate,
                            horizon,
                        ),
                        rng: SmallRng::seed_from_u64(seed ^ 0xA771_7A15),
                    },
                };
                // Bounded backlog: an arrival more than `timeout` behind schedule could only
                // time out; such arrivals are dropped (missed, censored) instead of queued.
                let max_queue_delay = knobs.timeout.get().max(Duration::from_secs(1));
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
                let mut pending = src.next(horizon).map(|(d, j)| (t0 + d, j));
                // `pending` holds the next arrival until it is dispatched, so the arrivals still
                // due when the loop stops are counted as missed below.
                'arrivals: while let Some((mut next, mut job)) = pending.clone() {
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
                    // Arrivals too far behind are dropped: each is missed, with its wait so far
                    // as the lower bound of its latency.
                    let now = Instant::now();
                    while now.saturating_duration_since(next) > max_queue_delay {
                        missed += 1;
                        missed_arrival(next, now);
                        match src.next(horizon) {
                            Some((d, j)) if t0 + d < deadline => (next, job) = (t0 + d, j),
                            Some((d, j)) => {
                                pending = Some((t0 + d, j));
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
                    let job = match job {
                        Some(r) => Job::Replay(r),
                        None => Job::Mix(pick_kind(&mut rng, &cum)),
                    };
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
                    pending = src.next(horizon).map(|(d, j)| (t0 + d, j));
                }
                let window_end = Instant::now().min(deadline);
                close_window();
                // Arrivals that were due inside the window but never started: censored at
                // (window end − intended start).
                if !max_ops.is_some_and(|m| issued >= m) {
                    while let Some((at, _)) = pending.as_ref().filter(|(t, _)| *t < window_end) {
                        missed += 1;
                        missed_arrival(*at, window_end);
                        pending = src.next(horizon).map(|(d, j)| (t0 + d, j));
                    }
                }
                src.save_cursor(pending.is_some(), &self.replay_cursor);
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

    /// Delete everything this session wrote (unless disabled) and verify.
    pub async fn close(&self) -> CleanupSummary {
        if let Some(c) = &self.client {
            c.shutdown().await;
        }
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
    job: &Job,
    rng: &mut SmallRng,
    slot: u64,
    intended: Instant,
    open_loop: bool,
    measured: bool,
    rec: &Arc<Recorder>,
    tree: Option<&await_tree::Registry>,
) {
    // The `schedule` stage: how late the operation starts against its intended start.
    let schedule_us = open_loop.then(|| intended.elapsed().as_micros() as u64);
    // `measured`: the caller decided, when it issued the operation, that it belongs to the
    // measurement window.
    let scope = OpScope::new_at(measured && rec.is_measuring(), intended, Some(rec.clone()));
    let measured = scope.measured;
    let name = match job {
        Job::Mix(kind) => verb(kind),
        Job::Replay(op) => op.verb(),
    };
    rec.op_started_at(scope.id, intended);
    let mut key = String::new();
    let fut = scope.run(async {
        match job {
            Job::Mix(kind) => execute(ctx, kind, rng, slot, &mut key).await,
            Job::Replay(op) => super::replay::execute(ctx, op, &mut key).await,
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
            schedule_us,
        },
    );
}

/// What one arrival runs: an operation of the mix, or a replayed log entry.
enum Job {
    Mix(OpKind),
    Replay(Arc<super::replay::ReplayOp>),
}

/// Where open-loop arrivals come from.
enum Source<'a> {
    /// A rate (constant, Poisson, shaped); the operation is picked from the mix at dispatch.
    Clock {
        clock: super::schedule::ArrivalClock,
        rng: SmallRng,
    },
    /// A traffic log from event `idx` on, its times relative to `base`.
    Replay {
        log: &'a super::replay::ReplayLog,
        idx: usize,
        base: Duration,
    },
}

impl Source<'_> {
    /// The next arrival (offset from the window start) before `horizon`.
    fn next(
        &mut self,
        horizon: Duration,
    ) -> Option<(Duration, Option<Arc<super::replay::ReplayOp>>)> {
        match self {
            Source::Clock { clock, rng } => clock.next(rng, horizon).map(|d| (d, None)),
            Source::Replay { log, idx, base } => {
                let e = log.events.get(*idx)?;
                let at = e.at.saturating_sub(*base);
                if at >= horizon {
                    return None;
                }
                *idx += 1;
                Some((at, Some(e.op.clone())))
            }
        }
    }

    fn label(&self) -> String {
        match self {
            Source::Clock { clock, .. } => clock.label(),
            Source::Replay { .. } => "replay".into(),
        }
    }

    /// The mean intended rate over the window (the target rate of a shaped schedule or log).
    fn mean_rate(&self, window: Duration, intended: u64) -> f64 {
        match self {
            Source::Clock { clock, .. } => {
                let steps = 200u32;
                if window.is_zero() {
                    return clock.rate_at(Duration::ZERO);
                }
                let dt = window / steps;
                (0..steps)
                    .map(|i| clock.rate_at(dt * i + dt / 2))
                    .sum::<f64>()
                    / f64::from(steps)
            }
            Source::Replay { .. } => intended as f64 / window.as_secs_f64().max(1e-3),
        }
    }

    /// Remember where the next window of a replay starts (`holding`: one event was taken from
    /// the log but not dispatched).
    fn save_cursor(&self, holding: bool, cursor: &Mutex<usize>) {
        if let Source::Replay { idx, .. } = self {
            *cursor.lock().unwrap_or_else(|e| e.into_inner()) = idx - usize::from(holding);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Arrivals, TargetSpec};

    fn quiet() -> ProgressFn {
        Arc::new(|_| {})
    }

    fn profile(url: &str) -> Profile {
        let mut p = Profile::from_target(TargetSpec::from_url(url).unwrap());
        p.workload.preset = crate::config::Preset::Metadata;
        p.workload.dataset = Some(crate::config::DatasetSpec {
            objects: 4,
            object_size: crate::util::ByteSize::kib(4),
            ..Default::default()
        });
        p.workload.verify = false;
        p.knobs.concurrency = 1;
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
        let knobs = s.profile.knobs.clone();
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
        p.knobs.concurrency = 1;
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = s.profile.knobs.clone();
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
    }

    #[tokio::test]
    async fn poisson_and_shaped_arrivals_follow_their_schedule() {
        let mut p = profile("memory://");
        p.workload.arrivals = Arrivals::Poisson;
        p.knobs.concurrency = 8;
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = s.profile.knobs.clone();
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
        p.knobs.concurrency = 8;
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = s.profile.knobs.clone();
        let o = RunOptions::from_profile(&s.profile);
        assert!(o.rate.is_none());
        let snap = s.run(&knobs, &opts(1000, None), None).await.unwrap();
        let ol = snap.open_loop.unwrap();
        assert_eq!(ol.arrivals, "ramp");
        assert!((ol.intended_count as i64 - 200).abs() <= 5, "{ol:?}");
        assert!((ol.target_rate - 200.0).abs() < 5.0, "{ol:?}");
    }

    #[tokio::test]
    async fn replays_a_cloudtrail_log_on_its_timing() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("trail.json");
        let rec = |t: &str, name: &str, key: &str| {
            format!(
                r#"{{"eventTime":"2024-03-01T10:00:{t}Z","eventName":"{name}","requestParameters":{{"key":"{key}"}},"additionalEventData":{{"bytesTransferredIn":2048}}}}"#
            )
        };
        let lines = [
            rec("00.000", "PutObject", "k/1"),
            rec("00.100", "GetObject", "k/1"),
            rec("00.200", "HeadObject", "k/1"),
            rec("00.250", "PutObject", "outside/2"),
            rec("00.300", "ListObjectsV2", ""),
            rec("09.000", "GetObject", "k/1"),
        ];
        std::fs::write(&f, lines.join("\n")).unwrap();
        let mut p = profile("memory://");
        let mut spec = crate::engine::replay::ReplaySpec::new(f);
        spec.map_prefix = vec![("k/".into(), "$RUN/k/".into())];
        p.workload.replay = Some(spec);
        p.knobs.concurrency = 4;
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        assert!(
            s.warnings.iter().any(|w| w.contains("1 writes/deletes")),
            "{:?}",
            s.warnings
        );
        let knobs = s.profile.knobs.clone();
        // (A warmup would replay the start of the log.)
        let o = RunOptions {
            warmup: Duration::ZERO,
            ..opts(800, None)
        };
        let snap = s.run(&knobs, &o, None).await.unwrap();
        let ol = snap.open_loop.clone().unwrap();
        assert_eq!(ol.arrivals, "replay");
        // The 9 s event is outside the 0.8 s window.
        assert_eq!(ol.intended_count, 4, "{ol:?}");
        assert_eq!(snap.summary.failed_ops, 0, "{:?}", snap.resilience);
        let verbs: Vec<&str> = snap.verbs.iter().map(|v| v.verb.as_str()).collect();
        assert_eq!(
            verbs,
            ["replay.list", "replay.read", "replay.stat", "replay.write"]
        );
        // The next window continues with the rest of the log.
        assert_eq!(*s.replay_cursor.lock().unwrap(), 4);
    }

    #[tokio::test]
    async fn lab_protocols_and_the_http2_preflight() {
        use crate::net::HttpVersion;
        let mut ok = Vec::new();
        for (url, http, microscope) in [
            ("lab://clean", HttpVersion::H2, false),
            ("lab://clean", HttpVersion::H2, true),
            ("lab://clean?proto=https", HttpVersion::Auto, false),
            ("lab://clean?proto=h2", HttpVersion::H2, false),
            ("lab://clean?proto=h2", HttpVersion::H2, true),
        ] {
            let mut p = profile(url);
            p.knobs.http_version = http;
            if microscope {
                p.knobs.transport = crate::config::TransportMode::Microscope;
            }
            p.knobs.concurrency = 4;
            let s = Session::open(p, Cancel::new(), quiet())
                .await
                .unwrap_or_else(|e| panic!("{url} {http:?} {microscope}: {e:#}"));
            let knobs = s.profile.knobs.clone();
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
        p.knobs.http_version = HttpVersion::H2;
        let e = Session::open(p, Cancel::new(), quiet())
            .await
            .err()
            .expect("refused");
        let c = crate::errors::classify(&e, "s3");
        assert_eq!(c.code, crate::errors::ErrorCode::NetHttp, "{e:#}");
        assert!(c.hint.unwrap_or_default().contains("proto=h2c"));
        assert!(crate::lab::LabProto::parse("h3").is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn microscope_uploads_end_at_the_kernel_acknowledgement() {
        // The server thinks 20 ms before answering: the acknowledgement comes well before
        // the first byte (on loopback with no delay they are indistinguishable).
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
        p.knobs.concurrency = 1;
        p.knobs.transport = crate::config::TransportMode::Microscope;
        p.workload.mix = vec![crate::config::OpSpec {
            op: OpKind::Write {
                size: crate::util::ByteSize::kib(512),
            },
            weight: 1,
        }];
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = s.profile.knobs.clone();
        s.run(&knobs, &opts(400, None), None).await.unwrap();
        let rows = s.take_trace().unwrap();
        let puts: Vec<_> = rows
            .reqs
            .iter()
            .filter(|r| r.bytes_up >= 512 * 1024)
            .collect();
        assert!(!puts.is_empty());
        let acked: Vec<_> = puts
            .iter()
            .filter_map(|r| {
                r.detail
                    .as_ref()
                    .and_then(|d| Some((r, d.upload_acked_us?, d.send_buffered_us?)))
            })
            .collect();
        assert!(
            acked.len() * 2 >= puts.len(),
            "{} of {}",
            acked.len(),
            puts.len()
        );
        for (r, at, buffered) in acked {
            assert!(r.send_us >= buffered, "{r:?}");
            assert!(at <= r.total_us, "{r:?}");
        }
        // Kernel fields beyond the smoothed RTT are kept.
        assert!(
            rows.reqs
                .iter()
                .any(|r| r.tcp.as_ref().is_some_and(|t| t.bytes_acked > 0))
        );
    }
}
