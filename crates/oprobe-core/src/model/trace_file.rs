//! A production trace as a [`Run`]: the `oprobe.trace/7` file an application writes with the
//! embedded `oprobe-trace` layer (`Handle::save`, `Snapshot::save`), built into a report by the
//! same builder as a run ([`super::build_run`]), and marked for what it is (design v6 §2: the
//! workbench opens it like any other profile).
//!
//! What differs from a run oprobe drove:
//! - `run.source` is `trace`; there is no workload (no preset, mix or dataset) and nothing to
//!   replay, so `oprobe prove` refuses it;
//! - the operator is what the layer saw: the service's scheme, name (bucket) and root;
//! - an `oprobe.trace/7` file has the application's operations (each OpenDAL call, linked to
//!   the requests it issued; keys hashed, design v7 B1), so the summary, verbs, hot keys,
//!   prefixes and over-read read them as a run's do. An `oprobe.trace/6` file has requests
//!   only: each request stands in for one operation in the summary (count, failures, latency),
//!   and the report says so;
//! - only the findings the trace can support are kept ([`TRACE_FINDINGS`], plus
//!   [`OP_TRACE_FINDINGS`] when it has operations). The ones that need a load oprobe varied or
//!   its own client (bandwidth cap, client bound) are not derived; the ones that judge the
//!   operator's `read.concurrent` / `write.concurrent` read what its requests did instead of
//!   oprobe's defaults.

use std::path::Path;
use std::time::{Duration, SystemTime};

use super::report::*;
use super::{OpRow, ReqRow, Run, RunId};
use crate::trace::{ServiceRecord, Snapshot, TraceFile};

/// The findings a production trace keeps (they read request rows and aggregates only).
pub const TRACE_FINDINGS: &[&str] = &[
    "errors",
    "throttling",
    "rate_limit",
    "key_limit",
    "hung_requests",
    "tail_stragglers",
    "cold_connections",
    "invalid_run",
];

/// The findings a trace with operations keeps besides [`TRACE_FINDINGS`]: they read operations
/// (ranges per read, parts per write, repeated keys).
pub const OP_TRACE_FINDINGS: &[&str] = &["small_ranges", "slow_multipart", "repeated_reads"];

/// The warning every report of a trace without operation rows carries.
pub const PER_REQUEST_WARNING: &str = "production trace: oprobe did not drive this traffic and the application's requests are not grouped into operations, so each request counts as one operation";

/// The operator URL of the service a trace recorded (`s3://bucket/root`); `unknown://` when
/// the file does not say (a bare `Snapshot::save`).
pub fn operator_url(s: Option<&ServiceRecord>) -> String {
    let Some(s) = s else {
        return "unknown://".into();
    };
    let root = s.root.trim_matches('/');
    match s.scheme.as_str() {
        "memory" => "memory://".into(),
        "fs" => format!("fs:///{root}"),
        scheme => {
            let mut u = format!("{scheme}://{}", s.name);
            if !root.is_empty() {
                u.push('/');
                u.push_str(root);
            }
            u
        }
    }
}

/// `2026-11-22T10:00:00Z` as a time.
fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<u32>().ok();
    crate::util::utc_time(
        n(0..4)? as i64,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    )
}

