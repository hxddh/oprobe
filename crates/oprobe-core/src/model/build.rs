//! Build a [`Report`] (and the full [`Run`]) from what the engine measured.

use std::collections::HashMap;
use std::time::SystemTime;

use super::report::*;
use super::{ReqRow, Run, RunId};
use crate::analysis::diagnosis::{Severity, check_sla, diagnose};
use crate::analysis::hypothesis::HypothesisKind;
use crate::config::{Knobs, OpKind, Profile};
use crate::engine::CleanupSummary;
use crate::errors::exit;
use crate::trace::{Snapshot, TraceRows};

/// Everything a report is built from.
pub struct ReportInput<'a> {
    pub run_id: &'a RunId,
    /// Configured target name, when the command used one.
    pub target_name: Option<String>,
    pub profile: &'a Profile,
    /// Knobs the window ran with (the tuner's winner for `tune`).
    pub knobs: &'a Knobs,
    pub snapshot: Snapshot,
    pub trace: TraceRows,
    pub cleanup: CleanupSummary,
    pub warnings: Vec<String>,
    pub started_at: SystemTime,
    pub ended_at: SystemTime,
    /// The run's spec (`RunSpec::new` of the profile before the session normalised it, at the
    /// target as given); `None`: derived from `profile` and `target_name`.
    pub spec: Option<super::spec::RunSpec>,
}

/// Replace the recorder's histogram percentiles (summary and per verb) with exact ones from
/// the operation rows, so the report and its findings read the same estimator (design v4
/// §4). Only when the rows are complete: not sampled, the rows of the window are the
/// operations the recorder counted (within 1 %, for operations straddling the end), and no
/// open-loop arrival was censored: a missed arrival has no row, and its lower bound must stay
/// in the percentiles (T3: no coordinated omission), so the recorder's histogram is kept.
fn exact_latency(snapshot: &mut Snapshot, ops: &[super::OpRow], sampled: bool) {
    let censored = snapshot
        .open_loop
        .as_ref()
        .is_some_and(|ol| ol.censored_count > 0 || ol.missed > 0);
    if sampled || ops.is_empty() || censored {
        return;
    }
    let window_us = (snapshot.elapsed_s * 1e6).ceil() as i64;
    let rows: Vec<&super::OpRow> = ops
        .iter()
        .filter(|o| o.t_us >= 0 && o.t_us < window_us.max(1))
        .collect();
    let counted = snapshot.summary.ops;
    let close = |n: u64, m: u64| n.abs_diff(m) as f64 <= (m as f64 * 0.01).max(1.0);
    if !close(rows.len() as u64, counted) {
        return;
    }
    let mut all: Vec<u64> = rows.iter().map(|o| o.latency_us).collect();
    if let Some(q) = crate::analysis::stats::exact_quantiles(&mut all) {
        snapshot.summary.latency = q;
    }
    for v in &mut snapshot.verbs {
        let mut lat: Vec<u64> = rows
            .iter()
            .filter(|o| o.verb == v.verb.as_str())
            .map(|o| o.latency_us)
            .collect();
        if close(lat.len() as u64, v.count)
            && let Some(q) = crate::analysis::stats::exact_quantiles(&mut lat)
        {
            v.latency = q;
        }
    }
}

/// The warning a partial run carries in `report.warnings`.
fn partial_warning(p: &PartialInfo, configured_s: f64, suppressed: bool) -> String {
    format!(
        "partial run: {} after {:.1} s of the {:.1} s window{}",
        p.reason.describe(),
        p.measured_s,
        configured_s,
        if suppressed {
            "; performance findings are suppressed"
        } else {
            ""
        }
    )
}

/// Apply the run's validity to its findings, warnings, headline and verdict (after the
/// findings are derived, and again when [`mark_partial`] learns more).
pub(super) fn finish(run: &mut Run, breached: bool) {
    let findings = std::mem::take(&mut run.report.findings);
    let (findings, validity) = crate::analysis::findings::validity(&run.report, findings);
    let r = &mut run.report;
    r.findings = findings;
    r.warnings.retain(|w| !w.starts_with("partial run: "));
    if let Some(p) = &r.run.partial {
        let w = partial_warning(p, r.workload.duration_s, validity.suppress);
        r.warnings.push(w);
    }
    r.verdict.headline = headline(r);
    if let Some(why) = &validity.invalid {
        r.verdict.headline = format!("Run invalid: {why}.");
    } else if let (Some(p), true) = (&r.run.partial, validity.suppress) {
        r.verdict.headline = format!(
            "Partial run ({} after {:.1} s): too short to judge performance.",
            p.reason.describe(),
            p.measured_s
        );
    }
    let worst = r.worst_severity();
    // A partial run (a budget stop, an interruption) is worth a look, never a judgement: its
    // numbers cover part of the window (docs/reading-the-profile.md).
    r.verdict.level = if r.is_partial() {
        VerdictLevel::Review
    } else if (breached && r.verdict.sla.enforced) || worst == Severity::Critical {
        VerdictLevel::Critical
    } else if worst == Severity::Warning || breached {
        VerdictLevel::Attention
    } else if r.is_partial() || !r.warnings.is_empty() {
        VerdictLevel::Review
    } else {
        VerdictLevel::Ok
    };
}

