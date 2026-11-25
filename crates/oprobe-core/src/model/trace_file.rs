//! A production trace as a [`Run`]: the `oprobe.trace/9` file an application writes with the
//! embedded `oprobe-trace` layer (`Handle::save`, `Snapshot::save`), built into a report by the
//! same builder as a run ([`super::build_run`]), and marked for what it is (design v6 §2: the
//! workbench opens it like any other profile).
//!
//! What differs from a run oprobe drove:
//! - `run.source` is `trace`; there is no workload (no preset, mix or dataset) and nothing to
//!   replay, so `oprobe prove` refuses it;
//! - the operator is what the layer saw: the service's scheme, name (bucket), root and HTTP
//!   client posture;
//! - every OpenDAL call is recorded as an operation linked to the requests it issued (keys
//!   hashed, design v7 B1), so the summary, verbs, hot keys, prefixes and over-read read them
//!   as a run's do;
//! - only the findings the trace can support are kept ([`TRACE_FINDINGS`], plus
//!   [`OP_TRACE_FINDINGS`] when it has operation rows). The ones that need a load oprobe varied
//!   or its own client (bandwidth cap, client bound) are not derived; the ones that judge the
//!   operator's `read.concurrent` / `write.concurrent` read what its requests did instead of
//!   oprobe's defaults.

use std::path::Path;
use std::time::{Duration, SystemTime};

use super::report::*;
use super::{Run, RunId};
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

/// The findings a trace with operation rows keeps besides [`TRACE_FINDINGS`]: they read
/// operations (ranges per read, parts per write, repeated keys).
pub const OP_TRACE_FINDINGS: &[&str] = &["small_ranges", "slow_multipart", "repeated_reads"];

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

fn is_non_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let o = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// The endpoint a trace's requests went to when it is not the service's default (K8): remotes
/// on loopback, private, link-local or CGNAT/Tailscale addresses (a lab, MinIO, a gateway), as
/// `http://ip:port` (`https` when the requests used TLS). `None` otherwise: a public address is
/// taken for the service's own endpoint, which `oprobe run URL` finds by itself.
pub fn custom_endpoint(r: &Report) -> Option<String> {
    let addrs: Vec<std::net::SocketAddr> = r
        .network
        .remotes
        .iter()
        .filter_map(|x| x.remote.parse().ok())
        .collect();
    if addrs.is_empty() || !addrs.iter().all(|a| is_non_public_ip(a.ip())) {
        return None;
    }
    let a = addrs
        .iter()
        .copied()
        .find(|a| a.is_ipv4())
        .unwrap_or(addrs[0]);
    let tls = !r.network.tls.version.is_empty() || a.port() == 443;
    Some(format!("{}://{a}", if tls { "https" } else { "http" }))
}

/// The run that profiles the service a trace recorded, to prove a remedy on (`oprobe run
/// s3://bucket/root`, with `--set options.endpoint=…` when the requests went to a custom
/// endpoint, and `--preset …` when the recorded operations match a non-default preset);
/// `None` when the file does not name the service.
pub fn run_command(r: &Report) -> Option<String> {
    let url = &r.spec.operator_ref;
    if url.is_empty() || url.starts_with("unknown://") {
        return None;
    }
    let mut c = format!("oprobe run {}", super::spec::shell_quote(url));
    if let Some(e) = custom_endpoint(r) {
        c += &format!(
            " --set {}",
            super::spec::shell_quote(&format!("options.endpoint={e}"))
        );
    }
    if let Some(preset) = inferred_preset(r) {
        c += &format!(" --preset {preset}");
    }
    Some(c)
}