/// The summary of a trace with no operations: every request stands in for one operation (a
/// throttled request is counted as throttled, not failed: a retry layer may have recovered it).
fn per_request_summary(s: &mut Snapshot, reqs: &[ReqRow], complete: bool) {
    let sum = |f: &dyn Fn(&crate::trace::ApiStats) -> u64| {
        s.apis.iter().fold(0u64, |n, a| n.saturating_add(f(a)))
    };
    let requests = sum(&|a| a.count);
    let throttled = sum(&|a| a.throttled);
    let failed = sum(&|a| a.errors.saturating_sub(a.throttled));
    let elapsed = s.elapsed_s.max(1e-3);
    let ratio = |n: u64| n as f64 / requests.max(1) as f64;
    let latency = if complete {
        let mut all: Vec<u64> = reqs.iter().map(|r| r.total_us).collect();
        crate::analysis::stats::exact_quantiles(&mut all)
    } else {
        None
    };
    let latency = latency.unwrap_or_else(|| {
        // Sampled rows over-represent the slow and failed requests: combine the exact
        // per-API aggregates instead (count-weighted; the maximum is exact).
        let mut q = crate::trace::Quantiles::default();
        for a in &s.apis {
            let t = &a.stages.total;
            let w = t.count as f64;
            q.count += t.count;
            q.mean += t.mean * w;
            q.p50 += t.p50 * w;
            q.p90 += t.p90 * w;
            q.p99 += t.p99 * w;
            q.p999 += t.p999 * w;
            q.max = q.max.max(t.max);
        }
        let n = q.count.max(1) as f64;
        q.mean /= n;
        q.p50 /= n;
        q.p90 /= n;
        q.p99 /= n;
        q.p999 /= n;
        q
    });
    let m = &mut s.summary;
    m.ops = requests;
    m.failed_ops = failed;
    m.ops_per_s = requests.saturating_sub(failed) as f64 / elapsed;
    m.attempted_ops_per_s = requests as f64 / elapsed;
    m.error_rate = ratio(failed);
    m.throttled_request_ratio = ratio(throttled);
    m.throttled_op_ratio = ratio(throttled);
    m.latency = latency;
    s.resilience.failed_ops = failed;
    if requests > 0 {
        s.bytes.request_amplification = 1.0;
    }
    // One "operation" per storage API, so where the time went and the per-operation table
    // read the requests (backend time is the request's, less its wait for a slot).
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    s.verbs = s
        .apis
        .iter()
        .filter(|a| a.count > 0)
        .map(|a| {
            let label = a.api.label();
            let t = &a.stages.total;
            let latency = complete
                .then(|| {
                    let mut v: Vec<u64> = reqs
                        .iter()
                        .filter(|r| r.api == label)
                        .map(|r| r.total_us)
                        .collect();
                    crate::analysis::stats::exact_quantiles(&mut v)
                })
                .flatten()
                .unwrap_or(*t);
            let bytes = a.bytes_up.saturating_add(a.bytes_down);
            let n = a.count as f64;
            crate::trace::VerbStats {
                verb: label.to_owned(),
                count: a.count,
                errors: a.errors.saturating_sub(a.throttled),
                throttled: a.throttled,
                ops_per_s: n / elapsed,
                mib_per_s: mib(bytes) / elapsed,
                latency,
                breakdown: crate::trace::OpBreakdown {
                    queue_ms: a.stages.queue.mean,
                    backend_ms: (t.mean - a.stages.queue.mean).max(0.0),
                    ..Default::default()
                },
                requests_per_op: 1.0,
                bytes_per_op: bytes as f64 / n,
                retries: 0,
            }
        })
        .collect();
}

/// The request rows as one-request operations, for the findings that read operation latency
/// (stragglers): a view, never stored.
fn per_request_view(run: &Run) -> Run {
    let mut reqs = run.reqs.clone();
    let ops: Vec<OpRow> = reqs
        .iter_mut()
        .enumerate()
        .map(|(i, q)| {
            let id = i as u64 + 1;
            q.op_id = id;
            q.attempt = 1;
            let failed = q.is_error() && q.status != 404 && !q.is_throttle();
            let mut o = crate::trace::log::op_row(
                id,
                "",
                String::new(),
                q.t_us,
                q.total_us,
                q.bytes_up.saturating_add(q.bytes_down),
                failed.then(|| {
                    q.error
                        .clone()
                        .unwrap_or_else(|| format!("status {}", q.status))
                }),
                q.is_throttle(),
                0,
                [q.queue_us, 0, q.total_us.saturating_sub(q.queue_us), 0, 0],
                1,
            );
            o.verb = std::borrow::Cow::Owned(q.api.to_string());
            o
        })
        .collect();
    Run {
        report: run.report.clone(),
        ops,
        reqs,
    }
}