/// The verdict line (design v6 §3): the top finding's cause; with no finding, where the time
/// went — and when nothing throttled and no load was varied, that a plateau is not a limit
/// one run can size (N2: prove sizes it).
pub fn headline(r: &Report) -> String {
    if let Some(f) = r.findings.first() {
        return f.cause.clone();
    }
    if r.summary.ops == 0 {
        return "No operations completed in the measurement window.".into();
    }
    let kind = r.capacity.selected().map(|h| h.kind);
    let stage = r.stages.bottleneck.label();
    match kind {
        Some(HypothesisKind::Queueing) => format!(
            "Inconclusive: latency grows with the requests in flight ({stage}), but one run varies no load — prove sizes a change."
        ),
        _ if r.resilience.throttled_responses == 0
            && matches!(
                r.stages.bottleneck,
                crate::analysis::diagnosis::Bottleneck::ServerLatency
                    | crate::analysis::diagnosis::Bottleneck::Download
                    | crate::analysis::diagnosis::Bottleneck::Upload
            ) =>
        {
            format!(
                "No limit reached: {stage} dominates (p50 {} · p99 {}); one run varies no load — prove sizes a change.",
                crate::util::fmt_ms(r.summary.latency.p50),
                crate::util::fmt_ms(r.summary.latency.p99)
            )
        }
        _ => format!(
            "No finding: {stage} dominates (p50 {} · p99 {}).",
            crate::util::fmt_ms(r.summary.latency.p50),
            crate::util::fmt_ms(r.summary.latency.p99)
        ),
    }
}

/// Mark a built run as partial (a budget stopped it, it was cancelled after the window, …):
/// `run.partial`, the warning, suppressed findings and the verdict follow. A budget stop is
/// kept over a plain interruption (the budget is why it was interrupted).
pub fn mark_partial(run: &mut Run, reason: PartialReason) {
    let r = &mut run.report;
    let reason = match &r.run.partial {
        Some(p) if p.reason == PartialReason::Budget => PartialReason::Budget,
        _ => reason,
    };
    r.run.partial = Some(PartialInfo {
        reason,
        measured_s: r.run.window_s,
    });
    let breached = !r.verdict.sla.passed;
    finish(run, breached);
}

fn request_p99(reqs: &[ReqRow], weights: &HashMap<u64, f64>) -> RequestStagesP99 {
    let w = |r: &ReqRow| weights.get(&r.op_id).copied().unwrap_or(1.0);
    let col = |f: &dyn Fn(&ReqRow) -> Option<u64>| -> f64 {
        let mut v: Vec<(u64, f64)> = reqs.iter().filter_map(|r| Some((f(r)?, w(r)))).collect();
        crate::analysis::stats::weighted_quantile_us_ms(&mut v, 0.99)
    };
    RequestStagesP99 {
        queue_ms: col(&|r| Some(r.queue_us)),
        connect_ms: col(&|r| (!r.reused && r.connect_us() > 0).then(|| r.connect_us())),
        send_ms: col(&|r| Some(r.send_us)),
        ttfb_ms: col(&|r| Some(r.ttfb_us)),
        body_ms: col(&|r| Some(r.body_us)),
        stall_ms: col(&|r| Some(r.stall_us)),
        total_ms: col(&|r| Some(r.total_us)),
    }
}

