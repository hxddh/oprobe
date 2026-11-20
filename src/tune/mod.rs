//! Closed-loop tuner: measures real trials against the target, finds the throttling ceiling,
//! refines workload-relevant knobs by coordinate descent and confirms the winner.

pub mod codegen;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::analysis::cost::cost_report;
use crate::config::{Access, Knobs, OpKind, TransportMode};
use crate::engine::{RunOptions, Session};
use crate::net::HttpVersion;
use crate::report::TuneSummary;
use crate::trace::Snapshot;
use crate::util::ByteSize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Objective {
    /// Maximize throughput within the SLA.
    #[default]
    Throughput,
    /// Minimize worst p99 while keeping ≥ 80% of the best throughput.
    Latency,
    /// Minimize cost per operation while keeping ≥ 80% of the best throughput.
    Cost,
}

impl Objective {
    pub fn label(&self) -> &'static str {
        match self {
            Objective::Throughput => "throughput",
            Objective::Latency => "latency",
            Objective::Cost => "cost",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TuneOptions {
    pub objective: Objective,
    pub trial: RunOptions,
    pub max_concurrency: usize,
    pub max_trials: usize,
    /// Pause after a throttled trial so the service's rate limiter recovers. The service's
    /// `Retry-After` (capped at [`MAX_RETRY_AFTER`]) is honored even when this is zero.
    pub cooldown: Duration,
}

/// Longest `Retry-After` the tuner waits for between trials.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Trial {
    pub id: usize,
    pub stage: String,
    pub knobs: Knobs,
    /// Successful operations per second (failed operations never count toward the objective).
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    /// Worst per-operation p99.
    pub p99_ms: f64,
    pub error_rate: f64,
    /// Fraction of operations that saw a 429/503.
    pub throttle_rate: f64,
    pub throttled_responses: u64,
    pub cost_per_million_ops: f64,
    pub feasible: bool,
    pub note: String,
}

impl Trial {
    pub fn throttled(&self) -> bool {
        self.throttled_responses > 0
    }
}

pub enum TuneEvent {
    Started {
        id: usize,
        stage: String,
        knobs: Knobs,
    },
    Finished(Trial),
    Message(String),
}

pub struct TuneOutcome {
    pub summary: TuneSummary,
    pub winner: Knobs,
    pub winner_snapshot: Snapshot,
}

struct Tuner<'a> {
    session: &'a Session,
    opts: &'a TuneOptions,
    trials: Vec<Trial>,
    snapshots: Vec<Snapshot>,
    use_bytes: bool,
    on_event: Arc<dyn Fn(TuneEvent) + Send + Sync>,
    /// Concurrency limit once a throttling ceiling is known (80 % of it): no winner may exceed it.
    concurrency_cap: Option<usize>,
}

