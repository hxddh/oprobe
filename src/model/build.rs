//! Build a [`Report`] (and the full [`Run`]) from what the engine measured.

use std::collections::HashMap;
use std::time::SystemTime;

use super::report::*;
use super::{ReqRow, Run, RunId};
use crate::analysis::cost::cost_report;
use crate::analysis::diagnosis::{Severity, check_sla, diagnose};
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
}

/// Percentile (nearest rank) of `v` in ms; reorders `v`.
fn p_ms(v: &mut [u64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let i = ((v.len() as f64 * q).ceil() as usize).clamp(1, v.len()) - 1;
    let (_, x, _) = v.select_nth_unstable(i);
    *x as f64 / 1000.0
}

fn request_p99(reqs: &[ReqRow]) -> RequestStagesP99 {
    let col = |f: &dyn Fn(&ReqRow) -> Option<u64>| -> f64 {
        let mut v: Vec<u64> = reqs.iter().filter_map(f).collect();
        p_ms(&mut v, 0.99)
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

fn remotes(reqs: &[ReqRow]) -> Vec<RemoteStats> {
    let mut by: HashMap<std::net::SocketAddr, (u64, u64, u64, Vec<u64>)> = HashMap::new();
    for r in reqs {
        if let Some(a) = r.remote {
            let e = by.entry(a).or_default();
            e.0 += 1;
            if r.is_error() && r.status != 404 {
                e.1 += 1;
            }
            e.2 += r.bytes_down;
            e.3.push(r.total_us);
        }
    }
    let mut out: Vec<RemoteStats> = by
        .into_iter()
        .map(|(a, (n, err, down, mut lat))| RemoteStats {
            remote: a.to_string(),
            requests_count: n,
            errors_count: err,
            bytes_down: down,
            p50_ms: p_ms(&mut lat, 0.50),
            p99_ms: p_ms(&mut lat, 0.99),
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
        rate_per_s: w.rate,
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
            0,
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
        snapshot,
        mut trace,
        cleanup,
        warnings,
        started_at,
        ended_at,
    } = i;
    ensure_slowest(&snapshot, &mut trace);
    let t = &profile.target;
    let endpoint = t.endpoint();
    let diagnosis = diagnose(&snapshot, knobs, &profile.sla, t.is_network());
    let sla_verdict = check_sla(&snapshot, &profile.sla);
    let mut cost = cost_report(&snapshot, &profile.cost, &t.scheme, endpoint.as_deref());
    if cost.applicable && cost.provider.is_none() {
        cost.provider = Some(cost.selected.key.clone());
    }
    let mut options = crate::util::redact_options(&t.options);
    options.remove("ca_cert_pem");
    let interrupted = snapshot.interrupted;
    let effective = EffectiveKnobValues::new(knobs, &snapshot.effective_knobs);
    let workload = workload_info(profile);
    let mut warnings = warnings;
    let chunked_writes = workload
        .mix
        .iter()
        .any(|m| m.weight > 0 && m.op == "multipart_write");
    if chunked_writes && effective.write_chunk_clamped() {
        warnings.push(format!(
            "chunk_size {} is outside {}'s multipart part-size limits: OpenDAL wrote {} parts (the requested size was not measured)",
            crate::util::ByteSize(knobs.chunk_size.0),
            t.scheme,
            crate::util::ByteSize(effective.write_chunk_bytes)
        ));
    }
    let findings: Vec<Finding> = diagnosis.findings.iter().map(Finding::from).collect();
    let worst = findings
        .iter()
        .map(|f| f.severity)
        .max()
        .unwrap_or_default();
    let breached = !sla_verdict.passed;
    let level = if (breached && sla_verdict.enforced) || worst == Severity::Critical {
        VerdictLevel::Critical
    } else if worst == Severity::Warning || breached {
        VerdictLevel::Attention
    } else if interrupted || !warnings.is_empty() {
        VerdictLevel::Review
    } else {
        VerdictLevel::Ok
    };
    let exit_code = if interrupted {
        exit::INTERRUPTED
    } else if breached && sla_verdict.enforced {
        exit::GATE
    } else {
        exit::OK
    };
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
        remotes: remotes(&trace.reqs),
    };
    let heatmap = Heatmap::from_ops(&trace.ops, snapshot.elapsed_s, 120);
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
        request_p99: request_p99(&trace.reqs),
    };
    let sampled = trace.sampled();
    let region = t.options.get("region").cloned();
    let report = Report {
        schema: REPORT_SCHEMA.to_owned(),
        run: RunInfo {
            id: run_id.to_string(),
            label: profile.name.clone(),
            started_at: crate::util::fmt_rfc3339(started_at),
            ended_at: crate::util::fmt_rfc3339(ended_at),
            window_s: snapshot.elapsed_s,
            interrupted,
            sampled,
            trace: trace.info.clone(),
            oprobe_version: env!("CARGO_PKG_VERSION").to_owned(),
            host: HostInfo::current(),
            env: EnvInfo {
                region,
                provider: cost.provider.clone(),
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
        knobs: KnobsInfo {
            requested: knobs.into(),
            effective,
        },
        sla: (&profile.sla).into(),
        verdict: Verdict {
            level,
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
        client: snapshot.client.clone(),
        open_loop: snapshot.open_loop.clone(),
        timeline: snapshot.timeline.clone(),
        heatmap,
        findings,
        cost,
        slowest: snapshot.slow_ops.iter().map(|s| s.op_id).collect(),
        cleanup,
        warnings,
        tune: None,
    };
    Run {
        report,
        ops: trace.ops,
        reqs: trace.reqs,
    }
}

/// Build only the report (the trace rows are dropped).
pub fn build_report(i: ReportInput<'_>) -> Report {
    build_run(i).report
}
