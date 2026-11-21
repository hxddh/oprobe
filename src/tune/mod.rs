//! Closed-loop tuner (design v4 §6): measures real trials against the target and finds the
//! best configuration within a throttle budget, using the capacity model.
//!
//! - **Objective:** `goodput` by default — successful ops/s (or MiB/s for large objects)
//!   while the share of throttled requests stays within `max_throttle_ratio` (default
//!   0.001); also `p99`, `cost` and `balanced`. Throttling within the budget is acceptable:
//!   the ceiling search does not stop at the first 503.
//! - **Search:** a concurrency ladder, then bisection between the highest feasible and the
//!   lowest infeasible concurrency, each boundary trial repeated (`boundary_repeats`). Every
//!   trial's request rows feed one pooled [`Capacity`] estimate (in-flight ceiling); the
//!   margin is [`Capacity::safe_concurrency`], counted in requests in flight, so it is
//!   re-checked whenever `io_concurrency` or `range_gap` change the requests one operation
//!   keeps in flight.
//! - **Honest deltas:** an infeasible baseline is flagged (`baseline_feasible: false`); deltas
//!   are then taken against the best feasible trial, and the headline never reports a
//!   negative "improvement". The cost of the safety margin (best measured vs recommended)
//!   and the Pareto front of the trials are reported.

pub mod codegen;
pub mod recommend;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::analysis::capacity::{self, Capacity, Window};
use crate::analysis::cost::cost_report;
use crate::config::{Access, Knobs, OpKind, TransportMode};
use crate::engine::{RunOptions, Session};
use crate::model::{OpRow, ReqRow};
use crate::net::HttpVersion;
use crate::report::TuneSummary;
use crate::trace::Snapshot;
use crate::util::ByteSize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Objective {
    /// Maximize goodput (successful ops/s or MiB/s) within the SLA and the throttle budget.
    #[default]
    Throughput,
    /// Minimize worst p99 while keeping ≥ 80% of the best goodput.
    Latency,
    /// Minimize cost per operation while keeping ≥ 80% of the best goodput.
    Cost,
    /// Goodput and p99 together: the best goodput-to-latency trade among trials within 80 %
    /// of the best goodput.
    Balanced,
}

impl Objective {
    pub fn label(&self) -> &'static str {
        match self {
            Objective::Throughput => "goodput",
            Objective::Latency => "p99",
            Objective::Cost => "cost",
            Objective::Balanced => "balanced",
        }
    }

    /// Parse an objective label (`goodput`/`throughput`, `p99`/`latency`, `cost`,
    /// `balanced`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "goodput" | "throughput" => Some(Objective::Throughput),
            "p99" | "latency" => Some(Objective::Latency),
            "cost" => Some(Objective::Cost),
            "balanced" => Some(Objective::Balanced),
            _ => None,
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
    /// Share of throttled requests a feasible configuration may cause.
    pub max_throttle_ratio: f64,
    /// Runs of each bisection trial near the throttle boundary.
    pub boundary_repeats: usize,
    /// In-flight headroom kept below the ceiling ([`capacity::DEFAULT_HEADROOM_RATIO`]).
    pub headroom_ratio: f64,
    /// A remembered capacity of this target and workload (`<data>/capacity.toml`).
    pub prior: Option<Capacity>,
}

