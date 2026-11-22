//! A run prepared for display: everything the inspector's tabs draw, derived once in Rust when
//! the run is opened (off the UI thread), then only read.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::model::{HEATMAP_LATENCY_BOUNDS_MS, OpRow, ReqRow, Run};

use super::series::{Series, quantiles};

/// One connection of the run (Connections tab).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnRow {
    pub conn_id: u64,
    pub remote: String,
    pub protocol: &'static str,
    pub requests: u64,
    /// Requests after the first (that reused it).
    pub reused: u64,
    pub errors: u64,
    pub first_t_us: i64,
    pub last_t_us: i64,
    pub dns_us: u64,
    pub tcp_us: u64,
    pub proxy_us: u64,
    pub tls_us: u64,
    /// `full`, `resumed`, `hrr`, or empty (no TLS / unknown).
    pub tls_kind: &'static str,
    pub tls_version: String,
    /// Smoothed RTT at the last request that reported TCP_INFO.
    pub rtt_us: Option<u64>,
    pub retrans: Option<u64>,
    pub bytes_down: u64,
}

/// One remote IP (Connections tab).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteRow {
    pub ip: String,
    pub conns: u64,
    pub requests: u64,
    pub errors: u64,
    pub reuse_pct: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub dial_p50_ms: f64,
}

/// Request latency over time (time × log latency), for the brushable heatmap.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReqHeatmap {
    pub bucket_s: f64,
    pub columns: usize,
    /// Lower bound (ms) of each latency row; the last is open-ended.
    pub bounds_ms: Vec<f64>,
    /// `counts[col * rows + row]`.
    pub counts: Vec<u32>,
    pub max: u32,
}

impl ReqHeatmap {
    pub fn rows(&self) -> usize {
        self.bounds_ms.len()
    }
    pub fn at(&self, col: usize, row: usize) -> u32 {
        self.counts
            .get(col * self.rows() + row)
            .copied()
            .unwrap_or(0)
    }

    fn build(reqs: &[ReqRow], window_s: f64) -> Self {
        let columns = 60usize;
        let bucket_s = (window_s.max(0.1) / columns as f64).max(1e-3);
        // Rows: from the fastest decade seen to the slowest (fixed bounds, comparable runs).
        let min_ms = reqs
            .iter()
            .map(|r| r.total_us as f64 / 1e3)
            .fold(f64::INFINITY, f64::min);
        let max_ms = reqs
            .iter()
            .map(|r| r.total_us as f64 / 1e3)
            .fold(0.0, f64::max);
        let all = HEATMAP_LATENCY_BOUNDS_MS;
        let lo = all.partition_point(|b| *b <= min_ms).saturating_sub(1);
        let hi = all.partition_point(|b| *b <= max_ms).max(lo + 1);
        let bounds_ms: Vec<f64> = all[lo..hi.min(all.len())].to_vec();
        let rows = bounds_ms.len().max(1);
        let mut counts = vec![0u32; columns * rows];
        for r in reqs {
            if r.t_us < 0 {
                continue;
            }
            let c = ((r.t_us as f64 / 1e6) / bucket_s) as usize;
            if c >= columns {
                continue;
            }
            let ms = r.total_us as f64 / 1e3;
            let row = bounds_ms.partition_point(|b| *b <= ms).saturating_sub(1);
            counts[c * rows + row.min(rows - 1)] += 1;
        }
        let max = counts.iter().copied().max().unwrap_or(0);
        Self {
            bucket_s,
            columns,
            bounds_ms,
            counts,
            max,
        }
    }
}

/// A run and what its views need, derived once.
#[derive(Debug)]
pub struct RunView {
    pub run: Run,
    pub series: Series,
    /// Operation indices by start time (the waterfall's rows).
    pub op_order: Vec<u32>,
    /// Request indices of each operation id, in start order.
    pub reqs_by_op: HashMap<u64, Vec<u32>>,
    /// Operation index by id.
    pub op_index: HashMap<u64, u32>,
    pub conns: Vec<ConnRow>,
    pub remotes: Vec<RemoteRow>,
    pub heatmap: ReqHeatmap,
    /// Earliest request/op start and latest end, µs (the timeline's extent).
    pub t_min_us: i64,
    pub t_max_us: i64,
    pub path: Option<std::path::PathBuf>,
}

