//! Where the traffic and the trouble are (design v5 §3, fixes M12): every measured operation
//! is counted by key prefix, hot key, object size bucket and cold vs warm read. Aggregates
//! (not sampled rows), with bounded memory.

use crate::util::{FxHashMap as HashMap, FxHashSet as HashSet};

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

/// Prefixes tracked at most (the rest count as `(other)`).
const MAX_PREFIXES: usize = 256;
/// Keys tracked for hot-key detection at most.
const MAX_KEYS: usize = 8192;
/// Keys remembered for cold/warm detection at most (hashes; ~8 bytes each).
const MAX_SEEN: usize = 2_000_000;
/// Hot keys reported.
pub const HOT_KEYS: usize = 20;
/// Path segments of a prefix slice by default.
pub const DEFAULT_PREFIX_DEPTH: u32 = 1;

/// One slice: operations, requests, failures, throttling and the tail.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct SliceStats {
    pub key: String,
    pub ops_count: u64,
    pub requests_count: u64,
    pub throttled_count: u64,
    pub errors_count: u64,
    pub p99_ms: f64,
}

/// A hot key's traffic and throttling.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct HotKeyStats {
    pub key: String,
    pub requests_count: u64,
    pub throttled_count: u64,
}

/// The dimensions of a measurement window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct DimensionStats {
    pub prefix_depth_count: u32,
    pub prefixes: Vec<SliceStats>,
    pub hot_keys: Vec<HotKeyStats>,
    pub size_buckets: Vec<SliceStats>,
    pub cold_warm: Vec<SliceStats>,
    pub total_requests_count: u64,
    pub total_throttled_count: u64,
    /// Keys beyond the hot-key table (their traffic is not in `hot_keys`).
    pub untracked_keys_count: u64,
}

struct Slice {
    ops: u64,
    requests: u64,
    throttled: u64,
    errors: u64,
    lat: Histogram<u64>,
}

impl Slice {
    fn new() -> Self {
        Self {
            ops: 0,
            requests: 0,
            throttled: 0,
            errors: 0,
            lat: Histogram::new_with_bounds(1, 3_600_000_000, 2).expect("valid histogram bounds"),
        }
    }

    fn add(&mut self, o: &DimOp) {
        self.ops += 1;
        self.requests += o.requests;
        self.throttled += o.throttled_requests;
        self.errors += u64::from(o.failed);
        let _ = self.lat.record(o.latency_us.clamp(1, 3_600_000_000));
    }

    fn stats(&self, key: &str) -> SliceStats {
        SliceStats {
            key: key.to_owned(),
            ops_count: self.ops,
            requests_count: self.requests,
            throttled_count: self.throttled,
            errors_count: self.errors,
            p99_ms: if self.lat.is_empty() {
                0.0
            } else {
                self.lat.value_at_quantile(0.99) as f64 / 1000.0
            },
        }
    }
}

/// One completed operation, as the dimensions see it.
pub struct DimOp<'a> {
    pub verb: &'a str,
    pub key: &'a str,
    pub bytes: u64,
    pub latency_us: u64,
    pub requests: u64,
    pub throttled_requests: u64,
    pub failed: bool,
}

const SIZE_BUCKETS: [(&str, u64); 6] = [
    ("0", 1),
    ("<4KiB", 4 << 10),
    ("<64KiB", 64 << 10),
    ("<1MiB", 1 << 20),
    ("<16MiB", 16 << 20),
    (">=16MiB", u64::MAX),
];

/// The aggregates (one per recorder window).
pub struct DimAgg {
    depth: u32,
    prefixes: HashMap<String, Slice>,
    other: Slice,
    keys: HashMap<String, (u64, u64)>,
    untracked: u64,
    sizes: Vec<Slice>,
    cold: Slice,
    warm: Slice,
    seen: HashSet<u64>,
    total_requests: u64,
    total_throttled: u64,
}

impl Default for DimAgg {
    fn default() -> Self {
        Self::new(DEFAULT_PREFIX_DEPTH)
    }
}

fn prefix_of(key: &str, depth: u32) -> &str {
    let mut end = 0;
    for _ in 0..depth.max(1) {
        match key[end..].find('/') {
            Some(i) => end += i + 1,
            None => return if end == 0 { "/" } else { &key[..end] },
        }
    }
    &key[..end]
}

fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = crate::util::FxHasher::default();
    s.hash(&mut h);
    h.finish()
}

impl DimAgg {
    pub fn new(depth: u32) -> Self {
        Self {
            depth: depth.max(1),
            prefixes: HashMap::default(),
            other: Slice::new(),
            keys: HashMap::default(),
            untracked: 0,
            sizes: SIZE_BUCKETS.iter().map(|_| Slice::new()).collect(),
            cold: Slice::new(),
            warm: Slice::new(),
            seen: HashSet::default(),
            total_requests: 0,
            total_throttled: 0,
        }
    }