impl TuneOptions {
    /// Options with the built-in defaults ([`crate::config::defaults`]) for everything but
    /// the objective and the trial.
    pub fn new(objective: Objective, trial: RunOptions) -> Self {
        let d = crate::config::defaults();
        Self {
            objective,
            trial,
            max_concurrency: d.tune.max_concurrency,
            max_trials: d.tune.max_trials,
            cooldown: d.tune.cooldown.get(),
            max_throttle_ratio: d.tune.max_throttle_ratio,
            boundary_repeats: d.tune.boundary_repeats,
            headroom_ratio: d.headroom_ratio,
            prior: None,
        }
    }
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
    /// Throttled responses / requests (the throttle budget applies to this).
    pub throttled_request_ratio: f64,
    /// Operations with at least one throttled response / operations.
    pub throttled_op_ratio: f64,
    pub throttled_responses: u64,
    pub requests: u64,
    pub cost_per_million_ops: f64,
    /// Requests in flight at the trial's peak.
    pub peak_inflight: u64,
    /// Requests one operation keeps in flight at its peak (p95 over operations).
    pub peak_inflight_per_op: f64,
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

/// Time offset between pooled trials (µs): far larger than any trial, so in-flight levels of
/// different trials never overlap in the pooled estimate.
const POOL_OFFSET_US: i64 = 10_000_000_000;
/// Most request rows kept per trial for the pooled estimate.
const POOL_MAX_PER_TRIAL: usize = 200_000;

struct Tuner<'a> {
    session: &'a Session,
    opts: &'a TuneOptions,
    trials: Vec<Trial>,
    snapshots: Vec<Snapshot>,
    use_bytes: bool,
    on_event: Arc<dyn Fn(TuneEvent) + Send + Sync>,
    /// Request and op rows of every trial, shifted apart in time.
    pool_reqs: Vec<ReqRow>,
    pool_ops: Vec<OpRow>,
    /// The in-flight ceiling (requests), once known.
    ceiling: Option<capacity::Ceiling>,
}