fn remotes(reqs: &[ReqRow], weights: &HashMap<u64, f64>) -> Vec<RemoteStats> {
    #[derive(Default)]
    struct RemoteAccum {
        requests: f64,
        errors: f64,
        bytes_down: f64,
        lat: Vec<(u64, f64)>,
    }

    // `None`: network requests that failed with no connection (a reset, a refused dial), kept
    // in their own row so the table counts every request.
    let mut by: HashMap<Option<std::net::SocketAddr>, RemoteAccum> = HashMap::new();
    for r in reqs {
        let key = match r.remote {
            Some(a) => Some(Some(a)),
            None if r.status == 0 && r.protocol != crate::model::WireProtocol::Inproc => Some(None),
            None => None,
        };
        if let Some(a) = key {
            let w = weights.get(&r.op_id).copied().unwrap_or(1.0);
            let e = by.entry(a).or_default();
            e.requests += w;
            let is_cut = r.detail.as_ref().is_some_and(|d| d.cut);
            if r.is_error() && r.status != 404 && !is_cut {
                e.errors += w;
            }
            e.bytes_down += r.bytes_down as f64 * w;
            e.lat.push((r.total_us, w));
        }
    }
    let mut out: Vec<RemoteStats> = by
        .into_iter()
        .map(|(a, mut acc)| RemoteStats {
            remote: a.map_or_else(
                || crate::analysis::slice::NO_CONNECTION.to_owned(),
                |a| a.to_string(),
            ),
            requests_count: acc.requests.round() as u64,
            errors_count: acc.errors.round() as u64,
            bytes_down: acc.bytes_down.round() as u64,
            p50_ms: crate::analysis::stats::weighted_quantile_us_ms(&mut acc.lat, 0.50),
            p99_ms: crate::analysis::stats::weighted_quantile_us_ms(&mut acc.lat, 0.99),
        })
        .collect();
    out.sort_by(|a, b| {
        b.requests_count
            .cmp(&a.requests_count)
            .then(a.remote.cmp(&b.remote))
    });
    out
}

fn mix_entry(m: &crate::config::OpSpec) -> MixEntry {
    let mut e = MixEntry {
        weight: m.weight,
        ..Default::default()
    };
    match &m.op {
        OpKind::Read { size } => {
            e.op = "read".into();
            e.size_bytes = size.map(|s| s.0);
        }
        OpKind::ParquetScan {
            columns,
            column_size,
            footer_size,
            suffix,
        } => {
            e.op = "parquet_scan".into();
            e.columns_count = Some(*columns);
            e.column_bytes = Some(column_size.0);
            e.footer_bytes = Some(footer_size.0);
            e.suffix = suffix.then_some(true);
        }
        OpKind::Write { size } => {
            e.op = "write".into();
            e.size_bytes = Some(size.0);
        }
        OpKind::MultipartWrite { size } => {
            e.op = "multipart_write".into();
            e.size_bytes = Some(size.0);
        }
        OpKind::Stat => e.op = "stat".into(),
        OpKind::List { recursive } => {
            e.op = "list".into();
            e.recursive = Some(*recursive);
        }
        OpKind::ListPaged { limit } => {
            e.op = "list_paged".into();
            e.limit_count = Some(*limit);
        }
        OpKind::Delete { batch } => {
            e.op = "delete".into();
            e.batch_count = Some(*batch);
        }
        OpKind::Copy => e.op = "copy".into(),
        OpKind::PresignedRead => e.op = "presigned_read".into(),
        OpKind::ConditionalWrite { size } => {
            e.op = "conditional_write".into();
            e.size_bytes = Some(size.0);
        }
        OpKind::MetadataWrite { size, entries } => {
            e.op = "metadata_write".into();
            e.size_bytes = Some(size.0);
            e.metadata_entries_count = Some(*entries);
        }
    }
    e
}

/// The workload of a profile in report form (preset defaults applied).
pub fn workload_info(p: &Profile) -> WorkloadInfo {
    let w = &p.workload;
    let ds = w.effective_dataset();
    WorkloadInfo {
        preset: w.preset.slug().to_owned(),
        duration_s: w.duration.get().as_secs_f64(),
        warmup_s: w.warmup.get().as_secs_f64(),
        max_ops_count: w.max_ops,
        rate_per_s: w.peak_rate(),
        access: w.access.to_string(),
        dataset: DatasetInfo {
            objects_count: ds.objects as u64,
            object_size_bytes: ds.object_size.0,
            partitions_count: ds.partitions as u64,
            existing_prefix: ds.existing_prefix.clone(),
            persist: ds.persist,
        },
        mix: w.effective_mix().iter().map(mix_entry).collect(),
        verify: w.verify,
        cleanup: w.cleanup,
        seed: w.seed,
    }
}