fn inferred_preset(r: &Report) -> Option<&'static str> {
    let total: u64 = r.verbs.iter().map(|v| v.count).sum();
    if total == 0 {
        return None;
    }
    let writes: u64 = r
        .verbs
        .iter()
        .filter(|v| {
            v.verb.starts_with("write")
                || matches!(
                    v.verb.as_str(),
                    "PutObject" | "UploadPart" | "CreateMultipart"
                )
        })
        .map(|v| v.count)
        .sum();
    let meta: u64 = r
        .verbs
        .iter()
        .filter(|v| {
            matches!(
                v.verb.as_str(),
                "list" | "stat" | "ListObjectsV2" | "HeadObject"
            )
        })
        .map(|v| v.count)
        .sum();
    let ranges: u64 = r
        .verbs
        .iter()
        .filter(|v| v.verb == "read.range" || v.verb.contains("parquet"))
        .map(|v| v.count)
        .sum();
    let multipart = r
        .verbs
        .iter()
        .any(|v| v.verb.contains("multipart") || v.verb == "UploadPart")
        || r.apis
            .iter()
            .any(|a| a.api == crate::trace::Api::UploadPart && a.count > 0);
    let max_bpo = r
        .verbs
        .iter()
        .map(|v| v.bytes_per_op)
        .fold(0.0_f64, f64::max);
    if writes == 0 && meta.saturating_mul(10) >= total.saturating_mul(7) {
        Some("list-stat")
    } else if writes == 0 && ranges.saturating_mul(2) >= total {
        Some("range-reads")
    } else if multipart || max_bpo >= (4 << 20) as f64 {
        Some("large-objects")
    } else {
        None
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
fn sequential(run: &Run, api: crate::trace::Api) -> bool {
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
    let snapshot = std::mem::take(&mut file.snapshot);
    if let Some(svc) = file.service.as_ref() {
        profile.operator.http.version = svc.http_version;
        profile.operator.http.keep_alive = svc.keep_alive;
        if let Some(limit) = svc.max_inflight_count {
            profile.operator.layers.concurrent_limit = Some(crate::operator::ConcurrentLimit {
                http: Some(limit),
                operations: None,
            });
        }
    }
    if profile.operator.http.version == crate::net::HttpVersion::Auto
        && snapshot.connections.http2_requests > 0
        && snapshot.connections.http1_requests == 0
    {
        profile.operator.http.version = crate::net::HttpVersion::H2;
    }
    let knobs = crate::config::Knobs::of(&profile);
    let ended_at = parse_rfc3339(&file.created_at).unwrap_or_else(SystemTime::now);
    let started_at = Duration::try_from_secs_f64(snapshot.elapsed_s.max(0.0))
        .ok()
        .and_then(|d| ended_at.checked_sub(d))
        .unwrap_or(ended_at);
    let run_id = RunId::at(started_at, 0);
    let mut warnings = Vec::new();
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
    // Findings from the operations (or from aggregates alone when saved without rows).
    let mut findings = if has_ops {
        let view = observed_view(&run);
        let mut f = crate::analysis::findings::derive(&view);
        f.retain(|x| match x.code.as_str() {
            // These judge `read.concurrent` / `write.concurrent`, which the trace does not
            // know: keep them only when the requests really were sequential.
            "small_ranges" => sequential(&run, crate::trace::Api::GetObject),
            "slow_multipart" => sequential(&run, crate::trace::Api::UploadPart),
            _ => true,
        });
        f
    } else {
        let mut f = crate::analysis::findings::derive(&run);
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
    let run_cmd = run_command(&run.report);
    for f in &mut findings {
        f.remedy.next = Some(next_command(&file_ref, run_cmd.as_deref(), f));
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
/// (`run`, the service's concrete [`run_command`], then `oprobe prove`), never on the trace.
fn next_command(file: &str, run: Option<&str>, f: &Finding) -> String {
    match f.code.as_str() {
        "errors" if f.remedy.path.is_none() => format!("oprobe show {file} --by api"),
        "invalid_run" => format!("oprobe show {file} --detail"),
        // A file that does not name its service: the user names the operator to drive.
        _ => run.map_or_else(|| "oprobe run URL".to_owned(), str::to_owned),
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
            http_version: crate::trace::HttpVersion::Auto,
            keep_alive: true,
            max_inflight_count: None,
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

    #[test]
    fn custom_endpoint_handles_multi_addr_private_and_cgnat() {
        let report_with = |addrs: &[&str], tls: &str| {
            let p = crate::config::Profile::from_url("s3://bkt");
            let k = crate::config::Knobs::of(&p);
            let mut run = crate::model::build_run(crate::model::ReportInput {
                run_id: &RunId::new(),
                target_name: None,
                profile: &p,
                knobs: &k,
                snapshot: Default::default(),
                trace: Default::default(),
                cleanup: Default::default(),
                warnings: Vec::new(),
                started_at: SystemTime::now(),
                ended_at: SystemTime::now(),
                spec: None,
            });
            run.report.network.tls.version = tls.to_owned();
            run.report.network.remotes = addrs
                .iter()
                .map(|a| RemoteStats {
                    remote: (*a).to_owned(),
                    requests_count: 1,
                    ..Default::default()
                })
                .collect();
            run.report
        };
        assert_eq!(
            custom_endpoint(&report_with(&["[::1]:9000", "127.0.0.1:9000"], "")),
            Some("http://127.0.0.1:9000".into())
        );
        assert_eq!(
            custom_endpoint(&report_with(&["10.0.1.2:9000", "10.0.1.3:9000"], "TLSv1.3")),
            Some("https://10.0.1.2:9000".into())
        );
        assert_eq!(
            custom_endpoint(&report_with(&["100.80.1.2:9000"], "")),
            Some("http://100.80.1.2:9000".into())
        );
        assert_eq!(
            custom_endpoint(&report_with(
                &["52.216.1.2:443", "52.216.1.3:443"],
                "TLSv1.3"
            )),
            None
        );
    }
}
