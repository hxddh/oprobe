//! Benchmark session: prepares the dataset once, runs one or many measurement windows
//! (closed- or open-loop) with different knobs, verifies integrity and cleans up.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use opendal::Operator;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use tokio::sync::Semaphore;

use super::cleanup::{delete_prefix, new_run_prefix, CleanupSummary};
use super::dataset::{Dataset, PayloadPool, Picker};
use super::operator::{admin_operator, base_operator, measured_operator};
use super::ops::{error_class, execute, verb, OpCtx};
use crate::config::{Knobs, OpKind, OpSpec, Profile};
use crate::trace::{OpCompletion, OpScope, OpenLoopStats, Phase, Recorder, Snapshot};

/// Cooperative cancellation shared with Ctrl-C handlers and the TUI.
#[derive(Clone, Default)]
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
    trial: AtomicU64,
}

impl Session {
    /// Connect to the target and prepare the dataset.
    pub async fn open(mut profile: Profile, cancel: Cancel, progress: ProgressFn) -> Result<Self> {
        profile.target = profile.target.normalized()?;
        profile.validate()?;
        let base = base_operator(&profile.target)?;
        let admin = admin_operator(&base, &profile.target)?;
        let run_prefix = new_run_prefix();
        let spec = profile.workload.effective_dataset();
        let mut mix = profile.workload.effective_mix();
        let mut warnings = Vec::new();
        if spec.existing_prefix.is_some() && mix.iter().any(|m| m.op.is_write()) {
            mix.retain(|m| !m.op.is_write());
            warnings.push("existing_prefix datasets are read-only: write operations were removed from the mix".into());
            anyhow::ensure!(
                !mix.is_empty(),
                "the workload mix only contains writes, but the dataset is read-only"
            );
        }
        let needs_objects = mix.iter().any(|m| !m.op.is_write() && m.weight > 0);
        let p2 = progress.clone();
        let dataset = Dataset::prepare(
            &admin,
            &spec,
            profile.workload.seed,
            &run_prefix,
            needs_objects,
            &move |m| p2(Progress::Message(m)),
        )
        .await?;
        if let Some(OpKind::Read { size: Some(sz) }) = mix
            .iter()
            .map(|m| &m.op)
            .find(|o| matches!(o, OpKind::Read { size: Some(_) }))
        {
            if let Some(min) = dataset.objects.iter().map(|o| o.size).min() {
                if sz.0 > min {
                    warnings.push(format!("range size {sz} exceeds the smallest object ({min} B); those reads become whole-object reads"));
                }
            }
        }
        let max_write = mix
            .iter()
            .filter_map(|m| match &m.op {
                OpKind::Write { size } | OpKind::MultipartWrite { size } => Some(size.as_usize()),
                _ => None,
            })
            .max()
            .unwrap_or(0);
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
            trial: AtomicU64::new(0),
        })
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
            tokio::spawn(async move {
                let mut cpu = super::procstat::CpuMeter::new();
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                let mut n = 0u64;
                while !stop.load(Ordering::Acquire) {
                    tick.tick().await;
                    n += 1;
                    rec.sample_trees(rec.current_p90_us().max(2_000));
                    if n.is_multiple_of(50) {
                        if let Some((pct, rss)) = cpu.sample() {
                            rec.update_client(pct, rss);
                        }
                    }
                    if n.is_multiple_of(12) {
                        if let Some(cb) = &progress {
                            cb(Progress::Snapshot(Box::new(rec.snapshot())));
                        }
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
                .await;
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
            .await;
            recorder.set_phase(Phase::Verify);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        stop.store(true, Ordering::Release);
        let _ = sampler.await;
        result?;
        let mut snap = recorder.snapshot();
        snap.phase = Phase::Done;
        if let Some(cb) = &progress {
            cb(Progress::Snapshot(Box::new(snap.clone())));
        }
        Ok(snap)
    }

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
    ) {
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
        let seed = self.profile.workload.seed ^ (trial << 32) ^ if measured { 0x5eed } else { 0 };

        match opts.rate {
            None => {
                let mut handles = Vec::new();
                for w in 0..knobs.concurrency.max(1) {
                    let ctx = ctx.clone();
                    let cum = cum.clone();
                    let stop = should_stop.clone();
                    let started = started.clone();
                    let rec = recorder.clone();
                    let tree = tree.cloned();
                    handles.push(tokio::spawn(async move {
                        let mut rng =
                            SmallRng::seed_from_u64(seed ^ (w as u64).wrapping_mul(0x9E37_79B9));
                        let mut seq = 0u64;
                        while !stop() {
                            started.fetch_add(1, Ordering::Relaxed);
                            let kind = pick_kind(&mut rng, &cum);
                            let slot = (w as u64) * 4 + seq % 4;
                            seq += 1;
                            run_one(
                                &ctx,
                                &kind,
                                &mut rng,
                                slot,
                                Instant::now(),
                                measured,
                                &rec,
                                tree.as_ref(),
                            )
                            .await;
                        }
                    }));
                }
                for h in handles {
                    let _ = h.await;
                }
            }
            Some(rate) => {
                let cap = Arc::new(Semaphore::new(knobs.concurrency.max(1)));
                let interval = Duration::from_secs_f64(1.0 / rate.max(0.001));
                let t0 = Instant::now();
                let mut next = t0;
                let mut rng = SmallRng::seed_from_u64(seed);
                let mut missed = 0u64;
                let mut issued = 0u64;
                let mut max_lag = Duration::ZERO;
                let mut seq = 0u64;
                let mut handles = Vec::new();
                while !should_stop() {
                    tokio::time::sleep_until(next.into()).await;
                    let lag = next.elapsed();
                    max_lag = max_lag.max(lag);
                    match cap.clone().try_acquire_owned() {
                        Ok(permit) => {
                            started.fetch_add(1, Ordering::Relaxed);
                            issued += 1;
                            let kind = pick_kind(&mut rng, &cum);
                            let mut op_rng = SmallRng::seed_from_u64(rng.gen());
                            let slot = seq % (knobs.concurrency as u64 * 4).max(4);
                            seq += 1;
                            let ctx = ctx.clone();
                            let rec = recorder.clone();
                            let tree = tree.cloned();
                            let intended = next;
                            handles.push(tokio::spawn(async move {
                                run_one(
                                    &ctx,
                                    &kind,
                                    &mut op_rng,
                                    slot,
                                    intended,
                                    measured,
                                    &rec,
                                    tree.as_ref(),
                                )
                                .await;
                                drop(permit);
                            }));
                        }
                        Err(_) => missed += 1,
                    }
                    next += interval;
                    handles.retain(|h| !h.is_finished());
                }
                for h in handles {
                    let _ = h.await;
                }
                if measured {
                    recorder.set_open_loop(OpenLoopStats {
                        target_rate: rate,
                        achieved_rate: issued as f64 / t0.elapsed().as_secs_f64().max(1e-3),
                        missed,
                        max_schedule_lag_ms: max_lag.as_secs_f64() * 1000.0,
                    });
                }
            }
        }
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
    let scope = OpScope::new(measured && rec.is_measuring());
    let measured = scope.measured;
    let name = verb(kind);
    rec.op_started(scope.id);
    let fut = scope.run(execute(ctx, kind, rng, slot));
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
        Err(e) => (0, 0, Some(error_class(&e))),
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
            data: scope.take(),
        },
    );
}
