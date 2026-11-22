//! A production trace as a [`Run`]: the `oprobe.trace/6` file an application writes with the
//! embedded `oprobe-trace` layer (`Handle::save`, `Snapshot::save`), built into a report by the
//! same builder as a run ([`super::build_run`]), and marked for what it is (design v6 §2: the
//! workbench opens it like any other profile).
//!
//! What differs from a run oprobe drove:
//! - `run.source` is `trace`; there is no workload (no preset, mix or dataset) and nothing to
//!   replay, so `oprobe prove` refuses it;
//! - the operator is what the layer saw: the service's scheme, name (bucket) and root;
//! - an application's requests are not grouped into operations, so each request stands in for
//!   one operation in the summary (count, failures, latency), and the request rows have no
//!   operation rows (unless the trace has them);
//! - only the findings that request rows can support are kept: errors, throttling and its
//!   limit models, hung requests, stragglers and cold connections. The ones that need oprobe's
//!   operations or a load it varied (small ranges, slow multipart, repeated reads, bandwidth
//!   cap, client bound) are not derived.

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
    let requests: u64 = s.apis.iter().map(|a| a.count).sum();
    let throttled: u64 = s.apis.iter().map(|a| a.throttled).sum();
    let failed = s
        .apis
        .iter()
        .map(|a| a.errors.saturating_sub(a.throttled))
        .sum::<u64>();
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
            let bytes = a.bytes_up + a.bytes_down;
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
                q.bytes_up + q.bytes_down,
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

/// Build the run of a trace file `path` (already read).
pub fn trace_run(file: TraceFile, path: &Path) -> Run {
    let rows = file.rows();
    let has_ops = !rows.ops.is_empty();
    let has_rows = file.has_rows();
    let url = operator_url(file.service.as_ref());
    let mut profile = crate::config::Profile::from_url(&url);
    profile.name = None;
    let knobs = crate::config::Knobs::of(&profile);
    let ended_at = parse_rfc3339(&file.created_at).unwrap_or_else(SystemTime::now);
    let mut snapshot = file.snapshot.clone();
    let started_at = ended_at
        .checked_sub(Duration::from_secs_f64(snapshot.elapsed_s.max(0.0)))
        .unwrap_or(ended_at);
    let run_id = RunId::at(started_at, 0);
    let mut warnings = Vec::new();
    if !has_ops {
        let requests: u64 = snapshot.apis.iter().map(|a| a.count).sum();
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
    // Findings from the requests alone; a trace without operations reads them one request
    // per operation.
    let mut findings = if has_ops {
        crate::analysis::findings::derive(&run)
    } else {
        let view = per_request_view(&run);
        let mut f = crate::analysis::findings::derive(&view);
        for x in &mut f {
            x.evidence.op_ids.clear();
        }
        f
    };
    findings.retain(|f| TRACE_FINDINGS.contains(&f.code.as_str()));
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
    run
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