impl Tuner<'_> {
    fn metric(&self, t: &Trial) -> f64 {
        if self.use_bytes {
            t.mib_per_s
        } else {
            t.ops_per_s
        }
    }

    async fn trial(
        &mut self,
        stage: &str,
        knobs: &Knobs,
        opts: Option<&RunOptions>,
    ) -> Result<usize> {
        if let Some(i) = self
            .trials
            .iter()
            .position(|t| &t.knobs == knobs && t.stage != "confirm")
            && stage != "confirm"
        {
            return Ok(i);
        }
        let id = self.trials.len();
        (self.on_event)(TuneEvent::Started {
            id,
            stage: stage.into(),
            knobs: knobs.clone(),
        });
        let snap = self
            .session
            .run(knobs, opts.unwrap_or(&self.opts.trial), None)
            .await?;
        let p = &self.session.profile;
        let worst_p99 = snap.verbs.iter().map(|v| v.latency.p99).fold(0.0, f64::max);
        let throttled_ops: u64 = snap.verbs.iter().map(|v| v.throttled).sum();
        let ops = snap.summary.ops.max(1) as f64;
        let cost = cost_report(
            &snap,
            &p.cost,
            &p.target.scheme,
            p.target.endpoint().as_deref(),
        );
        let mut t = Trial {
            id,
            stage: stage.into(),
            knobs: knobs.clone(),
            ops_per_s: snap.summary.ops.saturating_sub(snap.summary.failed_ops) as f64
                / snap.elapsed_s.max(1e-3),
            mib_per_s: snap.summary.mib_per_s,
            p50_ms: snap.summary.latency.p50,
            p99_ms: worst_p99,
            error_rate: snap.summary.error_rate,
            throttle_rate: throttled_ops as f64 / ops,
            throttled_responses: snap.resilience.throttled_responses,
            cost_per_million_ops: cost.selected.per_million_ops_usd,
            feasible: true,
            note: String::new(),
        };
        let mut why = Vec::new();
        if snap.summary.ops == 0 {
            why.push("no ops completed".to_owned());
        }
        if t.error_rate > p.sla.max_error_rate {
            why.push(format!("errors {:.2}%", t.error_rate * 100.0));
        }
        let throttle_limit = p.sla.tune_throttle_limit();
        if t.throttle_rate > throttle_limit || (throttle_limit == 0.0 && t.throttled()) {
            why.push(format!("{} throttled responses", t.throttled_responses));
        }
        if let Some(limit) = p.sla.p99_ms
            && worst_p99 > limit
        {
            why.push(format!("p99 {worst_p99:.1}ms > {limit}ms"));
        }
        t.feasible = why.is_empty();
        t.note = why.join(", ");
        (self.on_event)(TuneEvent::Finished(t.clone()));
        let wait = cooldown_after(
            t.throttled(),
            self.opts.cooldown,
            snap.resilience.max_retry_after_ms,
        );
        self.trials.push(t);
        self.snapshots.push(snap);
        if !wait.is_zero() && !self.session.cancel.is_cancelled() {
            (self.on_event)(TuneEvent::Message(format!(
                "throttled: cooling down {:.1}s before the next trial",
                wait.as_secs_f64()
            )));
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.session.cancel.cancelled() => {}
            }
        }
        Ok(id)
    }

    fn budget_left(&self) -> bool {
        self.trials.len() < self.opts.max_trials && !self.session.cancel.is_cancelled()
    }

    /// Trials that may win: not a confirmation run, and within the throttling headroom cap.
    fn eligible(&self, i: usize) -> bool {
        let t = &self.trials[i];
        t.stage != "confirm"
            && self
                .concurrency_cap
                .is_none_or(|cap| t.knobs.concurrency <= cap)
    }

    fn any_feasible(&self) -> bool {
        self.trials
            .iter()
            .any(|t| t.feasible && t.stage != "confirm")
    }

    /// Candidate pool, in order of preference: feasible trials within the headroom cap; any
    /// feasible trial; and only when no trial met the SLA, the least-bad ones (not throttled
    /// if possible, then the lowest error rate).
    fn pool(&self) -> Vec<usize> {
        let all = || (0..self.trials.len()).filter(|&i| self.trials[i].stage != "confirm");
        let capped: Vec<usize> = all()
            .filter(|&i| self.trials[i].feasible && self.eligible(i))
            .collect();
        if !capped.is_empty() {
            return capped;
        }
        let feasible: Vec<usize> = all().filter(|&i| self.trials[i].feasible).collect();
        if !feasible.is_empty() {
            return feasible;
        }
        let mut rest: Vec<usize> = all().filter(|&i| self.eligible(i)).collect();
        if rest.is_empty() {
            rest = all().collect();
        }
        if rest.iter().any(|&i| !self.trials[i].throttled()) {
            rest.retain(|&i| !self.trials[i].throttled());
        }
        let min_err = rest
            .iter()
            .map(|&i| self.trials[i].error_rate)
            .fold(f64::INFINITY, f64::min);
        // Within 0.1 percentage points of the lowest error rate counts as equally clean.
        rest.retain(|&i| self.trials[i].error_rate <= min_err + 0.001);
        rest
    }

    /// Index of the best trial for the objective. Never an SLA-violating trial while any trial
    /// met the SLA.
    fn best(&self) -> Option<usize> {
        let pool = self.pool();
        let top = pool
            .iter()
            .map(|&i| self.metric(&self.trials[i]))
            .fold(0.0, f64::max);
        let near = |i: &usize| self.metric(&self.trials[*i]) >= 0.8 * top;
        match self.opts.objective {
            Objective::Throughput => pool.into_iter().max_by(|&a, &b| {
                self.metric(&self.trials[a])
                    .total_cmp(&self.metric(&self.trials[b]))
            }),
            Objective::Latency => pool
                .into_iter()
                .filter(near)
                .min_by(|&a, &b| self.trials[a].p99_ms.total_cmp(&self.trials[b].p99_ms)),
            Objective::Cost => pool.into_iter().filter(near).min_by(|&a, &b| {
                self.trials[a]
                    .cost_per_million_ops
                    .total_cmp(&self.trials[b].cost_per_million_ops)
            }),
        }
    }

    /// Is trial `a` meaningfully better than `b` for the objective (>3%)?
    fn better(&self, a: usize, b: usize) -> bool {
        let (ta, tb) = (&self.trials[a], &self.trials[b]);
        if ta.feasible != tb.feasible {
            return ta.feasible;
        }
        match self.opts.objective {
            Objective::Throughput => self.metric(ta) > self.metric(tb) * 1.03,
            Objective::Latency => {
                ta.p99_ms < tb.p99_ms * 0.97 && self.metric(ta) >= 0.8 * self.metric(tb)
            }
            Objective::Cost => {
                ta.cost_per_million_ops < tb.cost_per_million_ops * 0.97
                    && self.metric(ta) >= 0.8 * self.metric(tb)
            }
        }
    }

    /// Coordinate descent over one knob dimension.
    async fn descend<T: Clone + PartialEq>(
        &mut self,
        stage: &str,
        mut current: usize,
        values: &[T],
        get: impl Fn(&Knobs) -> T,
        set: impl Fn(&mut Knobs, T),
    ) -> Result<usize> {
        let base_val = get(&self.trials[current].knobs);
        let Some(pos) = values.iter().position(|v| *v == base_val) else {
            return Ok(current);
        };
        for dir in [1i64, -1] {
            let mut p = pos as i64;
            loop {
                p += dir;
                if p < 0 || p as usize >= values.len() || !self.budget_left() {
                    break;
                }
                let mut k = self.trials[current].knobs.clone();
                set(&mut k, values[p as usize].clone());
                let id = self.trial(stage, &k, None).await?;
                if self.better(id, current) {
                    current = id;
                } else {
                    break;
                }
            }
        }
        Ok(current)
    }
}