/// A shared, cheaply comparable handle (signals need `PartialEq`).
#[derive(Clone, Debug)]
pub struct Shared(pub Arc<RunView>);

impl PartialEq for Shared {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::ops::Deref for Shared {
    type Target = RunView;
    fn deref(&self) -> &RunView {
        &self.0
    }
}

impl RunView {
    pub fn new(run: Run, path: Option<std::path::PathBuf>) -> Self {
        let window_s = run.report.run.window_s;
        let series = Series::from_ops(&run.ops, window_s);
        let mut op_order: Vec<u32> = (0..run.ops.len() as u32).collect();
        op_order.sort_by_key(|i| run.ops[*i as usize].t_us);
        let op_index: HashMap<u64, u32> = run
            .ops
            .iter()
            .enumerate()
            .map(|(i, o)| (o.op_id, i as u32))
            .collect();
        let mut reqs_by_op: HashMap<u64, Vec<u32>> = HashMap::new();
        for (i, r) in run.reqs.iter().enumerate() {
            reqs_by_op.entry(r.op_id).or_default().push(i as u32);
        }
        for v in reqs_by_op.values_mut() {
            v.sort_by_key(|i| run.reqs[*i as usize].t_us);
        }
        let (conns, remotes) = connections(&run.reqs);
        let heatmap = ReqHeatmap::build(&run.reqs, window_s);
        let t_min_us = run
            .ops
            .iter()
            .map(|o| o.t_us)
            .chain(run.reqs.iter().map(|r| r.t_us))
            .min()
            .unwrap_or(0)
            .min(0);
        let t_max_us = run
            .ops
            .iter()
            .map(|o| o.t_us + o.latency_us as i64)
            .chain(run.reqs.iter().map(|r| r.t_us + r.total_us as i64))
            .max()
            .unwrap_or(0)
            .max((window_s * 1e6) as i64);
        Self {
            run,
            series,
            op_order,
            reqs_by_op,
            op_index,
            conns,
            remotes,
            heatmap,
            t_min_us,
            t_max_us,
            path,
        }
    }

    pub fn id(&self) -> &str {
        self.run.id()
    }

