//! The per-operation waterfall (design v5 §9 "Trace drilldown", U8): one operation's requests
//! on its own time axis, each split into its stages (queue, connect, upload, server, download,
//! stall), the backoff between a failed attempt and its retry, the critical path, and how the
//! operation compares with a typical one of the same verb.

use crate::model::{OpRow, ReqRow};

/// One stage of a request bar: its name ([`crate::ui::kit::stage_token`] colours it) and length.
#[derive(Clone, Debug, PartialEq)]
pub struct Seg {
    pub stage: &'static str,
    pub us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    Request,
    /// The wait between an attempt and its retry.
    Backoff,
}

/// One row of the waterfall.
#[derive(Clone, Debug, PartialEq)]
pub struct Bar {
    pub kind: BarKind,
    /// The request's index in the run's rows (`None` for a backoff).
    pub req: Option<u32>,
    pub label: String,
    /// Start, µs after the operation's start.
    pub start_us: u64,
    pub len_us: u64,
    pub segs: Vec<Seg>,
    /// On the operation's critical path.
    pub critical: bool,
    pub failed: bool,
}

/// A typical operation of the same verb, for "compare with a typical op".
#[derive(Clone, Debug, PartialEq)]
pub struct Typical {
    pub op_index: u32,
    pub latency_us: u64,
    pub requests: u32,
    /// `op latency / typical latency`.
    pub ratio: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Waterfall {
    pub op_id: u64,
    pub verb: String,
    pub key: String,
    pub total_us: u64,
    pub bars: Vec<Bar>,
    pub typical: Option<Typical>,
}

/// The stages of a request, in order, with their lengths (zero-length ones left out).
pub fn segs(r: &ReqRow) -> Vec<Seg> {
    let parts: [(&'static str, u64); 6] = [
        ("queue", r.queue_us),
        ("connect", r.connect_us()),
        ("upload", r.send_us),
        ("server", r.ttfb_us),
        ("download", r.body_us),
        ("stall", r.stall_us),
    ];
    let mut out: Vec<Seg> = parts
        .into_iter()
        .filter(|(_, us)| *us > 0)
        .map(|(stage, us)| Seg { stage, us })
        .collect();
    // What the stages do not account for (client time inside the request).
    let sum: u64 = out.iter().map(|s| s.us).sum();
    if r.total_us > sum {
        out.push(Seg {
            stage: "client",
            us: r.total_us - sum,
        });
    }
    out
}

/// The waterfall of operation `op` with its requests (`reqs`: index into the run's rows and the
/// row, in any order); `all_ops` gives the typical operation of the same verb.
pub fn build(op: &OpRow, reqs: &[(u32, &ReqRow)], all_ops: &[OpRow]) -> Waterfall {
    let mut sorted: Vec<(u32, &ReqRow)> = reqs.to_vec();
    sorted.sort_by_key(|(_, r)| (r.t_us, r.attempt));
    let t0 = sorted
        .iter()
        .map(|(_, r)| r.t_us)
        .min()
        .unwrap_or(op.t_us)
        .min(op.t_us);
    let mut bars: Vec<Bar> = Vec::new();
    for (k, (i, r)) in sorted.iter().enumerate() {
        // A retry: the gap since the previous attempt of the same API ended is its backoff.
        if r.attempt > 1
            && let Some((_, p)) = sorted[..k]
                .iter()
                .rev()
                .find(|(_, p)| p.api == r.api && p.attempt + 1 == r.attempt)
        {
            let end = p.t_us + p.total_us as i64;
            if r.t_us > end {
                bars.push(Bar {
                    kind: BarKind::Backoff,
                    req: None,
                    label: match p.retry_after_ms {
                        Some(ms) => format!("backoff · Retry-After {ms} ms"),
                        None => "backoff".into(),
                    },
                    start_us: (end - t0).max(0) as u64,
                    len_us: (r.t_us - end) as u64,
                    segs: vec![Seg {
                        stage: "retry",
                        us: (r.t_us - end) as u64,
                    }],
                    critical: false,
                    failed: false,
                });
            }
        }
        bars.push(Bar {
            kind: BarKind::Request,
            req: Some(*i),
            label: format!(
                "{}{} · {}",
                r.api,
                if r.attempt > 1 {
                    format!(" (attempt {})", r.attempt)
                } else {
                    String::new()
                },
                if r.status == 0 {
                    "no response".to_owned()
                } else {
                    r.status.to_string()
                }
            ),
            start_us: (r.t_us - t0).max(0) as u64,
            len_us: r.total_us,
            segs: segs(r),
            critical: false,
            failed: r.is_error(),
        });
    }
    mark_critical(&mut bars);
    let total_us = bars
        .iter()
        .map(|b| b.start_us + b.len_us)
        .max()
        .unwrap_or(0)
        .max(op.latency_us);
    Waterfall {
        op_id: op.op_id,
        verb: op.verb.to_string(),
        key: op.key.clone(),
        total_us,
        bars,
        typical: typical(op, all_ops),
    }
}

/// The critical path: from the bar that ends last, back through the bar that ended last before
/// each one started (requests that ran in parallel and ended earlier are off the path).
fn mark_critical(bars: &mut [Bar]) {
    let Some(mut cur) = (0..bars.len()).max_by_key(|&i| (bars[i].start_us + bars[i].len_us, i))
    else {
        return;
    };
    loop {
        bars[cur].critical = true;
        let start = bars[cur].start_us;
        let prev = (0..bars.len())
            .filter(|&i| i != cur && bars[i].start_us + bars[i].len_us <= start)
            .max_by_key(|&i| (bars[i].start_us + bars[i].len_us, i));
        match prev {
            Some(p) => cur = p,
            None => break,
        }
    }
}

/// The median-latency successful operation of the same verb.
fn typical(op: &OpRow, all: &[OpRow]) -> Option<Typical> {
    let mut same: Vec<(u32, &OpRow)> = all
        .iter()
        .enumerate()
        .filter(|(_, o)| o.verb == op.verb && o.ok)
        .map(|(i, o)| (i as u32, o))
        .collect();
    if same.len() < 3 {
        return None;
    }
    same.sort_by_key(|(_, o)| o.latency_us);
    let (i, t) = same[same.len() / 2];
    Some(Typical {
        op_index: i,
        latency_us: t.latency_us,
        requests: t.requests,
        ratio: op.latency_us as f64 / t.latency_us.max(1) as f64,
    })
}

/// The operation to drill into for a request, a time window or a p99 point: the slowest
/// operation that overlaps `[from_us, to_us]` (whole run when `None`).
pub fn slowest_op(ops: &[OpRow], window_us: Option<(i64, i64)>) -> Option<u32> {
    ops.iter()
        .enumerate()
        .filter(|(_, o)| match window_us {
            Some((a, b)) => o.t_us <= b && o.t_us + o.latency_us as i64 >= a,
            None => true,
        })
        .max_by_key(|(_, o)| o.latency_us)
        .map(|(i, _)| i as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(t: i64, attempt: u32, status: u16, ttfb: u64, total: u64) -> ReqRow {
        ReqRow {
            t_us: t,
            op_id: 7,
            attempt,
            status,
            send_us: 0,
            ttfb_us: ttfb,
            body_us: 0,
            total_us: total,
            ..crate::ui::model::requests::tests::req(0, "GetObject", status, 1)
        }
    }

    fn op(latency: u64, verb: &'static str, ok: bool) -> OpRow {
        OpRow {
            op_id: 7,
            verb: verb.into(),
            key: String::new(),
            t_us: 0,
            latency_us: latency,
            bytes: 0,
            ok,
            error_code: None,
            error: None,
            throttled: false,
            attempts: 2,
            breakdown: Default::default(),
            requests: 2,
            await_tree: None,
        }
    }

    #[test]
    fn a_retry_shows_its_backoff_and_the_critical_path() {
        let a = req(1_000, 1, 503, 800, 1_000);
        let b = req(52_000, 2, 200, 9_000, 10_000);
        let o = OpRow {
            t_us: 1_000,
            ..op(61_000, "read", true)
        };
        let ops = vec![op(10_000, "read", true), op(11_000, "read", true), op(12_000, "read", true)];
        let w = build(&o, &[(5, &b), (4, &a)], &ops);
        assert_eq!(w.bars.len(), 3);
        assert_eq!(w.bars[0].req, Some(4));
        assert!(w.bars[0].failed);
        assert_eq!(w.bars[1].kind, BarKind::Backoff);
        assert_eq!(w.bars[1].start_us, 1_000);
        assert_eq!(w.bars[1].len_us, 50_000);
        assert_eq!(w.bars[2].label, "GetObject (attempt 2) · 200");
        assert!(w.bars.iter().all(|b| b.critical));
        assert_eq!(w.total_us, 61_000);
        let t = w.typical.unwrap();
        assert_eq!(t.latency_us, 11_000);
        assert!((t.ratio - 61.0 / 11.0).abs() < 1e-9);
        // The stages of a request add up to its total.
        let s: u64 = w.bars[2].segs.iter().map(|s| s.us).sum();
        assert_eq!(s, 10_000);
    }

    #[test]
    fn parallel_requests_that_end_early_are_off_the_critical_path() {
        let a = req(0, 1, 200, 100, 1_000);
        let b = ReqRow {
            api: "HeadObject".into(),
            ..req(0, 1, 200, 100, 3_000)
        };
        let c = ReqRow {
            api: "PutObject".into(),
            ..req(3_000, 1, 200, 100, 2_000)
        };
        let w = build(&op(5_000, "write", true), &[(0, &a), (1, &b), (2, &c)], &[]);
        let crit: Vec<bool> = w.bars.iter().map(|b| b.critical).collect();
        assert_eq!(crit, vec![false, true, true]);
        assert!(w.typical.is_none());
        assert_eq!(slowest_op(&[op(5, "a", true), op(9, "a", true)], None), Some(1));
    }
}