/// Highest concurrency the tuner recommends below a throttling ceiling (80 % of it).
pub fn safe_concurrency(ceiling: usize) -> usize {
    ((ceiling as f64) * 0.8).floor().max(1.0) as usize
}

/// "throttling ceiling: concurrency 48 was throttled (… 429/503 responses, trial #9); 40 is the
/// highest clean concurrency measured (first seen at 64 on the doubling ladder, narrowed by
/// bisection)".
fn ceiling_note(trials: &[Trial], base: &Knobs, ce: usize, first: Option<usize>) -> String {
    let at = |c: usize| {
        let k = Knobs {
            concurrency: c,
            ..base.clone()
        };
        trials
            .iter()
            .rev()
            .find(|t| t.stage != "confirm" && t.knobs == k)
    };
    let mut s = format!("throttling ceiling: concurrency {ce}");
    if let Some(t) = at(ce) {
        s += &format!(
            " was throttled ({} responses 429/503, trial #{})",
            t.throttled_responses, t.id
        );
    }
    let clean = trials
        .iter()
        .filter(|t| t.stage != "confirm" && !t.throttled() && t.knobs.concurrency < ce)
        .map(|t| t.knobs.concurrency)
        .max();
    if let Some(c) = clean {
        s += &format!("; {c} is the highest clean concurrency measured");
    }
    if let Some(f) = first.filter(|&f| f != ce) {
        s += &format!(" (first seen at {f} on the doubling ladder, narrowed down by bisection)");
    }
    s
}