impl Tuner<'_> {
    fn metric(&self, t: &Trial) -> f64 {
        if self.use_bytes {
            t.mib_per_s
        } else {
            t.ops_per_s
        }
    }

    async fn trial(&mut self, stage: &str, knobs: &Knobs) -> Result<usize> {
        if let Some(i) = self
            .trials
            .iter()
            .position(|t| &t.knobs == knobs && t.stage != "confirm")
        {
            return Ok(i);
        }
        self.measure(stage, knobs, None).await
    }

    /// Measure `knobs` (always a new trial).
    async fn measure(
        &mut self,
        stage: &str,
        knobs: &Knobs,
        opts: Option<&RunOptions>,
    ) -> Result<usize> {
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
        let rows = self.session.take_trace().unwrap_or_default();
        let cap = capacity::estimate(&rows.reqs, &rows.ops, Window::measured(snap.elapsed_s));
        let offset = id as i64 * POOL_OFFSET_US;
        for q in rows
            .reqs
            .iter()
            .filter(|q| q.t_us >= 0)
            .take(POOL_MAX_PER_TRIAL)
        {
            let mut q = q.clone();
            q.t_us += offset;
            self.pool_reqs.push(q);
        }
        for o in rows
            .ops
            .iter()
            .filter(|o| o.t_us >= 0)
            .take(POOL_MAX_PER_TRIAL)
        {
            let mut o = o.clone();
            o.t_us += offset;
            self.pool_ops.push(o);
        }
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
        let requests = snap.connections.requests;
        let throttled = snap.resilience.throttled_responses;
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
            throttled_request_ratio: crate::util::ratio(throttled as f64, requests as f64),
            throttled_op_ratio: throttled_ops as f64 / ops,
            throttled_responses: throttled,
            requests,
            cost_per_million_ops: cost.selected.per_million_ops_usd,
            peak_inflight: cap.observed.peak_inflight_count,
            peak_inflight_per_op: cap.per_op.peak_inflight_per_op.max(1.0),
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
        if t.throttled_request_ratio > self.opts.max_throttle_ratio {
            why.push(format!(
                "throttled {:.2}% of requests > budget {:.2}%",
                t.throttled_request_ratio * 100.0,
                self.opts.max_throttle_ratio * 100.0
            ));
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

    /// The pooled capacity estimate over every trial so far.
    fn pooled(&self) -> Capacity {
        let mut c = capacity::estimate(&self.pool_reqs, &self.pool_ops, Window::all());
        c.source = capacity::CapacitySource::Tune;
        c
    }

    /// Workers that keep a trial's peak in-flight under the ceiling with headroom, for its
    /// request shape (`None` without a ceiling).
    fn safe_for(&self, peak_per_op: f64) -> Option<usize> {
        let ce = self.ceiling?;
        let h = self.opts.headroom_ratio.clamp(0.0, 0.95);
        Some(((ce.lo * (1.0 - h) / peak_per_op.max(1.0)).floor() as usize).max(1))
    }

    /// Within the safety margin: its peak in-flight (concurrency × its per-op peak) stays
    /// under the ceiling with headroom.
    fn within_margin(&self, i: usize) -> bool {
        let t = &self.trials[i];
        self.safe_for(t.peak_inflight_per_op)
            .is_none_or(|s| t.knobs.concurrency <= s)
    }

    /// Trials that may win: not a confirmation run, and within the safety margin.
    fn eligible(&self, i: usize) -> bool {
        self.trials[i].stage != "confirm" && self.within_margin(i)
    }

    fn any_feasible(&self) -> bool {
        self.trials
            .iter()
            .any(|t| t.feasible && t.stage != "confirm")
    }

    /// Candidate pool, in order of preference: feasible trials within the margin; any
    /// feasible trial; and only when no trial met the SLA, the least-bad ones (least
    /// throttled, then the lowest error rate).
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
        let min_thr = rest
            .iter()
            .map(|&i| self.trials[i].throttled_request_ratio)
            .fold(f64::INFINITY, f64::min);
        rest.retain(|&i| self.trials[i].throttled_request_ratio <= min_thr + 1e-3);
        let min_err = rest
            .iter()
            .map(|&i| self.trials[i].error_rate)
            .fold(f64::INFINITY, f64::min);
        // Within 0.1 percentage points of the lowest error rate counts as equally clean.
        rest.retain(|&i| self.trials[i].error_rate <= min_err + 0.001);
        rest
    }

    /// The best of `pool` for the objective.
    fn best_of(&self, pool: Vec<usize>) -> Option<usize> {
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
            Objective::Balanced => {
                let pool: Vec<usize> = pool.into_iter().filter(near).collect();
                let min_p99 = pool
                    .iter()
                    .map(|&i| self.trials[i].p99_ms)
                    .fold(f64::INFINITY, f64::min)
                    .max(1e-6);
                let score = |i: usize| {
                    let t = &self.trials[i];
                    self.metric(t) / top.max(1e-9) - 0.5 * (t.p99_ms / min_p99 - 1.0)
                };
                pool.into_iter()
                    .max_by(|&a, &b| score(a).total_cmp(&score(b)))
            }
        }
    }

    /// Index of the best trial for the objective. Never an SLA-violating trial while any trial
    /// met the SLA.
    fn best(&self) -> Option<usize> {
        self.best_of(self.pool())
    }

    /// The best feasible trial regardless of the margin (what the margin is measured against).
    fn best_feasible(&self) -> Option<usize> {
        let pool: Vec<usize> = (0..self.trials.len())
            .filter(|&i| self.trials[i].stage != "confirm" && self.trials[i].feasible)
            .collect();
        if pool.is_empty() {
            None
        } else {
            self.best_of(pool)
        }
    }

    /// Is trial `a` meaningfully better than `b` for the objective (>3%)?
    fn better(&self, a: usize, b: usize) -> bool {
        let (ta, tb) = (&self.trials[a], &self.trials[b]);
        if ta.feasible != tb.feasible {
            return ta.feasible;
        }
        let (ea, eb) = (self.within_margin(a), self.within_margin(b));
        if ea != eb {
            return ea;
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
            Objective::Balanced => {
                let s = |t: &Trial| self.metric(t) / t.p99_ms.max(1e-6);
                s(ta) > s(tb) * 1.03 && self.metric(ta) >= 0.8 * self.metric(tb)
            }
        }
    }

    /// Coordinate descent over one knob dimension. Every accepted step that changes the
    /// requests one operation keeps in flight re-checks the margin.
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
                let id = self.trial(stage, &k).await?;
                let id = self.recheck_margin(id).await?;
                if self.better(id, current) {
                    current = id;
                } else {
                    break;
                }
            }
        }
        Ok(current)
    }

    /// When trial `id` keeps more requests in flight per operation than the margin allows at
    /// its concurrency, measure it again at the concurrency that fits ("margin-recheck").
    async fn recheck_margin(&mut self, id: usize) -> Result<usize> {
        let t = &self.trials[id];
        let Some(safe) = self.safe_for(t.peak_inflight_per_op) else {
            return Ok(id);
        };
        if t.knobs.concurrency <= safe || !self.budget_left() {
            return Ok(id);
        }
        let mut k = t.knobs.clone();
        let msg = format!(
            "margin re-check: {} peaks at {:.0} requests in flight per op, so concurrency {} exceeds the ceiling's margin; trying {safe}",
            k.summary(),
            t.peak_inflight_per_op,
            k.concurrency
        );
        (self.on_event)(TuneEvent::Message(msg));
        k.concurrency = safe;
        self.trial("margin-recheck", &k).await
    }
}

