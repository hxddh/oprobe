//! Clap arguments → the service layer's typed requests ([`oprobe_core::app::input`]).

use crate::app::input as i;

use super::args::*;

impl From<SafetyArgs> for i::Safety {
    fn from(a: SafetyArgs) -> Self {
        Self {
            dry_run: a.dry_run,
            read_only: a.read_only,
            max_cost_usd: a.max_cost_usd,
            max_requests: a.max_requests,
            max_bytes: a.max_bytes,
            max_duration: a.max_duration,
            idempotency_key: a.idempotency_key,
            allow_write: true,
        }
    }
}

impl From<TargetArgs> for i::TargetInput {
    fn from(a: TargetArgs) -> Self {
        Self {
            target: a.target,
            profile: a.profile,
            endpoint: a.endpoint,
            region: a.region,
            options: a.options,
            anonymous: a.anonymous,
            ca_cert: a.ca_cert,
            insecure: a.insecure,
            proxy: a.proxy,
        }
    }
}

impl From<WorkloadArgs> for i::WorkloadInput {
    fn from(a: WorkloadArgs) -> Self {
        Self {
            preset: a.preset,
            duration: a.duration,
            warmup: a.warmup,
            max_ops: a.max_ops,
            rate: a.rate,
            existing: a.existing,
            objects: a.objects,
            object_size: a.object_size,
            access: a.access,
            persist_dataset: a.persist_dataset,
            no_verify: a.no_verify,
            keep_data: a.keep_data,
        }
    }
}

impl From<KnobArgs> for i::KnobInput {
    fn from(a: KnobArgs) -> Self {
        Self {
            concurrency: a.concurrency,
            io_concurrency: a.io_concurrency,
            chunk_size: a.chunk_size,
            range_gap: a.range_gap,
            max_inflight: a.max_inflight,
            http: a.http.map(Into::into),
            no_keep_alive: a.no_keep_alive,
            microscope: a.microscope,
            cache: a.cache,
            retries: a.retries,
            tail_cut: a.tail_cut,
            addressing: a.addressing,
            checksum: a.checksum,
            sse: a.sse,
        }
    }
}

impl From<CostArgs> for i::CostInput {
    fn from(a: CostArgs) -> Self {
        Self {
            provider: a.provider,
            monthly_ops: a.monthly_ops,
            egress_fraction: a.egress_fraction,
            stored_gib: a.stored_gib,
        }
    }
}

impl From<SlaArgs> for i::SlaInput {
    fn from(a: SlaArgs) -> Self {
        Self {
            sla: a.sla.map(Into::into),
            max_error_rate: a.max_error_rate,
            max_throttle_rate: a.max_throttle_rate,
            p99_ms: a.p99_ms,
        }
    }
}

impl From<OutputArgs> for i::OutputOptions {
    fn from(a: OutputArgs) -> Self {
        Self {
            json: a.json,
            markdown: a.markdown,
            no_save: a.no_save,
            fail_on: a.fail_on.map(Into::into),
            label: a.label,
        }
    }
}

impl From<TuneSearchArgs> for i::TuneSearch {
    fn from(a: TuneSearchArgs) -> Self {
        Self {
            objective: a.objective.into(),
            trial_duration: a.trial_duration,
            trial_warmup: a.trial_warmup,
            max_trials: a.max_trials,
            max_concurrency: a.max_concurrency,
            cooldown: a.cooldown,
            max_throttle_ratio: a.max_throttle_ratio,
            boundary_repeats: a.boundary_repeats,
            headroom_ratio: a.headroom_ratio,
            risk_ratio: a.risk,
        }
    }
}

impl From<TuneArgs> for i::TuneRequest {
    fn from(a: TuneArgs) -> Self {
        Self {
            target: a.target.into(),
            workload: a.workload.into(),
            knobs: a.knobs.into(),
            cost: a.cost.into(),
            sla: a.sla.into(),
            output: a.output.into(),
            safety: a.safety.into(),
            search: a.search.into(),
            budget: a.budget,
            emit_profile: a.emit_profile,
            emit_rust: a.emit_rust,
            emit_python: a.emit_python,
        }
    }
}

impl From<VerifyArgs> for i::VerifyRequest {
    fn from(a: VerifyArgs) -> Self {
        Self {
            run: a.run,
            code: a.code,
            all: a.all,
            repeat: a.repeat,
            no_calibrate: a.no_calibrate,
            safety: a.safety.into(),
            no_save: a.no_save,
        }
    }
}