/// Explain the safety margin with the numbers that decided it.
fn safety_note(t: &Tuner<'_>, winner: usize, ce: usize) -> String {
    let safe = safe_concurrency(ce);
    let w = &t.trials[winner];
    let mut s = format!(
        "safety margin: concurrency is capped at {safe} = 80% of the throttling ceiling {ce}, leaving {} slots of headroom for production variance",
        ce - safe
    );
    // The best clean trial the cap excluded, if it measured better than the winner.
    let excluded = (0..t.trials.len())
        .filter(|&i| {
            let x = &t.trials[i];
            x.stage != "confirm" && x.feasible && x.knobs.concurrency > safe
        })
        .max_by(|&a, &b| t.metric(&t.trials[a]).total_cmp(&t.metric(&t.trials[b])));
    if let Some(i) = excluded {
        let x = &t.trials[i];
        let (mx, mw) = (t.metric(x), t.metric(w));
        if mw > 0.0 && mx > mw {
            s += &format!(
                "; concurrency {} (trial #{}) was clean and measured {:+.0}% {} vs concurrency {} (trial #{}), but sits only {:.0}% below the ceiling, so it was not recommended",
                x.knobs.concurrency,
                x.id,
                (mx / mw - 1.0) * 100.0,
                if t.use_bytes { "MiB/s" } else { "ops/s" },
                w.knobs.concurrency,
                w.id,
                (1.0 - x.knobs.concurrency as f64 / ce as f64) * 100.0
            );
        }
    }
    s
}

/// Chunk candidates as OpenDAL would write them on a service with `cap`: each clamped to the
/// part-size limits, duplicates removed (order kept). Returns `(requested, effective)`.
fn write_chunk_candidates(
    cap: &opendal::Capability,
    chunks: &[ByteSize],
) -> (Vec<ByteSize>, Vec<ByteSize>) {
    let mut out: Vec<ByteSize> = Vec::new();
    for c in chunks {
        let e = ByteSize(crate::engine::stack::effective_write_chunk(cap, c.as_usize()) as u64);
        if !out.contains(&e) {
            out.push(e);
        }
    }
    (chunks.to_vec(), out)
}

/// How long to pause after a trial: nothing unless it was throttled; then the configured
/// cooldown or the service's `Retry-After` from *this* trial (capped at [`MAX_RETRY_AFTER`]),
/// whichever is longer. `Retry-After` is honored even with `--cooldown 0`.
fn cooldown_after(throttled: bool, cooldown: Duration, retry_after_ms: u64) -> Duration {
    if !throttled {
        return Duration::ZERO;
    }
    cooldown.max(Duration::from_millis(retry_after_ms).min(MAX_RETRY_AFTER))
}