/// Rows for slow operations that sampling dropped, rebuilt from the recorder's slow-op samples
/// (which also carry the await-tree call stacks).
fn ensure_slowest(snapshot: &Snapshot, trace: &mut TraceRows) {
    let present: std::collections::HashSet<u64> = trace.ops.iter().map(|o| o.op_id).collect();
    for s in &snapshot.slow_ops {
        if let Some(row) = trace.ops.iter_mut().find(|o| o.op_id == s.op_id) {
            if row.await_tree.is_none() {
                row.await_tree = s.await_tree.clone();
            }
            continue;
        }
        if present.contains(&s.op_id) {
            continue;
        }
        let us = |ms: f64| (ms * 1000.0).round() as u64;
        let t_us = (s.at_s * 1e6) as i64;
        let mut row = crate::trace::log::op_row(
            s.op_id,
            "",
            s.key.clone(),
            t_us,
            us(s.latency_ms),
            s.bytes,
            s.error.clone(),
            s.requests.iter().any(|r| r.is_throttle()),
            s.retries,
            [
                us(s.breakdown.queue_ms),
                us(s.breakdown.cache_ms),
                us(s.breakdown.backend_ms),
                us(s.breakdown.retry_ms),
                us(s.breakdown.client_ms),
            ],
            s.requests.len() as u64,
        );
        row.verb = std::borrow::Cow::Owned(s.verb.clone());
        row.await_tree = s.await_tree.clone();
        trace.ops.push(row);
        let att = crate::trace::log::attempts(&s.requests);
        for (i, r) in s.requests.iter().cloned().enumerate() {
            let n = att.as_ref().map_or(1, |a| a[i]);
            trace
                .reqs
                .push(crate::trace::log::req_row(r, s.op_id, t_us, n));
        }
    }
    trace.ops.sort_by_key(|o| (o.t_us, o.op_id));
    trace.reqs.sort_by_key(|r| (r.t_us, r.op_id));
}