impl From<FixArgs> for i::FixRequest {
    fn from(a: FixArgs) -> Self {
        Self {
            target: a.target.into(),
            workload: a.workload.into(),
            knobs: a.knobs.into(),
            cost: a.cost.into(),
            sla: a.sla.into(),
            output: a.output.into(),
            safety: a.safety.into(),
            search: a.search.into(),
            repeat: a.repeat,
            write_profile: a.write_profile,
            budget: a.budget,
            from: a.from,
            set: a.set,
        }
    }
}

impl From<CalibrateArgs> for i::CalibrateRequest {
    fn from(a: CalibrateArgs) -> Self {
        Self {
            target: a.target.into(),
            workload: a.workload.into(),
            knobs: a.knobs.into(),
            safety: a.safety.into(),
            repeat: a.repeat,
            from: a.from,
            no_save: a.no_save,
        }
    }
}

impl From<CompareArgs> for i::CompareRequest {
    fn from(a: CompareArgs) -> Self {
        Self {
            baseline: a.baseline,
            candidate: a.candidate,
            baseline_name: a.baseline_name,
            max_p99_increase: a.max_p99_increase,
            max_p50_increase: a.max_p50_increase,
            min_latency_delta_ms: a.min_latency_delta_ms,
            max_throughput_drop: a.max_throughput_drop,
            max_error_increase: a.max_error_increase,
            max_request_amp_increase: a.max_request_amp_increase,
            max_read_amp_increase: a.max_read_amp_increase,
            max_cost_increase: a.max_cost_increase,
            min_samples: a.min_samples,
            allow_throttling: a.allow_throttling,
            allow_workload_change: a.allow_workload_change,
            allow_target_change: a.allow_target_change,
            strict: a.strict,
            fail_on_warn: a.fail_on_warn,
            markdown: a.markdown,
            junit: a.junit,
            json: a.json,
        }
    }
}

impl From<DoctorArgs> for i::DoctorRequest {
    fn from(a: DoctorArgs) -> Self {
        Self {
            target: a.target.into(),
            read_only: a.read_only,
            write_probe: a.write_probe,
            verbose: a.verbose,
        }
    }
}

impl From<TargetSampleArgs> for i::SampleRequest {
    fn from(a: TargetSampleArgs) -> Self {
        Self {
            target: a.target.into(),
            prefix: a.prefix,
            limit: a.limit,
        }
    }
}

impl From<TargetAddArgs> for i::TargetAddRequest {
    fn from(a: TargetAddArgs) -> Self {
        Self {
            name: a.name,
            url: a.url,
            region: a.region,
            endpoint: a.endpoint,
            credentials: a.credentials,
            options: a.options,
            no_test: a.no_test,
            replace: a.replace,
        }
    }
}

impl From<TargetTestArgs> for i::TargetTestRequest {
    fn from(a: TargetTestArgs) -> Self {
        Self {
            name: a.name,
            read_only: a.read_only,
            write_probe: a.write_probe,
        }
    }
}

impl From<CleanupArgs> for i::CleanupRequest {
    fn from(a: CleanupArgs) -> Self {
        Self {
            target: a.target.into(),
            older_than: a.older_than,
            datasets: a.datasets,
            dry_run: a.dry_run,
            read_only: a.read_only,
            yes: a.yes,
        }
    }
}

impl From<AnalyzeArgs> for i::AnalyzeRequest {
    fn from(a: AnalyzeArgs) -> Self {
        Self {
            run: a.run,
            by: a.by,
            window: a.window,
            r#where: a.r#where,
            slowest: a.slowest,
        }
    }
}

impl From<ExplainArgs> for i::ExplainRequest {
    fn from(a: ExplainArgs) -> Self {
        Self {
            subject: a.subject,
            list: a.list,
            top: a.top,
        }
    }
}

impl From<RunsListArgs> for i::RunsListRequest {
    fn from(a: RunsListArgs) -> Self {
        Self { limit: a.limit }
    }
}

impl From<RunsShowArgs> for i::RunsShowRequest {
    fn from(a: RunsShowArgs) -> Self {
        Self {
            run: a.run,
            detail: a.detail,
        }
    }
}

impl From<RunsRmArgs> for i::RunsRmRequest {
    fn from(a: RunsRmArgs) -> Self {
        Self { runs: a.runs }
    }
}

impl From<RunsGcArgs> for i::RunsGcRequest {
    fn from(a: RunsGcArgs) -> Self {
        Self {
            keep: a.keep,
            older_than: a.older_than,
            dry_run: a.dry_run,
        }
    }
}

impl From<BaselineSetArgs> for i::BaselineSetRequest {
    fn from(a: BaselineSetArgs) -> Self {
        Self {
            name: a.name,
            runs: a.runs,
            last: a.last,
            window: a.window,
            append: a.append,
        }
    }
}

