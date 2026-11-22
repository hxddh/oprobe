//! Limit faults of the lab (design v5 §4, lab parity): one per limit model the analysis fits.
//!
//! - **Rate limit** ([`RateLimitFault`]): a token bucket over every request; an empty bucket
//!   answers `503 SlowDown` (or `429`). Optionally it steps to another rate a while after the
//!   first request ([`RateLimitFault::after`]): a time-varying limit.
//! - **Per-key limit** ([`KeyRateLimitFault`]): one token bucket per object key (or per
//!   prefix, the key up to its last `/`): a hot key throttles while the others are served.
//! - **Queueing** ([`QueueFault`]): an M/M/c server — `servers` requests are served at once,
//!   each for an exponentially distributed time; the others wait in line (latency grows with
//!   the requests in flight, nothing is throttled).
//! - **Aggregate bandwidth cap** ([`LimitState::pace_bytes`]): every response body shares one
//!   byte budget per second, however many streams there are.
//! - **Random 5xx mix** ([`LabConfig::server_error_ratio`](super::LabConfig)): a share of
//!   requests answered `503`/`500` at random, unrelated to the load.
//!
//! The runtime state (buckets, the queue's semaphore, the shared byte clock) lives in
//! [`LimitState`], one per lab server.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A token bucket over all requests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateLimitFault {
    /// Tokens added per second.
    pub requests_per_s: f64,
    /// Bucket size (requests accepted at once after an idle period).
    pub burst: u32,
    /// After this long (from the first request the lab served), the rate becomes the second
    /// value: a time-varying limit.
    pub after: Option<(Duration, f64)>,
}

/// One token bucket per key (or per prefix).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyRateLimitFault {
    pub requests_per_s: f64,
    pub burst: u32,
    /// Limit per prefix (the key up to its last `/`) instead of per key.
    pub by_prefix: bool,
}

/// An M/M/c queue in front of the handler.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueueFault {
    /// Requests served at once.
    pub servers: usize,
    /// Mean service time (exponentially distributed).
    pub service: Duration,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn new(burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            last: now,
        }
    }

    /// Refill at `rate` up to `burst`, then take one token if there is one.
    fn take(&mut self, rate: f64, burst: f64, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * rate).min(burst.max(1.0));
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The runtime state of the limit faults of one lab server.
#[derive(Debug)]
pub struct LimitState {
    first: Mutex<Option<Instant>>,
    global: Mutex<Option<Bucket>>,
    keys: Mutex<HashMap<String, Bucket>>,
    queue: Mutex<Option<(usize, std::sync::Arc<tokio::sync::Semaphore>)>>,
    /// When the shared byte budget is next free (the aggregate bandwidth cap).
    byte_clock: Mutex<Option<Instant>>,
    rng: AtomicU64,
}