/// Build the report and keep the trace rows next to it.
pub fn build_run(i: ReportInput<'_>) -> Run {
    let ReportInput {
        run_id,
        target_name,
        profile,
        knobs,
        mut snapshot,
        mut trace,
        cleanup,
        warnings,
        started_at,
        ended_at,
        spec,
    } = i;
    let spec = spec.unwrap_or_else(|| {
        let target_ref = target_name
            .clone()
            .or_else(|| profile.target().url.clone())
            .unwrap_or_else(|| profile.target().display());
        super::spec::RunSpec::new(profile, target_ref)
    });
    ensure_slowest(&snapshot, &mut trace);
    let sampled = trace.sampled();
    exact_latency(&mut snapshot, &trace.ops, sampled);
    let t = &profile.target();
    let endpoint = t.endpoint();
    let diagnosis = diagnose(&snapshot, t.is_network());
    let sla_verdict = check_sla(&snapshot, &profile.sla);
    let breached = !sla_verdict.passed;
    let mut options = crate::util::redact_options(&t.options);
    options.remove("ca_cert_pem");
    let interrupted = snapshot.interrupted;
    let effective = snapshot.effective_knobs.clone();
    let workload = workload_info(profile);
    let mut warnings = warnings;
    let chunked_writes = workload
        .mix
        .iter()
        .any(|m| m.weight > 0 && m.op == "multipart_write");
    if chunked_writes
        && effective.write_chunk_size != 0
        && effective.write_chunk_size != knobs.chunk_size.0
    {
        warnings.push(format!(
            "chunk_size {} is outside {}'s multipart part-size limits: OpenDAL wrote {} parts (the requested size was not measured)",
            crate::util::ByteSize(knobs.chunk_size.0),
            t.scheme,
            crate::util::ByteSize(effective.write_chunk_size)
        ));
    }
    if knobs.tail_cut {
        let blind = (crate::engine::stack::TAIL_CUT_BLIND_START.as_secs_f64() - workload.warmup_s)
            .clamp(0.0, snapshot.elapsed_s.max(0.0));
        if blind > 0.0 {
            warnings.push(format!(
                "tail-cut cut nothing during the first {blind:.1} s of the {:.1} s window: OpenDAL's TailCutLayer ignores the latencies of its first 10 s (a long-lived client is unaffected; measure longer to see its effect)",
                snapshot.elapsed_s
            ));
        }
    }
    // A breached SLA is reported, not an exit code (design v6 §8: 3 is Prove's "refuted").
    let exit_code = if interrupted {
        exit::INTERRUPTED
    } else {
        exit::OK
    };
    let op_weights: HashMap<u64, f64> = trace
        .ops
        .iter()
        .filter(|o| (o.weight - 1.0).abs() > f64::EPSILON)
        .map(|o| (o.op_id, o.weight))
        .collect();
    let c = &snapshot.connections;
    let network = Network {
        tls: TlsInfo {
            version: c.tls_version.clone(),
            alpn: c.alpn.clone(),
            cipher: (!c.cipher.is_empty()).then(|| c.cipher.clone()),
            handshake: c.tls,
            handshakes: crate::net::stats::tls_handshakes(&trace.reqs),
        },
        tcp: TcpInfo {
            dns: c.dns,
            connect: c.tcp,
            proxy_connect: c.proxy_connect,
            dialed_count: c.dialed,
            kernel: crate::net::stats::tcp_kernel(&trace.reqs),
        },
        remotes: remotes(&trace.reqs, &op_weights),
    };
    let heatmap = Heatmap::from_timeline(&snapshot.timeline, snapshot.elapsed_s, 120)
        .unwrap_or_else(|| Heatmap::from_ops(&trace.ops, snapshot.elapsed_s, 120));
    let stages = Stages {
        bottleneck: diagnosis.bottleneck,
        shares: (&diagnosis.shares).into(),
        per_verb: snapshot
            .verbs
            .iter()
            .map(|v| VerbBreakdown {
                verb: v.verb.clone(),
                breakdown: v.breakdown,
            })
            .collect(),
        request_p99: request_p99(&trace.reqs, &op_weights),
    };
    let region = t.options.get("region").cloned();
    let capacity = crate::analysis::capacity::estimate_with(
        &trace.reqs,
        &trace.ops,
        crate::analysis::capacity::Window::measured(snapshot.elapsed_s),
        &crate::analysis::capacity::FitInputs {
            client_cpu_busy_ratio: Some(
                snapshot.client.cpu_pct / 100.0 / snapshot.client.cpu_cores.max(1) as f64,
            ),
            client_saturated: snapshot
                .self_obs
                .as_ref()
                .is_some_and(|o| o.client_saturated),
        },
    )
    .finished();
    let partial = interrupted.then_some(PartialInfo {
        reason: PartialReason::Interrupted,
        measured_s: snapshot.elapsed_s,
    });
    let report = Report {
        schema: REPORT_SCHEMA.to_owned(),
        spec,
        capacity,
        run: RunInfo {
            id: run_id.to_string(),
            source: RunSource::Run,
            label: profile.name.clone(),
            parent_id: None,
            started_at: crate::util::fmt_rfc3339(started_at),
            ended_at: crate::util::fmt_rfc3339(ended_at),
            window_s: snapshot.elapsed_s,
            partial,
            sampled,
            trace: trace.info.clone(),
            oprobe_version: env!("CARGO_PKG_VERSION").to_owned(),
            host: HostInfo::current(),
            env: EnvInfo {
                region,
                provider: None,
            },
        },
        target: TargetInfo {
            name: target_name,
            display: t.display(),
            scheme: t.scheme.clone(),
            endpoint: if t.is_network() { endpoint } else { None },
            options,
            proxy: c.proxy.as_deref().map(crate::util::strip_url_credentials),
        },
        workload,
        effective_write: effective,
        sla: (&profile.sla).into(),
        verdict: Verdict {
            level: VerdictLevel::Ok,
            headline: diagnosis.headline.clone(),
            exit_code,
            bottleneck: diagnosis.bottleneck,
            sla: sla_verdict,
        },
        summary: snapshot.summary.clone(),
        stages,
        apis: snapshot.apis.clone(),
        verbs: snapshot.verbs.clone(),
        connections: snapshot.connections.clone(),
        network,
        resilience: snapshot.resilience.clone(),
        bytes: snapshot.bytes.clone(),
        cache: snapshot.cache.clone(),
        integrity: snapshot.integrity.clone(),
        self_stats: crate::model::SelfStats::from_observation(
            &snapshot.client,
            snapshot.self_obs.as_ref(),
        ),
        schedule: crate::model::ScheduleStats::from_open_loop(
            snapshot.open_loop.as_ref(),
            snapshot.elapsed_s,
        ),
        dimensions: crate::model::Dimensions::from_stats(snapshot.dimensions.as_ref()),
        sampling: crate::model::Sampling::from_stats(&trace.sampling),
        open_loop: snapshot.open_loop.clone(),
        timeline: snapshot.timeline.clone(),
        heatmap,
        latency_histogram: snapshot.latency_histogram.clone(),
        findings: Vec::new(),
        slowest: snapshot.slow_ops.iter().map(|s| s.op_id).collect(),
        cleanup,
        warnings,
    };
    let mut run = Run {
        report,
        ops: trace.ops,
        reqs: trace.reqs,
    };
    // Findings read the finished report and the trace rows (stream B3: analysis::findings).
    run.report.findings = crate::analysis::findings::derive(&run);
    finish(&mut run, breached);
    run
}

/// Build only the report (the trace rows are dropped).
pub fn build_report(i: ReportInput<'_>) -> Report {
    build_run(i).report
}