impl From<DemoArgs> for i::DemoRequest {
    fn from(a: DemoArgs) -> Self {
        Self {
            duration: a.duration,
        }
    }
}

impl From<ProbeArgs> for i::ProbeRequest {
    fn from(a: ProbeArgs) -> Self {
        Self {
            endpoint: a.endpoint,
            samples: a.samples,
            http: a.http.into(),
            get: a.get,
            timeout: a.timeout,
            ca_cert: a.ca_cert,
            insecure: a.insecure,
            proxy: a.proxy,
            json: a.json,
        }
    }
}

impl From<CompatArgs> for i::CompatRequest {
    fn from(a: CompatArgs) -> Self {
        Self {
            target: a.target.into(),
            safety: a.safety.into(),
            min_score: a.min_score,
        }
    }
}

impl From<MatrixArgs> for i::MatrixRequest {
    fn from(a: MatrixArgs) -> Self {
        Self {
            target: a.target.into(),
            workload: a.workload.into(),
            knobs: a.knobs.into(),
            cost: a.cost.into(),
            sla: a.sla.into(),
            safety: a.safety.into(),
            targets: a.targets,
            vary: a.vary,
            rank_by: a.rank_by,
            label: a.label,
        }
    }
}

impl From<WatchArgs> for i::WatchRequest {
    fn from(a: WatchArgs) -> Self {
        Self {
            target: a.target.into(),
            workload: a.workload.into(),
            knobs: a.knobs.into(),
            cost: a.cost.into(),
            safety: a.safety.into(),
            interval: a.interval,
            window: a.window,
            slo: a.slo,
            listen: a.listen,
            otlp_endpoint: a.otlp_endpoint,
            count: a.count,
        }
    }
}

impl From<ExportArgs> for i::ExportRequest {
    fn from(a: ExportArgs) -> Self {
        Self {
            run: a.run,
            format: a.format.into(),
            out: a.out,
            endpoint: a.endpoint,
            table: a.table,
            signal: a.signal.into(),
        }
    }
}

impl From<LabArgs> for i::LabRequest {
    fn from(a: LabArgs) -> Self {
        Self {
            bind: a.bind,
            scenario: a.scenario,
            error_every: a.error_every,
            reset_every: a.reset_every,
            tls: a.tls,
            h1_only: a.h1_only,
            latency: a.latency,
            straggler_every: a.straggler_every,
            straggler: a.straggler,
            throttle_above: a.throttle_above,
            throttle_every: a.throttle_every,
            throttle_status: a.throttle_status,
            bandwidth_kib: a.bandwidth_kib,
            throttle_early_close: a.throttle_early_close,
            complete_delay: a.complete_delay,
            write_ca: a.write_ca,
        }
    }
}

impl From<FailOn> for i::FailOn {
    fn from(v: FailOn) -> Self {
        match v {
            FailOn::Warning => Self::Warning,
            FailOn::Critical => Self::Critical,
        }
    }
}

impl From<SlaMode> for i::SlaMode {
    fn from(v: SlaMode) -> Self {
        match v {
            SlaMode::Enforce => Self::Enforce,
            SlaMode::None => Self::None,
        }
    }
}

impl From<ObjectiveArg> for i::Objective {
    fn from(v: ObjectiveArg) -> Self {
        v.objective()
    }
}

impl From<ExportFormat> for i::ExportFormat {
    fn from(v: ExportFormat) -> Self {
        match v {
            ExportFormat::Json => Self::Json,
            ExportFormat::Ndjson => Self::Ndjson,
            ExportFormat::Csv => Self::Csv,
            ExportFormat::Har => Self::Har,
            ExportFormat::Otlp => Self::Otlp,
            ExportFormat::Md => Self::Md,
            ExportFormat::Html => Self::Html,
        }
    }
}

impl From<OtlpSignal> for i::OtlpSignal {
    fn from(v: OtlpSignal) -> Self {
        match v {
            OtlpSignal::Traces => Self::Traces,
            OtlpSignal::Metrics => Self::Metrics,
            OtlpSignal::All => Self::All,
        }
    }
}

/// The profile flags of `oprobe run` (and `--from` / `--set` / `--label`).
pub fn profile_request(a: &RunArgs) -> i::ProfileRequest {
    i::ProfileRequest {
        target: a.target.clone().into(),
        workload: a.workload.clone().into(),
        knobs: a.knobs.clone().into(),
        cost: a.cost.clone().into(),
        sla: a.sla.clone().into(),
        from: a.from.clone(),
        set: a.set.clone(),
        label: a.output.label.clone(),
    }
}
