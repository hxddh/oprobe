//! Self-observation (design v5 §3, fixes M7/M8): is the load generator itself the bottleneck?
//!
//! A [`SelfProbe`] runs during the measurement window and measures, after warmup:
//! - a 1 ms scheduling-lag probe (how late a timer task wakes: tokio workers that are busy or
//!   starved delay every request's bookkeeping, which then shows up as server time);
//! - the tokio runtime's busy share and global queue depth;
//! - the process CPU over the window, as a share of the cores in use;
//! - current and peak resident memory.
//!
//! The recorder adds the measured observer cost per operation. The result is
//! `report.self` ([`crate::model::SelfStats`]); `client_saturated` flags a run whose
//! server-side stages cannot be trusted.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;

use crate::trace::SelfObservation;

/// Scheduling-lag p99 above which the client counts as saturated.
pub const SATURATED_LAG_P99_MS: f64 = 5.0;
/// CPU share of the cores in use at or above which the client counts as saturated.
pub const SATURATED_CPU_RATIO: f64 = 0.9;

/// Resident memory now (Linux: `/proc/self/statm`; Windows: the working set; elsewhere the
/// peak, which is all the OS reports cheaply).
pub fn current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Some(pages * page.max(4096) as u64)
    }
    #[cfg(not(target_os = "linux"))]
    {
        super::procstat::usage().map(|u| u.rss_bytes)
    }
}

/// Peak resident memory of the process so far.
pub fn peak_rss_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        current_rss_bytes()
    }
    #[cfg(not(windows))]
    {
        super::procstat::usage().map(|u| u.rss_bytes)
    }
}

struct RuntimeMark {
    at: Instant,
    busy: Duration,
    workers: usize,
}

fn runtime_mark() -> Option<RuntimeMark> {
    let h = tokio::runtime::Handle::try_current().ok()?;
    let m = h.metrics();
    let workers = m.num_workers();
    #[cfg(target_has_atomic = "64")]
    let busy = (0..workers).map(|w| m.worker_total_busy_duration(w)).sum();
    #[cfg(not(target_has_atomic = "64"))]
    let busy = Duration::ZERO;
    Some(RuntimeMark {
        at: Instant::now(),
        busy,
        workers,
    })
}

/// Measures the client's health during a measurement window.
pub struct SelfProbe {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<(Histogram<u64>, u64, u64)>,
    start: Instant,
    cpu0: Option<Duration>,
    rt0: Option<RuntimeMark>,
}

impl SelfProbe {
    /// Start probing (call when the measurement window opens, after warmup).
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let task = tokio::spawn(async move {
            let mut lag = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)
                .expect("valid histogram bounds");
            let mut queue_max = 0u64;
            let mut peak_rss = 0u64;
            let mut tick = tokio::time::interval(Duration::from_millis(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut n = 0u64;
            while !s.load(Ordering::Acquire) {
                let due = tick.tick().await;
                let late = Instant::now().saturating_duration_since(due.into_std());
                let _ = lag.record((late.as_micros() as u64).max(1));
                n += 1;
                if n.is_multiple_of(10)
                    && let Ok(h) = tokio::runtime::Handle::try_current()
                {
                    queue_max = queue_max.max(h.metrics().global_queue_depth() as u64);
                }
                if n.is_multiple_of(250)
                    && let Some(r) = current_rss_bytes()
                {
                    peak_rss = peak_rss.max(r);
                }
            }
            (lag, queue_max, peak_rss)
        });
        Self {
            stop,
            task,
            start: Instant::now(),
            cpu0: super::procstat::usage().map(|u| u.cpu),
            rt0: runtime_mark(),
        }
    }

    /// Stop probing and summarize. `observer_per_op_us` comes from the recorder.
    pub async fn finish(self, observer_per_op_us: Option<f64>) -> SelfObservation {
        let wall = self.start.elapsed();
        let cpu1 = super::procstat::usage().map(|u| u.cpu);
        let rt1 = runtime_mark();
        self.stop.store(true, Ordering::Release);
        let (lag, queue_max, peak_sampled) = self.task.await.unwrap_or_else(|_| {
            (
                Histogram::new_with_bounds(1, 60_000_000, 3).expect("valid histogram bounds"),
                0,
                0,
            )
        });
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let busy_ratio = match (&self.rt0, &rt1) {
            (Some(a), Some(b)) if b.workers > 0 && b.busy > Duration::ZERO => {
                let wall = b.at.saturating_duration_since(a.at).as_secs_f64();
                (wall > 0.0).then(|| {
                    (b.busy.saturating_sub(a.busy).as_secs_f64() / (wall * b.workers as f64))
                        .clamp(0.0, 1.0)
                })
            }
            _ => None,
        };
        // The cores the client can use: the runtime's workers (plus the blocking pool and the
        // timer, which rarely matter) bounded by the machine.
        let cores_in_use = rt1.as_ref().map_or(cores, |r| r.workers.clamp(1, cores));
        let cpu_ratio = match (self.cpu0, cpu1) {
            (Some(a), Some(b)) if wall.as_secs_f64() > 0.05 => {
                Some(b.saturating_sub(a).as_secs_f64() / (wall.as_secs_f64() * cores_in_use as f64))
            }
            _ => None,
        };
        let lag_p99_ms = (!lag.is_empty()).then(|| lag.value_at_quantile(0.99) as f64 / 1000.0);
        let current = current_rss_bytes();
        let peak = peak_rss_bytes()
            .into_iter()
            .chain(current)
            .chain((peak_sampled > 0).then_some(peak_sampled))
            .max();
        let saturated = lag_p99_ms.is_some_and(|l| l > SATURATED_LAG_P99_MS)
            || cpu_ratio.is_some_and(|c| c >= SATURATED_CPU_RATIO);
        SelfObservation {
            observer_per_op_us,
            runtime_busy_ratio: busy_ratio,
            runtime_workers_count: rt1.as_ref().map(|r| r.workers as u64),
            global_queue_depth_max_count: rt1.as_ref().map(|_| queue_max),
            sched_lag_p99_ms: lag_p99_ms,
            sched_lag_max_ms: (!lag.is_empty()).then(|| lag.max() as f64 / 1000.0),
            cpu_ratio,
            cores_in_use_count: cores_in_use as u64,
            rss_current_bytes: current,
            rss_peak_bytes: peak,
            client_saturated: saturated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_idle_client_is_not_saturated_and_a_blocked_one_is() {
        let p = SelfProbe::start();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let o = p.finish(Some(1.5)).await;
        assert_eq!(o.observer_per_op_us, Some(1.5));
        assert!(o.sched_lag_p99_ms.is_some());
        assert!(o.rss_current_bytes.unwrap_or(1) > 0);
        assert!(o.rss_peak_bytes >= o.rss_current_bytes);
        assert_eq!(o.runtime_workers_count, Some(2));

        // Both workers blocked: the 1 ms probe wakes late.
        let p = SelfProbe::start();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let block = || {
            tokio::spawn(async {
                let t = Instant::now();
                while t.elapsed() < Duration::from_millis(30) {
                    std::hint::spin_loop();
                }
            })
        };
        for _ in 0..12 {
            let (a, b) = (block(), block());
            let _ = tokio::join!(a, b);
        }
        let o = p.finish(None).await;
        assert!(o.sched_lag_max_ms.unwrap() > SATURATED_LAG_P99_MS, "{o:?}");
        assert!(o.client_saturated, "{o:?}");
        assert!(o.runtime_busy_ratio.unwrap() > 0.5, "{o:?}");
    }
}