/// Run the tuning loop on an open session.
pub async fn tune(
    session: &Session,
    opts: &TuneOptions,
    on_event: Arc<dyn Fn(TuneEvent) + Send + Sync>,
) -> Result<TuneOutcome> {
    let p = &session.profile;
    if opts.objective == Objective::Cost {
        let probe = cost_report(
            &Snapshot::default(),
            &p.cost,
            &p.target.scheme,
            p.target.endpoint().as_deref(),
        );
        if let Some(why) = probe.not_applicable_reason {
            anyhow::bail!("the cost objective needs a price table: {why}");
        }
    }
    let mut t = Tuner {
        session,
        opts,
        trials: Vec::new(),
        snapshots: Vec::new(),
        use_bytes: false,
        on_event,
        concurrency_cap: None,
    };
    let base_knobs = p.knobs.clone();
    let baseline = t.trial("baseline", &base_knobs, None).await?;
    let bs = &t.snapshots[baseline];
    t.use_bytes = bs.summary.ops > 0
        && (bs.bytes.logical_read + bs.bytes.logical_write) / bs.summary.ops.max(1) >= 64 * 1024;
    let mut notes = vec![format!(
        "objective: {} (measured as {})",
        opts.objective.label(),
        if t.use_bytes { "MiB/s" } else { "ops/s" }
    )];

    // 1. Concurrency ladder with throttling-ceiling detection.
    let max_c = opts.max_concurrency.max(1);
    let ladder: Vec<usize> = std::iter::successors(Some(1usize), |c| Some(c * 2))
        .take_while(|&c| c <= max_c)
        .collect();
    let mut ceiling: Option<usize> = if t.trials[baseline].throttled() {
        Some(base_knobs.concurrency)
    } else {
        None
    };
    let mut last_clean: Option<usize> = None;
    let mut best_metric = 0.0f64;
    let mut flat = 0;
    for &c in ladder.iter().filter(|&&c| c >= 2) {
        if !t.budget_left() || ceiling.is_some_and(|ce| c >= ce) {
            break;
        }
        let k = Knobs {
            concurrency: c,
            ..base_knobs.clone()
        };
        let id = t.trial("concurrency", &k, None).await?;
        let tr = t.trials[id].clone();
        if tr.throttled() {
            ceiling = Some(c);
            break;
        }
        last_clean = Some(c);
        if !tr.feasible {
            notes.push(format!(
                "concurrency {c} violates the SLA ({}); not going higher",
                tr.note
            ));
            break;
        }
        let m = t.metric(&tr);
        if m > best_metric * 1.05 {
            best_metric = m;
            flat = 0;
        } else {
            flat += 1;
            if flat >= 2 {
                notes.push(format!("throughput plateaued at concurrency ≈{}", c / 4));
                break;
            }
        }
    }
    let first_throttled = ceiling;
    // Binary refinement just below the throttling ceiling.
    if let (Some(hi0), Some(lo0)) = (ceiling, last_clean) {
        let (mut lo, mut hi) = (lo0, hi0);
        for _ in 0..3 {
            if hi - lo < 2 || !t.budget_left() {
                break;
            }
            let mid = (lo + hi) / 2;
            let id = t
                .trial(
                    "ceiling",
                    &Knobs {
                        concurrency: mid,
                        ..base_knobs.clone()
                    },
                    None,
                )
                .await?;
            if t.trials[id].throttled() {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        ceiling = Some(hi);
    }
    // One statement about the ceiling, consistent with `throttle_ceiling` in the summary.
    if let Some(ce) = ceiling {
        notes.push(ceiling_note(&t.trials, &base_knobs, ce, first_throttled));
    }
    let mut current = t.best().unwrap_or(baseline);
    if let Some(ce) = ceiling {
        let safe = safe_concurrency(ce);
        t.concurrency_cap = Some(safe);
        if t.trials[current].knobs.concurrency > safe {
            if t.budget_left() {
                let k = Knobs {
                    concurrency: safe,
                    ..t.trials[current].knobs.clone()
                };
                current = t.trial("safety-margin", &k, None).await?;
            } else if let Some(b) = t.best() {
                current = b;
            }
        }
    }

    // 2. Workload-relevant knobs.
    let mix = &session.mix;
    let has = |f: &dyn Fn(&OpKind) -> bool| mix.iter().any(|m| m.weight > 0 && f(&m.op));
    let big_objects = session
        .dataset
        .objects
        .iter()
        .map(|o| o.size)
        .max()
        .unwrap_or(0)
        >= 8 << 20;
    let multipart = has(&|o| matches!(o, OpKind::MultipartWrite { .. }));
    let vectored = has(&|o| matches!(o, OpKind::ParquetScan { .. }));
    let full_reads = has(&|o| matches!(o, OpKind::Read { size: None }));
    let reads = has(&|o| !o.is_write());
    let mut chunks = vec![
        ByteSize::mib(1),
        ByteSize::mib(4),
        ByteSize::mib(8),
        ByteSize::mib(16),
        ByteSize::mib(32),
    ];
    if multipart {
        // OpenDAL silently raises (lowers) write chunks to the service's minimum (maximum)
        // multipart part size: only try chunk sizes it actually uses.
        let (requested, effective) = write_chunk_candidates(&session.capability, &chunks);
        if effective != requested {
            notes.push(format!(
                "chunk sizes tried: {} — {} would be raised to the service's minimum part size {} by OpenDAL",
                effective.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", "),
                requested
                    .iter()
                    .filter(|c| !effective.contains(c))
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                session
                    .capability
                    .write_multi_min_size
                    .map(|m| ByteSize(m as u64).to_string())
                    .unwrap_or_default()
            ));
        }
        chunks = effective;
    }
    let ios = [1usize, 2, 4, 8, 16];
    let gaps = [
        ByteSize::b(0),
        ByteSize::kib(16),
        ByteSize::kib(64),
        ByteSize::kib(256),
        ByteSize::mib(1),
    ];

    let snap_dims = |k: &mut Knobs,
                     list: &[ByteSize],
                     f: fn(&Knobs) -> ByteSize,
                     s: fn(&mut Knobs, ByteSize)| {
        if !list.contains(&f(k)) {
            let v = f(k);
            let nearest = *list
                .iter()
                .min_by_key(|x| x.0.abs_diff(v.0))
                .expect("non-empty");
            s(k, nearest);
        }
    };
    if multipart || (full_reads && big_objects) {
        let mut k = t.trials[current].knobs.clone();
        snap_dims(&mut k, &chunks, |k| k.chunk_size, |k, v| k.chunk_size = v);
        if !ios.contains(&k.io_concurrency) {
            k.io_concurrency = 4;
        }
        if k != t.trials[current].knobs && t.budget_left() {
            let id = t.trial("normalize", &k, None).await?;
            if !t.better(current, id) {
                current = id;
            }
        }
        current = t
            .descend(
                "chunk_size",
                current,
                &chunks,
                |k| k.chunk_size,
                |k, v| k.chunk_size = v,
            )
            .await?;
        current = t
            .descend(
                "io_concurrency",
                current,
                &ios,
                |k| k.io_concurrency,
                |k, v| k.io_concurrency = v,
            )
            .await?;
    }
    if vectored {
        let mut k = t.trials[current].knobs.clone();
        snap_dims(&mut k, &gaps, |k| k.range_gap, |k, v| k.range_gap = v);
        if k != t.trials[current].knobs && t.budget_left() {
            let id = t.trial("normalize", &k, None).await?;
            if !t.better(current, id) {
                current = id;
            }
        }
        current = t
            .descend(
                "range_gap",
                current,
                &gaps,
                |k| k.range_gap,
                |k, v| k.range_gap = v,
            )
            .await?;
        if !multipart {
            current = t
                .descend(
                    "io_concurrency",
                    current,
                    &ios,
                    |k| k.io_concurrency,
                    |k, v| k.io_concurrency = v,
                )
                .await?;
        }
    }
    // The read cache. A generated dataset is a small stand-in for the real data: under uniform
    // access it fitting into the cache says nothing about production, so the cache is only
    // tried for skewed access (locality) or for the user's real objects (`existing_prefix`),
    // and a win that relies on the whole test dataset fitting is flagged.
    let skewed = !matches!(p.workload.access, Access::Uniform);
    let real_data = session.dataset.read_only;
    let working_set: u64 = session.dataset.objects.iter().map(|o| o.size).sum();
    let fits = working_set <= t.trials[current].knobs.cache_size.0;
    if reads && t.budget_left() && (skewed || (real_data && fits)) {
        let mut k = t.trials[current].knobs.clone();
        k.cache = !k.cache;
        let id = t.trial("cache", &k, None).await?;
        if t.better(id, current) {
            current = id;
            if t.trials[id].knobs.cache && fits && !real_data {
                notes.push(format!(
                    "the read cache won, but the whole {} test dataset fits in the {} cache: the hit ratio depends on your real working set (dataset-size-dependent; verify with `existing_prefix` or a production-sized dataset)",
                    crate::util::fmt_bytes(working_set),
                    t.trials[id].knobs.cache_size
                ));
            }
        }
    } else if reads && !skewed && fits && !real_data && !t.trials[current].knobs.cache {
        notes.push(
            "read cache not tried: under uniform access its benefit depends on the real working set, not the generated test dataset".into(),
        );
    }
    if p.target.endpoint().is_some_and(|e| e.starts_with("https")) && t.budget_left() {
        // Compare the two protocols the measured client can actually speak: OpenDAL's reqwest
        // client is HTTP/1.1 unless h2 is requested (`Auto` == `H1` there); the microscope pool
        // negotiates h2 by ALPN under `Auto`.
        let mut k = t.trials[current].knobs.clone();
        k.http_version = match (k.transport, k.http_version) {
            (TransportMode::Opendal, HttpVersion::H2) => HttpVersion::Auto,
            (TransportMode::Opendal, _) => HttpVersion::H2,
            (TransportMode::Microscope, HttpVersion::H1) => HttpVersion::Auto,
            (TransportMode::Microscope, _) => HttpVersion::H1,
        };
        let id = t.trial("http_version", &k, None).await?;
        if t.better(id, current) {
            current = id;
        }
    }

    // 3. Confirm the winner with a longer window.
    let mut winner_idx = t
        .best()
        .map(|b| if t.better(b, current) { b } else { current })
        .unwrap_or(current);
    let no_feasible = !t.any_feasible();
    if !no_feasible && !t.trials[winner_idx].feasible {
        // Never recommend an SLA-violating configuration while one met the SLA.
        winner_idx = t.best().unwrap_or(winner_idx);
    }
    if no_feasible {
        let msg = format!(
            "no trial met the SLA; recommending the least-bad configuration (trial #{}: {})",
            winner_idx, t.trials[winner_idx].note
        );
        (t.on_event)(TuneEvent::Message(msg.clone()));
        notes.insert(0, msg);
    }
    if let Some(ce) = ceiling {
        notes.push(safety_note(&t, winner_idx, ce));
    }
    let winner = t.trials[winner_idx].knobs.clone();
    let confirm_opts = RunOptions {
        duration: opts.trial.duration * 2,
        ..opts.trial.clone()
    };
    let confirm = if !session.cancel.is_cancelled() {
        t.trial("confirm", &winner, Some(&confirm_opts)).await?
    } else {
        winner_idx
    };
    if !t.trials[baseline].feasible {
        notes.push(format!(
            "baseline configuration violated the SLA ({}); the winner is compared against it",
            t.trials[baseline].note
        ));
    }
    if !t.trials[confirm].feasible {
        notes.push(format!(
            "confirmation run was not clean ({}); treat the result with care",
            t.trials[confirm].note
        ));
    }
    let base_m = t.metric(&t.trials[baseline]);
    let improvement = if base_m > 0.0 {
        t.metric(&t.trials[confirm]) / base_m - 1.0
    } else {
        0.0
    };
    let mut winner_snapshot = t.snapshots[confirm].clone();
    if session.cancel.is_cancelled() {
        winner_snapshot.interrupted = true;
    }
    Ok(TuneOutcome {
        summary: TuneSummary {
            objective: opts.objective.label().into(),
            trials: t.trials,
            baseline_trial: baseline,
            winner_trial: confirm,
            improvement,
            throttle_ceiling: ceiling,
            no_feasible_trial: no_feasible,
            notes,
        },
        winner,
        winner_snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_chunk_candidates_respect_the_minimum_part_size() {
        let mut cap = opendal::Capability::default();
        let list = [
            ByteSize::mib(1),
            ByteSize::mib(4),
            ByteSize::mib(8),
            ByteSize::mib(16),
        ];
        assert_eq!(write_chunk_candidates(&cap, &list).1, list.to_vec());
        cap.write_multi_min_size = Some(5 << 20);
        cap.write_multi_max_size = Some(10 << 20);
        assert_eq!(
            write_chunk_candidates(&cap, &list).1,
            vec![ByteSize::mib(5), ByteSize::mib(8), ByteSize::mib(10)]
        );
    }

    #[test]
    fn cooldown_honors_capped_retry_after_per_trial() {
        let s = Duration::from_secs;
        assert_eq!(cooldown_after(false, s(5), 30_000), Duration::ZERO);
        assert_eq!(cooldown_after(true, Duration::ZERO, 2_000), s(2));
        assert_eq!(cooldown_after(true, s(3), 1_000), s(3));
        assert_eq!(
            cooldown_after(true, Duration::ZERO, 3_600_000),
            MAX_RETRY_AFTER
        );
        assert_eq!(cooldown_after(true, Duration::ZERO, 0), Duration::ZERO);
    }
}