    /// Request indices of the operation at op index `i`.
    pub fn reqs_of_op_index(&self, i: u32) -> &[u32] {
        self.run
            .ops
            .get(i as usize)
            .and_then(|o| self.reqs_by_op.get(&o.op_id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// The Timeline's rows (U8): the operations at or above the run's p99 latency (`tail`), or all
/// of them; slowest first (`slowest`), else in start order (`by_start`, the run's own order).
/// The p99 tail keeps at least 20 operations so a short run still shows something to read.
pub fn timeline_order(ops: &[OpRow], by_start: &[u32], slowest: bool, tail: bool) -> Vec<u32> {
    let mut idx: Vec<u32> = by_start.to_vec();
    if tail && !ops.is_empty() {
        let mut lat: Vec<u64> = ops.iter().map(|o| o.latency_us).collect();
        lat.sort_unstable();
        let keep = (ops.len() / 100).max(20).min(ops.len());
        let floor = lat[lat.len() - keep];
        idx.retain(|i| ops[*i as usize].latency_us >= floor);
    }
    if slowest {
        idx.sort_by_key(|i| std::cmp::Reverse(ops[*i as usize].latency_us));
    }
    idx
}

fn tls_kind(r: &ReqRow) -> &'static str {
    match &r.tls {
        Some(t) if t.hrr => "hrr",
        Some(t) if t.resumed => "resumed",
        Some(_) => "full",
        None if r.tls_us > 0 => "full",
        None => "",
    }
}

/// Per-connection and per-remote-IP tables.
pub fn connections(reqs: &[ReqRow]) -> (Vec<ConnRow>, Vec<RemoteRow>) {
    let mut by_conn: BTreeMap<u64, ConnRow> = BTreeMap::new();
    for r in reqs {
        let c = by_conn.entry(r.conn_id).or_insert_with(|| ConnRow {
            conn_id: r.conn_id,
            remote: r.remote.map(|a| a.to_string()).unwrap_or_default(),
            protocol: super::requests::proto(r.protocol),
            first_t_us: r.t_us,
            last_t_us: r.t_us,
            ..Default::default()
        });
        c.requests += 1;
        if r.reused {
            c.reused += 1;
        }
        if r.is_error() {
            c.errors += 1;
        }
        c.first_t_us = c.first_t_us.min(r.t_us);
        c.last_t_us = c.last_t_us.max(r.t_us);
        c.bytes_down += r.bytes_down;
        if !r.reused && r.connect_us() > 0 {
            c.dns_us = r.dns_us;
            c.tcp_us = r.tcp_us;
            c.proxy_us = r.proxy_us;
            c.tls_us = r.tls_us;
            c.tls_kind = tls_kind(r);
            if let Some(t) = &r.tls {
                c.tls_version = t.version.clone();
            }
        }
        if let Some(t) = &r.tcp {
            c.rtt_us = Some(t.rtt_us);
            c.retrans = Some(t.retrans);
        }
    }
    let conns: Vec<ConnRow> = by_conn.into_values().collect();

    let mut by_ip: BTreeMap<String, (RemoteRow, Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for r in reqs {
        let ip = r
            .remote
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|| "local".into());
        let e = by_ip.entry(ip.clone()).or_insert_with(|| {
            (
                RemoteRow {
                    ip,
                    ..Default::default()
                },
                Vec::new(),
                Vec::new(),
            )
        });
        e.0.requests += 1;
        if r.is_error() {
            e.0.errors += 1;
        }
        if r.reused {
            e.0.reuse_pct += 1.0;
        }
        e.1.push(r.total_us as f64 / 1e3);
        if !r.reused && r.connect_us() > 0 {
            e.2.push(r.connect_us() as f64 / 1e3);
        }
    }
    for c in &conns {
        let ip = c
            .remote
            .rsplit_once(':')
            .map(|(h, _)| h.trim_matches(['[', ']']).to_owned())
            .unwrap_or_else(|| "local".into());
        if let Some(e) = by_ip.get_mut(&ip) {
            e.0.conns += 1;
        }
    }
    let remotes = by_ip
        .into_values()
        .map(|(mut row, mut lat, mut dial)| {
            row.reuse_pct = row.reuse_pct / row.requests.max(1) as f64 * 100.0;
            (row.p50_ms, row.p99_ms) = quantiles(&mut lat);
            row.dial_p50_ms = quantiles(&mut dial).0;
            row
        })
        .collect();
    (conns, remotes)
}

#[cfg(test)]
mod tests {
    use super::super::requests::tests::req;
    use super::*;

    #[test]
    fn the_timeline_opens_on_the_slowest_operations() {
        let ops: Vec<OpRow> = (0..100u64)
            .map(|i| {
                let mut o = crate::report::fixture::run().ops[0].clone();
                o.t_us = i as i64;
                o.latency_us = if i == 7 { 1_000_000 } else { 1_000 + i };
                o
            })
            .collect();
        let by_start: Vec<u32> = (0..100).collect();
        let tail = timeline_order(&ops, &by_start, true, true);
        assert_eq!(tail.len(), 20);
        assert_eq!(tail[0], 7);
        assert!(
            tail.windows(2)
                .all(|w| ops[w[0] as usize].latency_us >= ops[w[1] as usize].latency_us)
        );
        assert_eq!(timeline_order(&ops, &by_start, false, false), by_start);
        assert!(timeline_order(&[], &[], true, true).is_empty());
    }

    #[test]
    fn connections_and_remotes() {
        let mut a = req(1, "GetObject", 200, 5);
        a.reused = false;
        a.tcp_us = 900;
        a.conn_id = 7;
        let mut b = req(2, "GetObject", 503, 50);
        b.conn_id = 7;
        let (conns, remotes) = connections(&[a, b]);
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].requests, 2);
        assert_eq!(conns[0].reused, 1);
        assert_eq!(conns[0].tcp_us, 900);
        assert_eq!(remotes[0].ip, "10.0.0.1");
        assert_eq!(remotes[0].conns, 1);
        assert_eq!(remotes[0].errors, 1);
    }

    #[test]
    fn heatmap_counts_every_request_in_the_window() {
        let rows: Vec<ReqRow> = (0..100)
            .map(|i| req(i * 10, "GetObject", 200, 1 + (i as u64 % 7) * 30))
            .collect();
        let h = ReqHeatmap::build(&rows, 1.0);
        assert_eq!(h.counts.iter().map(|c| *c as usize).sum::<usize>(), 100);
        assert!(h.max >= 1);
    }
}