/// The trials that are not dominated on (goodput, p99, cost, throttled ratio).
pub fn pareto_front(trials: &[Trial], use_bytes: bool) -> Vec<usize> {
    let m = |t: &Trial| if use_bytes { t.mib_per_s } else { t.ops_per_s };
    let cand: Vec<&Trial> = trials.iter().filter(|t| t.stage != "confirm").collect();
    let dominates = |a: &Trial, b: &Trial| {
        let ge = m(a) >= m(b)
            && a.p99_ms <= b.p99_ms
            && a.cost_per_million_ops <= b.cost_per_million_ops
            && a.throttled_request_ratio <= b.throttled_request_ratio;
        let gt = m(a) > m(b)
            || a.p99_ms < b.p99_ms
            || a.cost_per_million_ops < b.cost_per_million_ops
            || a.throttled_request_ratio < b.throttled_request_ratio;
        ge && gt
    };
    cand.iter()
        .filter(|b| !cand.iter().any(|a| dominates(a, b)))
        .map(|t| t.id)
        .collect()
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

/// The objective's value of a trial, signed so that higher is better.
fn score(o: Objective, use_bytes: bool, t: &Trial) -> f64 {
    let m = if use_bytes { t.mib_per_s } else { t.ops_per_s };
    match o {
        Objective::Throughput | Objective::Balanced => m,
        Objective::Latency => -t.p99_ms,
        Objective::Cost => -t.cost_per_million_ops,
    }
}

/// Relative improvement of `b` over `a` for the objective (positive = better).
fn improvement(o: Objective, use_bytes: bool, a: &Trial, b: &Trial) -> f64 {
    let (x, y) = (score(o, use_bytes, a), score(o, use_bytes, b));
    if x.abs() < 1e-12 {
        0.0
    } else {
        (y - x) / x.abs()
    }
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
        pool_reqs: Vec::new(),
        pool_ops: Vec::new(),
        ceiling: None,
    };
    let base_knobs = p.knobs.clone();
    let baseline = t.trial("baseline", &base_knobs).await?;
    let bs = &t.snapshots[baseline];
    t.use_bytes = bs.summary.ops > 0
        && (bs.bytes.logical_read + bs.bytes.logical_write) / bs.summary.ops.max(1) >= 64 * 1024;
    let unit = if t.use_bytes { "MiB/s" } else { "ops/s" };
    let mut notes = vec![format!(
        "objective: {} (measured as {unit}) within a throttle budget of {:.2}% of requests",
        opts.objective.label(),
        opts.max_throttle_ratio * 100.0
    )];
    if let Some(prior) = &opts.prior
        && let Some(ce) = prior.inflight_ceiling
    {
        notes.push(format!(
            "history: this target and workload throttled above ≈{:.0} requests in flight before",
            ce.lo
        ));
    }

    // 1. Concurrency ladder: up while feasible (throttling within the budget is fine).
    let max_c = opts.max_concurrency.max(1);
    let ladder: Vec<usize> = std::iter::successors(Some(1usize), |c| Some(c * 2))
        .take_while(|&c| c <= max_c)
        .collect();
    let throttle_bound = |tr: &Trial| tr.throttled_request_ratio > opts.max_throttle_ratio;
    let mut infeasible_at: Option<usize> = (!t.trials[baseline].feasible
        && throttle_bound(&t.trials[baseline]))
    .then_some(base_knobs.concurrency);
    let mut last_feasible: Option<usize> = t.trials[baseline]
        .feasible
        .then_some(base_knobs.concurrency);
    let mut best_metric = 0.0f64;
    let mut flat = 0;
    for &c in ladder.iter().filter(|&&c| c >= 2) {
        if !t.budget_left() || infeasible_at.is_some_and(|ce| c >= ce) {
            break;
        }
        let k = Knobs {
            concurrency: c,
            ..base_knobs.clone()
        };
        let id = t.trial("concurrency", &k).await?;
        let tr = t.trials[id].clone();
        if throttle_bound(&tr) {
            infeasible_at = Some(c);
            break;
        }
        if !tr.feasible {
            notes.push(format!(
                "concurrency {c} violates the SLA ({}); not going higher",
                tr.note
            ));
            break;
        }
        last_feasible = Some(last_feasible.map_or(c, |l| l.max(c)));
        let m = t.metric(&tr);
        if m > best_metric * 1.05 {
            best_metric = m;
            flat = 0;
        } else {
            flat += 1;
            if flat >= 2 {
                notes.push(format!("goodput plateaued at concurrency ≈{}", c / 4));
                break;
            }
        }
    }
    let first_infeasible = infeasible_at;
    // 2. Bisection between the highest feasible and the lowest throttle-infeasible
    // concurrency; each boundary trial is repeated and judged on its mean throttled ratio.
    if let (Some(hi0), Some(lo0)) = (
        infeasible_at,
        last_feasible.filter(|l| Some(*l) < infeasible_at),
    ) {
        let (mut lo, mut hi) = (lo0, hi0);
        for _ in 0..4 {
            if hi - lo < 2 || !t.budget_left() {
                break;
            }
            let mid = (lo + hi) / 2;
            let k = Knobs {
                concurrency: mid,
                ..base_knobs.clone()
            };
            let mut ids = vec![t.trial("boundary", &k).await?];
            for _ in 1..opts.boundary_repeats.max(1) {
                if !t.budget_left() {
                    break;
                }
                ids.push(t.measure("boundary", &k, None).await?);
            }
            let (thr, req): (u64, u64) = ids.iter().fold((0, 0), |(a, b), &i| {
                (
                    a + t.trials[i].throttled_responses,
                    b + t.trials[i].requests,
                )
            });
            let mean_ratio = crate::util::ratio(thr as f64, req as f64);
            let all_feasible = ids.iter().all(|&i| t.trials[i].feasible);
            if mean_ratio > opts.max_throttle_ratio {
                hi = mid;
            } else {
                lo = mid;
            }
            // A repeat that disagrees marks every run of this level infeasible.
            if mean_ratio > opts.max_throttle_ratio || !all_feasible {
                for &i in &ids {
                    if t.trials[i].feasible && mean_ratio > opts.max_throttle_ratio {
                        t.trials[i].feasible = false;
                        t.trials[i].note = format!(
                            "throttled {:.2}% of requests over {} repeats > budget",
                            mean_ratio * 100.0,
                            ids.len()
                        );
                    }
                }
            }
        }
        infeasible_at = Some(hi);
        last_feasible = Some(lo);
    }
    // 3. The capacity model over every trial: the in-flight ceiling and the margin.
    let mut cap = t.pooled();
    if cap.inflight_ceiling.is_none()
        && let Some(prior) = &opts.prior
        && prior.inflight_ceiling.is_some()
        && infeasible_at.is_some()
    {
        cap.inflight_ceiling = prior.inflight_ceiling;
        notes.push("the in-flight ceiling comes from the target's history".into());
    }
    t.ceiling = cap.inflight_ceiling;
    let base_shape = t.trials[baseline].peak_inflight_per_op;
    let mut safe = t.safe_for(base_shape);
    if let (Some(ce), Some(s)) = (t.ceiling, safe) {
        notes.push(format!(
            "throttling ceiling: {:.0}–{:.0} requests in flight ({} confidence); at {:.0} requests in flight per op the margin of {:.0}% allows concurrency {s}",
            ce.lo,
            ce.hi,
            match cap.confidence {
                capacity::Confidence::Low => "low",
                capacity::Confidence::Medium => "medium",
                capacity::Confidence::High => "high",
            },
            base_shape,
            opts.headroom_ratio * 100.0
        ));
        // The margin never exceeds what was measured feasible.
        if let Some(hi) = infeasible_at
            && s >= hi
        {
            safe = Some(hi.saturating_sub(1).max(1));
        }
    } else if let Some(hi) = infeasible_at {
        // Throttling that does not follow in-flight requests (a rate limit): stay at the
        // highest feasible concurrency measured.
        let fallback = last_feasible.unwrap_or(1).min(hi.saturating_sub(1)).max(1);
        notes.push(format!(
            "throttled above the budget from concurrency {hi}, but not at a clear in-flight level ({}); staying at concurrency {fallback}",
            cap.confidence_reason
        ));
        safe = Some(fallback);
    }
    if let Some(f) = first_infeasible.filter(|f| Some(*f) != infeasible_at)
        && let Some(hi) = infeasible_at
    {
        notes.push(format!(
            "over budget from concurrency {hi} (first seen at {f} on the doubling ladder, narrowed down by bisection with {} repeats per step)",
            opts.boundary_repeats.max(1)
        ));
    }
    let mut current = t.best().unwrap_or(baseline);
    if let Some(s) = safe {
        let at_safe = Knobs {
            concurrency: s,
            ..base_knobs.clone()
        };
        let cur_c = t.trials[current].knobs.concurrency;
        // For goodput the margin's concurrency is the natural answer: measure it unless a
        // measured trial at it exists.
        let measure_safe = cur_c != s
            && (cur_c > s || matches!(opts.objective, Objective::Throughput | Objective::Balanced));
        if measure_safe && t.budget_left() {
            let id = t.trial("safety-margin", &at_safe).await?;
            if t.trials[id].feasible
                && (t.trials[current].knobs.concurrency > s
                    || !t.within_margin(current)
                    || !t.better(current, id))
            {
                current = id;
            }
        } else if cur_c > s
            && let Some(b) = t.best()
        {
            current = b;
        }
    }

    // 4. Workload-relevant knobs.
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
    let multipart =
        has(&|o| matches!(o, OpKind::MultipartWrite { .. })) && session.capability.write_can_multi;
    if has(&|o| matches!(o, OpKind::MultipartWrite { .. })) && !multipart {
        notes.push(format!(
            "`{}` has no multipart uploads: write chunk sizes are not tuned (each object is one request)",
            session.profile.target.scheme
        ));
    }
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
            let id = t.trial("normalize", &k).await?;
            let id = t.recheck_margin(id).await?;
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
            let id = t.trial("normalize", &k).await?;
            let id = t.recheck_margin(id).await?;
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
    // The read cache: only for skewed access or the user's real objects (a generated dataset
    // fitting the cache says nothing about production).
    let skewed = !matches!(p.workload.access, Access::Uniform);
    let real_data = session.dataset.read_only;
    let working_set: u64 = session.dataset.objects.iter().map(|o| o.size).sum();
    let fits = working_set <= t.trials[current].knobs.cache_size.0;
    if reads && t.budget_left() && (skewed || (real_data && fits)) {
        let mut k = t.trials[current].knobs.clone();
        k.cache = !k.cache;
        let id = t.trial("cache", &k).await?;
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
        let mut k = t.trials[current].knobs.clone();
        k.http_version = match (k.transport, k.http_version) {
            (TransportMode::Opendal, HttpVersion::H2) => HttpVersion::Auto,
            (TransportMode::Opendal, _) => HttpVersion::H2,
            (TransportMode::Microscope, HttpVersion::H1) => HttpVersion::Auto,
            (TransportMode::Microscope, _) => HttpVersion::H1,
        };
        let id = t.trial("http_version", &k).await?;
        if t.better(id, current) {
            current = id;
        }
    }

    // 5. The winner: the best eligible trial; confirmed with a longer window.
    let mut winner_idx = t
        .best()
        .map(|b| if t.better(b, current) { b } else { current })
        .unwrap_or(current);
    let no_feasible = !t.any_feasible();
    if !no_feasible && (!t.trials[winner_idx].feasible || !t.eligible(winner_idx)) {
        winner_idx = t.best().unwrap_or(winner_idx);
    }
    if no_feasible {
        let msg = format!(
            "no trial met the SLA and the throttle budget; this is the least-bad configuration (trial #{}: {})",
            winner_idx, t.trials[winner_idx].note
        );
        (t.on_event)(TuneEvent::Message(msg.clone()));
        notes.insert(0, msg);
    }
    let winner = t.trials[winner_idx].knobs.clone();
    let confirm_opts = RunOptions {
        duration: opts.trial.duration * 2,
        ..opts.trial.clone()
    };
    let confirm = if !session.cancel.is_cancelled() {
        t.measure("confirm", &winner, Some(&confirm_opts)).await?
    } else {
        winner_idx
    };
    if !t.trials[confirm].feasible {
        notes.push(format!(
            "confirmation run was not clean ({}); treat the result with care",
            t.trials[confirm].note
        ));
    }
    // The final capacity estimate includes every trial.
    let mut cap = t.pooled();
    if cap.inflight_ceiling.is_none() {
        cap.inflight_ceiling = t.ceiling;
    }
    let o = opts.objective;
    let (wt, bt) = (&t.trials[winner_idx], &t.trials[baseline]);
    let baseline_feasible = bt.feasible;
    let best_feasible = t.best_feasible();
    let change = {
        let o = recommend::knob_overrides(&base_knobs, &wt.knobs);
        if o.is_empty() {
            "the baseline settings".to_owned()
        } else {
            o.summary()
        }
    };
    // Deltas: against the baseline when it is feasible and within the margin, else against
    // the best feasible trial; never a negative headline.
    let (improvement_ratio, headline) = if no_feasible {
        (
            None,
            format!(
                "no feasible configuration: least-bad is {} ({})",
                change.clone(),
                wt.note
            ),
        )
    } else if !baseline_feasible {
        (
            None,
            format!(
                "baseline infeasible ({:.1}% of requests throttled{}); recommended {} at {:.1} {unit}",
                bt.throttled_request_ratio * 100.0,
                if bt.error_rate > p.sla.max_error_rate {
                    format!(", {:.1}% errors", bt.error_rate * 100.0)
                } else {
                    String::new()
                },
                change.clone(),
                t.metric(wt)
            ),
        )
    } else {
        let imp = improvement(o, t.use_bytes, bt, wt);
        if imp >= 0.0 {
            (
                Some(imp),
                format!(
                    "{} → {:+.0}% {} vs baseline",
                    change.clone(),
                    imp * 100.0,
                    o.label()
                ),
            )
        } else {
            (
                None,
                format!(
                    "baseline exceeds the safety margin; recommended {} keeps {:.0}% headroom below the throttling ceiling (costs {:.0}% {})",
                    change.clone(),
                    opts.headroom_ratio * 100.0,
                    -imp * 100.0,
                    o.label()
                ),
            )
        }
    };
    let margin_cost_ratio = best_feasible
        .filter(|&b| b != winner_idx)
        .map(|b| improvement(o, t.use_bytes, wt, &t.trials[b]))
        .filter(|d| *d > 0.0);
    if let (Some(b), Some(d)) = (best_feasible, margin_cost_ratio) {
        let bf = &t.trials[b];
        notes.push(format!(
            "cost of the margin: trial #{} ({}) measured {:.1} {unit} within the throttle budget, {:.0}% more than the recommended {:.1} {unit}; it sits closer to the ceiling",
            bf.id,
            bf.knobs.summary(),
            t.metric(bf),
            d * 100.0,
            t.metric(wt)
        ));
    }
    let pareto = pareto_front(&t.trials, t.use_bytes);
    let mut winner_snapshot = t.snapshots[confirm].clone();
    if session.cancel.is_cancelled() {
        winner_snapshot.interrupted = true;
    }
    Ok(TuneOutcome {
        summary: TuneSummary {
            objective: o.label().into(),
            metric_unit: if t.use_bytes {
                "mib_per_s"
            } else {
                "ops_per_s"
            }
            .into(),
            trials: t.trials,
            baseline_trial: baseline,
            recommended_trial: winner_idx,
            winner_trial: confirm,
            best_feasible_trial: best_feasible,
            baseline_feasible,
            improvement: improvement_ratio,
            headline,
            margin_cost_ratio,
            throttle_ceiling: infeasible_at,
            safe_concurrency: safe,
            capacity: Some(cap),
            max_throttle_ratio: opts.max_throttle_ratio,
            pareto,
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

    fn tr(id: usize, ops: f64, p99: f64, cost: f64, thr: f64) -> Trial {
        Trial {
            id,
            stage: "concurrency".into(),
            ops_per_s: ops,
            p99_ms: p99,
            cost_per_million_ops: cost,
            throttled_request_ratio: thr,
            ..Default::default()
        }
    }

    #[test]
    fn pareto_front_drops_dominated_trials() {
        let trials = vec![
            tr(0, 100.0, 10.0, 1.0, 0.0),
            tr(1, 200.0, 12.0, 1.0, 0.0),
            tr(2, 150.0, 15.0, 1.0, 0.0), // dominated by 1
            tr(3, 300.0, 40.0, 1.0, 0.05),
        ];
        assert_eq!(pareto_front(&trials, false), vec![0, 1, 3]);
    }

    #[test]
    fn objective_labels_and_improvement_signs() {
        assert_eq!(Objective::parse("throughput"), Some(Objective::Throughput));
        assert_eq!(Objective::parse("goodput").unwrap().label(), "goodput");
        assert_eq!(Objective::parse("latency").unwrap().label(), "p99");
        assert_eq!(Objective::parse("balanced"), Some(Objective::Balanced));
        assert!(Objective::parse("nope").is_none());
        let (a, b) = (tr(0, 100.0, 20.0, 1.0, 0.0), tr(1, 150.0, 10.0, 0.5, 0.0));
        assert!((improvement(Objective::Throughput, false, &a, &b) - 0.5).abs() < 1e-9);
        assert!((improvement(Objective::Latency, false, &a, &b) - 0.5).abs() < 1e-9);
        assert!((improvement(Objective::Cost, false, &a, &b) - 0.5).abs() < 1e-9);
        assert!(improvement(Objective::Throughput, false, &b, &a) < 0.0);
        let o = TuneOptions::new(
            Objective::Throughput,
            RunOptions {
                duration: Duration::from_secs(1),
                warmup: Duration::ZERO,
                max_ops: None,
                rate: None,
                trace_slow_ops: false,
            },
        );
        assert_eq!(o.max_throttle_ratio, 0.001);
        assert_eq!(o.boundary_repeats, 2);
    }
}