impl Default for LimitState {
    fn default() -> Self {
        Self {
            first: Mutex::new(None),
            global: Mutex::new(None),
            keys: Mutex::new(HashMap::new()),
            queue: Mutex::new(None),
            byte_clock: Mutex::new(None),
            rng: AtomicU64::new(0x9e37_79b9_7f4a_7c15),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The key a per-key limit counts: the object key (`bucket/key` path), or its prefix.
pub fn limit_key(path: &str, by_prefix: bool) -> String {
    let p = path.trim_start_matches('/');
    if by_prefix {
        match p.rfind('/') {
            Some(i) => p[..i].to_owned(),
            None => String::new(),
        }
    } else {
        p.to_owned()
    }
}

impl LimitState {
    /// A uniform random number in `[0, 1)` (splitmix64; good enough for fault injection).
    pub fn uniform(&self) -> f64 {
        let mut z = self
            .rng
            .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Time since the first request this state saw (it starts the clock).
    pub fn elapsed(&self, now: Instant) -> Duration {
        let mut f = lock(&self.first);
        let start = *f.get_or_insert(now);
        now.saturating_duration_since(start)
    }

    /// The rate of `f` now (it may have stepped).
    pub fn current_rate(&self, f: &RateLimitFault, now: Instant) -> f64 {
        match f.after {
            Some((after, rps)) if self.elapsed(now) >= after => rps,
            _ => {
                self.elapsed(now);
                f.requests_per_s
            }
        }
    }

    /// Admit one request under the global bucket (`false`: throttle it).
    pub fn admit_global(&self, f: &RateLimitFault, now: Instant) -> bool {
        let rate = self.current_rate(f, now).max(0.0);
        let burst = f64::from(f.burst.max(1));
        let mut g = lock(&self.global);
        g.get_or_insert_with(|| Bucket::new(burst, now))
            .take(rate, burst, now)
    }

    /// Admit one request for `key` under the per-key buckets (`false`: throttle it).
    pub fn admit_key(&self, f: &KeyRateLimitFault, path: &str, now: Instant) -> bool {
        let key = limit_key(path, f.by_prefix);
        let burst = f64::from(f.burst.max(1));
        let mut m = lock(&self.keys);
        m.entry(key)
            .or_insert_with(|| Bucket::new(burst, now))
            .take(f.requests_per_s.max(0.0), burst, now)
    }

    /// Wait in line for one of the queue's servers, then for an exponentially distributed
    /// service time. The returned permit frees the server when dropped.
    pub async fn serve_queued(&self, q: &QueueFault) -> Option<tokio::sync::OwnedSemaphorePermit> {
        if q.servers == 0 {
            return None;
        }
        let sem = {
            let mut g = lock(&self.queue);
            match g.as_ref() {
                Some((n, s)) if *n == q.servers => s.clone(),
                _ => {
                    let s = std::sync::Arc::new(tokio::sync::Semaphore::new(q.servers));
                    *g = Some((q.servers, s.clone()));
                    s
                }
            }
        };
        let permit = sem.acquire_owned().await.ok()?;
        let u = self.uniform().max(1e-9);
        let service = q.service.as_secs_f64() * -u.ln();
        // Cap a single service time at 20 means so one sample cannot stall a short run.
        let service = service.min(q.service.as_secs_f64() * 20.0);
        tokio::time::sleep(Duration::from_secs_f64(service)).await;
        Some(permit)
    }

    /// Reserve `bytes` of the aggregate byte budget (`kib_s` KiB/s shared by every stream) and
    /// return how long to wait before sending them.
    pub fn pace_bytes(&self, bytes: usize, kib_s: u64, now: Instant) -> Duration {
        if kib_s == 0 {
            return Duration::ZERO;
        }
        let cost = Duration::from_secs_f64(bytes as f64 / (kib_s as f64 * 1024.0));
        let mut c = lock(&self.byte_clock);
        // An idle budget does not bank credit beyond 50 ms (a small burst).
        let floor = now.checked_sub(Duration::from_millis(50)).unwrap_or(now);
        let start = c.map_or(floor, |t| t.max(floor));
        let done = start + cost;
        *c = Some(done);
        done.saturating_duration_since(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_bucket_admits_its_rate() {
        let s = LimitState::default();
        let f = RateLimitFault {
            requests_per_s: 100.0,
            burst: 10,
            after: None,
        };
        let t0 = Instant::now();
        // 1000 arrivals over one second (every ms): the burst plus ~100 refills.
        let ok = (0..1000)
            .filter(|i| s.admit_global(&f, t0 + Duration::from_millis(*i)))
            .count();
        assert!((105..=112).contains(&ok), "{ok}");
    }

    #[test]
    fn the_rate_steps_after_its_time() {
        let s = LimitState::default();
        let f = RateLimitFault {
            requests_per_s: 400.0,
            burst: 1,
            after: Some((Duration::from_secs(1), 100.0)),
        };
        let t0 = Instant::now();
        let ok = |from: u64, to: u64| {
            (from..to)
                .filter(|i| s.admit_global(&f, t0 + Duration::from_micros(i * 500)))
                .count()
        };
        let first = ok(0, 2000);
        let second = ok(2000, 4000);
        assert!((380..=420).contains(&first), "{first}");
        assert!((90..=110).contains(&second), "{second}");
    }

    #[test]
    fn per_key_buckets_are_independent() {
        let s = LimitState::default();
        let f = KeyRateLimitFault {
            requests_per_s: 10.0,
            burst: 2,
            by_prefix: false,
        };
        let t = Instant::now();
        assert!(s.admit_key(&f, "/lab/a", t));
        assert!(s.admit_key(&f, "/lab/a", t));
        assert!(!s.admit_key(&f, "/lab/a", t));
        assert!(s.admit_key(&f, "/lab/b", t));
        assert_eq!(limit_key("/lab/p/x/obj", true), "lab/p/x");
        assert_eq!(limit_key("/lab/p/x/obj", false), "lab/p/x/obj");
    }

    #[test]
    fn the_byte_clock_is_shared() {
        let s = LimitState::default();
        let t = Instant::now();
        // 1 MiB/s: two 512 KiB chunks at once — the second waits ~0.5 s more than the first.
        let a = s.pace_bytes(512 * 1024, 1024, t);
        let b = s.pace_bytes(512 * 1024, 1024, t);
        assert!(b > a, "{a:?} {b:?}");
        assert!((b - a).as_secs_f64() > 0.45, "{a:?} {b:?}");
    }
}