/// The run as the findings read a trace with operations: the operations it observed stand in
/// for the workload mix (by verb, weighted by count), so the findings that apply to reads or
/// writes know there were some. A view, never stored: the report's workload stays empty.
fn observed_view(run: &Run) -> Run {
    let mut counts: std::collections::BTreeMap<&str, u64> = Default::default();
    for o in run.ops.iter().filter(|o| o.t_us >= 0) {
        let op = if o.verb.starts_with("read") {
            "read"
        } else if o.verb.starts_with("write.multipart") {
            "multipart_write"
        } else if o.verb.starts_with("write") {
            "write"
        } else {
            continue;
        };
        *counts.entry(op).or_default() += 1;
    }
    let mut view = Run {
        report: run.report.clone(),
        ops: run.ops.clone(),
        reqs: run.reqs.clone(),
    };
    view.report.workload.mix = counts
        .into_iter()
        .map(|(op, n)| MixEntry {
            weight: n.min(u64::from(u32::MAX)) as u32,
            op: op.to_owned(),
            ..Default::default()
        })
        .collect();
    view
}

/// Whether every operation issued its `api` requests one after another (no two overlapping in
/// time): what an operator with `concurrent = 1` does.
fn sequential(run: &Run, api: &str) -> bool {
    let mut by_op: std::collections::HashMap<u64, Vec<(i64, i64)>> = Default::default();
    for q in run.reqs.iter().filter(|q| q.op_id != 0 && q.api == api) {
        let start = q.t_us;
        by_op
            .entry(q.op_id)
            .or_default()
            .push((start, start.saturating_add(q.total_us as i64)));
    }
    by_op.into_values().all(|mut v| {
        v.sort_unstable();
        v.windows(2).all(|w| w[1].0 >= w[0].1)
    })
}

/// The longest recording a trace may span (ten years): beyond it the file is corrupt.
const MAX_ELAPSED_S: f64 = 10.0 * 365.25 * 86_400.0;

/// The largest integer a trace aggregate may hold (2^53: exact as a float, and far beyond any
/// real count or byte total), so sums and conversions cannot overflow.
const MAX_COUNT: u64 = 1 << 53;

/// The largest float a trace aggregate may hold (rates, means, quantiles in ms).
const MAX_FLOAT: f64 = 1e15;

/// The largest time offset or duration of one row (µs): the longest recording.
const MAX_ROW_US: u64 = (MAX_ELAPSED_S * 1e6) as u64;

/// The most bytes one request or operation may move (1 PiB).
const MAX_ROW_BYTES: u64 = 1 << 50;

/// Check that every value of a trace file is in range: finite, non-negative where it must be,
/// and within sane bounds, so building the report cannot overflow or panic. The error names
/// the first value out of range.
pub fn validate(s: &Snapshot, rows: &crate::trace::TraceRows) -> Result<(), String> {
    if !(s.elapsed_s.is_finite() && (0.0..=MAX_ELAPSED_S).contains(&s.elapsed_s)) {
        return Err(format!(
            "snapshot.elapsed_s = {} is not a duration between 0 and {MAX_ELAPSED_S} s",
            s.elapsed_s
        ));
    }
    let doc = serde_json::to_value(s).map_err(|e| e.to_string())?;
    check_value("snapshot", &doc)?;
    for (i, r) in rows.reqs.iter().enumerate() {
        let us = [
            r.queue_us, r.dns_us, r.tcp_us, r.proxy_us, r.tls_us, r.send_us,
        ]
        .into_iter()
        .chain([r.ttfb_us, r.body_us, r.stall_us, r.total_us]);
        if r.t_us.unsigned_abs() > MAX_ROW_US || us.into_iter().any(|v| v > MAX_ROW_US) {
            return Err(format!("rows.reqs[{i}]: a time is out of range"));
        }
        if r.bytes_up > MAX_ROW_BYTES || r.bytes_down > MAX_ROW_BYTES {
            return Err(format!("rows.reqs[{i}]: a byte count is out of range"));
        }
        if r.retry_after_ms.is_some_and(|v| v > MAX_ROW_US / 1000) {
            return Err(format!("rows.reqs[{i}].retry_after_ms is out of range"));
        }
    }
    for (i, o) in rows.ops.iter().enumerate() {
        let b = &o.breakdown;
        let us = [
            b.queue_us,
            b.cache_us,
            b.backend_us,
            b.retry_us,
            b.client_us,
        ];
        if o.t_us.unsigned_abs() > MAX_ROW_US
            || o.latency_us > MAX_ROW_US
            || us.into_iter().any(|v| v > MAX_ROW_US)
        {
            return Err(format!("rows.ops[{i}]: a time is out of range"));
        }
        if o.bytes > MAX_ROW_BYTES {
            return Err(format!("rows.ops[{i}].bytes is out of range"));
        }
    }
    Ok(())
}