    pub fn record(&mut self, o: &DimOp) {
        self.total_requests += o.requests;
        self.total_throttled += o.throttled_requests;
        if o.key.is_empty() {
            return;
        }
        let p = prefix_of(o.key, self.depth);
        let n_prefixes = self.prefixes.len();
        if let Some(s) = self.prefixes.get_mut(p) {
            s.add(o);
        } else if n_prefixes < MAX_PREFIXES {
            let mut s = Slice::new();
            s.add(o);
            self.prefixes.insert(p.to_owned(), s);
        } else {
            self.other.add(o);
        }
        let n_keys = self.keys.len();
        if let Some(k) = self.keys.get_mut(o.key) {
            k.0 += o.requests;
            k.1 += o.throttled_requests;
        } else if n_keys < MAX_KEYS {
            self.keys
                .insert(o.key.to_owned(), (o.requests, o.throttled_requests));
        } else {
            self.untracked += 1;
        }
        let i = SIZE_BUCKETS
            .iter()
            .position(|(_, lim)| o.bytes < *lim)
            .unwrap_or(SIZE_BUCKETS.len() - 1);
        self.sizes[i].add(o);
        let read =
            o.verb.starts_with("read") || o.verb.starts_with("parquet") || o.verb == "replay.read";
        if read {
            let h = hash(o.key);
            if self.seen.contains(&h) {
                self.warm.add(o);
            } else {
                if self.seen.len() < MAX_SEEN {
                    self.seen.insert(h);
                }
                self.cold.add(o);
            }
        }
    }

    pub fn stats(&self) -> DimensionStats {
        let mut prefixes: Vec<SliceStats> = self.prefixes.iter().map(|(k, s)| s.stats(k)).collect();
        if self.other.ops > 0 {
            prefixes.push(self.other.stats("(other)"));
        }
        prefixes.sort_by(|a, b| {
            b.requests_count
                .cmp(&a.requests_count)
                .then(a.key.cmp(&b.key))
        });
        let mut hot: Vec<HotKeyStats> = self
            .keys
            .iter()
            .map(|(k, (r, t))| HotKeyStats {
                key: k.clone(),
                requests_count: *r,
                throttled_count: *t,
            })
            .collect();
        hot.sort_by(|a, b| {
            b.requests_count
                .cmp(&a.requests_count)
                .then(a.key.cmp(&b.key))
        });
        hot.truncate(HOT_KEYS);
        let nonempty = |v: Vec<SliceStats>| v.into_iter().filter(|s| s.ops_count > 0).collect();
        DimensionStats {
            prefix_depth_count: self.depth,
            prefixes,
            hot_keys: hot,
            size_buckets: nonempty(
                SIZE_BUCKETS
                    .iter()
                    .zip(&self.sizes)
                    .map(|((name, _), s)| s.stats(name))
                    .collect(),
            ),
            cold_warm: nonempty(vec![self.cold.stats("cold"), self.warm.stats("warm")]),
            total_requests_count: self.total_requests,
            total_throttled_count: self.total_throttled,
            untracked_keys_count: self.untracked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op<'a>(verb: &'a str, key: &'a str, bytes: u64, throttled: bool) -> DimOp<'a> {
        DimOp {
            verb,
            key,
            bytes,
            latency_us: 1000,
            requests: 1,
            throttled_requests: u64::from(throttled),
            failed: false,
        }
    }

    #[test]
    fn slices_by_prefix_key_size_and_temperature() {
        assert_eq!(prefix_of("a/b/c", 1), "a/");
        assert_eq!(prefix_of("a/b/c", 2), "a/b/");
        assert_eq!(prefix_of("flat", 1), "/");
        let mut d = DimAgg::new(1);
        for _ in 0..3 {
            d.record(&op("read.full", "hot/k1", 100, true));
        }
        d.record(&op("read.full", "cold/k2", 10 << 20, false));
        d.record(&op("write", "w/k3", 0, false));
        let s = d.stats();
        assert_eq!(s.prefixes[0].key, "hot/");
        assert_eq!(s.prefixes[0].throttled_count, 3);
        assert_eq!(s.hot_keys[0].key, "hot/k1");
        assert_eq!(s.hot_keys[0].requests_count, 3);
        assert_eq!(s.total_throttled_count, 3);
        let names: Vec<&str> = s.size_buckets.iter().map(|b| b.key.as_str()).collect();
        assert_eq!(names, ["0", "<4KiB", "<16MiB"]);
        // First read of a key is cold, repeats are warm; writes are neither.
        assert_eq!(s.cold_warm[0].key, "cold");
        assert_eq!(s.cold_warm[0].ops_count, 2);
        assert_eq!(s.cold_warm[1].ops_count, 2);
    }
}