/// Every number of an aggregate in range; the quantiles of a latency non-negative.
fn check_value(at: &str, v: &serde_json::Value) -> Result<(), String> {
    use serde_json::Value;
    match v {
        Value::Number(n) => {
            let ok = if let Some(u) = n.as_u64() {
                u <= MAX_COUNT
            } else if let Some(i) = n.as_i64() {
                i.unsigned_abs() <= MAX_COUNT
            } else {
                n.as_f64()
                    .is_some_and(|f| f.is_finite() && f.abs() <= MAX_FLOAT)
            };
            if !ok {
                return Err(format!("{at} = {n} is out of range"));
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                check_value(&format!("{at}[{i}]"), x)?;
            }
        }
        Value::Object(o) => {
            let quantiles = o.contains_key("p50_ms") && o.contains_key("p99_ms");
            for (k, x) in o {
                let at = format!("{at}.{k}");
                if quantiles && x.as_f64().is_some_and(|f| f < 0.0) {
                    return Err(format!("{at} = {x} is negative"));
                }
                check_value(&at, x)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Build the run of a trace file `path` (already read). Fails, as a usage error, on a file
/// whose values are out of range ([`validate`]).
pub fn trace_run(mut file: TraceFile, path: &Path) -> Result<Run, crate::errors::OprobeError> {
    let has_rows = file.has_rows();
    let rows = file.take_rows();
    validate(&file.snapshot, &rows).map_err(|why| {
        crate::errors::OprobeError::new(
            crate::errors::ErrorCode::UsageInvalidArgument,
            format!("trace file `{}` is corrupt: {why}", path.display()),
        )
        .with_hint("save the profile again with oprobe-trace's `Handle::save`")
    })?;
    let has_ops = !rows.ops.is_empty();
    let url = operator_url(file.service.as_ref());
    let mut profile = crate::config::Profile::from_url(&url);
    profile.name = None;
    let knobs = crate::config::Knobs::of(&profile);
    let ended_at = parse_rfc3339(&file.created_at).unwrap_or_else(SystemTime::now);
    let mut snapshot = std::mem::take(&mut file.snapshot);
    let started_at = Duration::try_from_secs_f64(snapshot.elapsed_s.max(0.0))
        .ok()
        .and_then(|d| ended_at.checked_sub(d))
        .unwrap_or(ended_at);
    let run_id = RunId::at(started_at, 0);
    let mut warnings = Vec::new();
    if !has_ops {
        let requests = snapshot
            .apis
            .iter()
            .fold(0u64, |n, a| n.saturating_add(a.count));
        let complete = !rows.sampled() && rows.reqs.len() as u64 == requests;
        per_request_summary(&mut snapshot, &rows.reqs, complete);
        warnings.push(PER_REQUEST_WARNING.to_owned());
    }
    if !has_rows {
        warnings.push(
            "the trace holds aggregates only (a snapshot saved without its requests): findings that read requests are not derived"
                .to_owned(),
        );
    }
    let mut run = super::build_run(super::ReportInput {
        run_id: &run_id,
        target_name: None,
        profile: &profile,
        knobs: &knobs,
        snapshot,
        trace: rows,
        cleanup: Default::default(),
        warnings,
        started_at,
        ended_at,
        spec: Some(super::RunSpec::new(&profile, url.clone())),
    });
    let r = &mut run.report;
    r.run.source = RunSource::Trace;
    // The file as it was given: what `oprobe show` takes to open it again (`show_ref`).
    r.run.label = Some(path.display().to_string());
    r.run.oprobe_version = file.oprobe_trace_version.clone();
    // The layer records the remote addresses (`network.remotes`), not the endpoint URL: no
    // default endpoint is guessed from the scheme.
    r.target.endpoint = None;
    // The machine that recorded the trace is not in the file; this one only opened it.
    r.run.host = HostInfo::default();
    // No workload: oprobe drove nothing (the window is how long the layer recorded).
    r.workload = WorkloadInfo {
        duration_s: r.run.window_s,
        ..Default::default()
    };
    if file.service.is_none() {
        r.target.display = "unknown service".into();
        r.target.scheme = "unknown".into();
    }
    // Findings from the operations; a trace without operations reads them one request per
    // operation.
    let mut findings = if has_ops {
        let view = observed_view(&run);
        let mut f = crate::analysis::findings::derive(&view);
        f.retain(|x| match x.code.as_str() {
            // These judge `read.concurrent` / `write.concurrent`, which the trace does not
            // know: keep them only when the requests really were sequential.
            "small_ranges" => sequential(&run, "GetObject"),
            "slow_multipart" => sequential(&run, "UploadPart"),
            _ => true,
        });
        f
    } else {
        let view = per_request_view(&run);
        let mut f = crate::analysis::findings::derive(&view);
        for x in &mut f {
            x.evidence.op_ids.clear();
        }
        f
    };
    findings.retain(|f| {
        TRACE_FINDINGS.contains(&f.code.as_str())
            || (has_ops && OP_TRACE_FINDINGS.contains(&f.code.as_str()))
    });
    let file_ref = super::spec::shell_quote(&path.display().to_string());
    for f in &mut findings {
        f.remedy.next = Some(next_command(&file_ref, f));
    }
    run.report.findings = findings;
    let breached = !run.report.verdict.sla.passed;
    super::build::finish(&mut run, breached);
    for f in &mut run.report.findings {
        if f.code == "invalid_run" {
            f.remedy.next = Some(format!("oprobe show {file_ref} --detail"));
        }
    }
    Ok(run)
}

/// The next command of a finding of a trace: a remedy is proven on a run oprobe drives
/// (`oprobe run OPERATOR`, then `oprobe prove`), never on the trace.
fn next_command(file: &str, f: &Finding) -> String {
    match f.code.as_str() {
        "errors" if f.remedy.path.is_none() => format!("oprobe show {file} --by api"),
        "invalid_run" => format!("oprobe show {file} --detail"),
        // The service as the layer saw it has no endpoint or credentials: the user names the
        // operator to drive.
        _ => "oprobe run OPERATOR".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_urls_name_the_service() {
        let s = |scheme: &str, name: &str, root: &str| ServiceRecord {
            scheme: scheme.into(),
            name: name.into(),
            root: root.into(),
        };
        assert_eq!(operator_url(None), "unknown://");
        assert_eq!(operator_url(Some(&s("s3", "bkt", "/"))), "s3://bkt");
        assert_eq!(operator_url(Some(&s("s3", "bkt", "/a/b/"))), "s3://bkt/a/b");
        assert_eq!(operator_url(Some(&s("memory", "0x1", "/"))), "memory://");
        assert_eq!(operator_url(Some(&s("fs", "", "/tmp/x/"))), "fs:///tmp/x");
    }

    #[test]
    fn rfc3339_parses() {
        let t = parse_rfc3339("2026-11-22T10:00:05Z").unwrap();
        assert_eq!(crate::util::fmt_rfc3339(t), "2026-11-22T10:00:05Z");
        assert!(parse_rfc3339("garbage").is_none());
    }
}
