//! NSH 引擎线程响应式 `Copy` 状态仓库：
//! 严格遵守 NSH `rust.md` 规范：耗时 OpenDAL 负载在 `background(...)`（Tokio 多线程池）运行，
//! 返回结果后在 `spawn(...)`（引擎线程）通过 `batch(|| ..)` 原子写回 `Signal`。

use std::time::Duration;

use nsh_core::prelude::*;
use oprobe::autotune::{
    generate_rust_snippet, AutoTuneEngine, CandidateTrial, TuneKnob, TuneSearchSpace,
};
use oprobe::cost::compare_multi_cloud_finops;
use oprobe::net_probe::probe_endpoint_handshakes;
use oprobe::report::BenchmarkReport;
use oprobe::telemetry::TelemetryFrame;
use oprobe::tui::command::{apply_state_mutation, parse_minibuffer_command, MinibufferCommand};
use oprobe::workload::{execute_workload_round, ProfileConfig, WorkloadKind};

#[derive(Clone, Copy)]
pub struct ProfilerStore {
    pub profile: Signal<ProfileConfig>,
    pub knob: Signal<TuneKnob>,
    pub frame: Signal<TelemetryFrame>,
    pub trials: Signal<Vec<CandidateTrial>>,
    pub generated_code: Signal<SharedString>,
    pub active_tab: Signal<u8>,
    pub command_draft: Signal<String>,
    pub status_label: Signal<SharedString>,
    pub running: Signal<bool>,
    held_task: Stored<Option<Spawned<()>>>,
}

impl ProfilerStore {
    pub fn new() -> Self {
        let default_profile = ProfileConfig::memory_demo(WorkloadKind::ParquetPointLookup);
        let default_knob = TuneKnob::default();
        let code = generate_rust_snippet(&default_profile.target.scheme, &default_knob);
        Self {
            profile: signal(default_profile),
            knob: signal(default_knob),
            frame: signal(TelemetryFrame::default()),
            trials: signal(Vec::new()),
            generated_code: signal(SharedString::from(code)),
            active_tab: signal(0),
            command_draft: signal(String::new()),
            status_label: signal(SharedString::from(
                "IDLE · 按 Space 单点剖析 | 按 O 自动寻优 | 按 E 落盘报告 | 底部命令栏支持 :help",
            )),
            running: signal(false),
            held_task: stored(None),
        }
    }

    pub fn cancel_active_task(&self) {
        if let Some(prev) = self.held_task.replace(None) {
            prev.cancel();
            self.running.set(false);
            self.status_label
                .set(SharedString::from("CANCELLED · 已取消在途异步流"));
        }
    }

    pub fn select_workload(&self, kind: WorkloadKind) {
        self.profile.update(|p| p.workload.kind = kind);
        self.run_single_probe();
    }

    pub fn update_knob(&self, f: impl FnOnce(&mut TuneKnob)) {
        let mut next = self.knob.get_untracked();
        f(&mut next);
        let scheme = self.profile.with_untracked(|p| p.target.scheme.clone());
        self.knob.set(next);
        self.generated_code
            .set(SharedString::from(generate_rust_snippet(&scheme, &next)));
        self.run_single_probe();
    }

    pub fn toggle_probe_session(&self) {
        if self.running.get_untracked() {
            self.cancel_active_task();
        } else {
            self.run_single_probe();
        }
    }

    pub fn submit_command(&self) {
        let raw = self.command_draft.get_untracked();
        if raw.trim().is_empty() {
            return;
        }
        self.command_draft.set(String::new());

        match parse_minibuffer_command(&raw) {
            Ok(MinibufferCommand::ProbeOnce) => self.run_single_probe(),
            Ok(MinibufferCommand::AutoTune) => self.start_autotune_sweep(),
            Ok(MinibufferCommand::ExportReport(prefix)) => self.export_snapshot(&prefix),
            Ok(MinibufferCommand::NetProbe(url)) => self.run_net_probe(url),
            Ok(MinibufferCommand::Quit) => self.cancel_active_task(),
            Ok(cmd) => {
                let mut prof = self.profile.get_untracked();
                let mut knob = self.knob.get_untracked();
                if let Some(msg) = apply_state_mutation(&cmd, &mut prof, &mut knob) {
                    let code = generate_rust_snippet(&prof.target.scheme, &knob);
                    batch(|| {
                        self.profile.set(prof);
                        self.knob.set(knob);
                        self.generated_code.set(SharedString::from(code));
                        self.status_label.set(SharedString::from(msg));
                    });
                    if !matches!(cmd, MinibufferCommand::Help) {
                        self.run_single_probe();
                    }
                }
            }
            Err(e) => {
                self.status_label
                    .set(SharedString::from(format!("命令解析错误: {e}")));
            }
        }
    }

    pub fn export_snapshot(&self, prefix: &str) {
        let prof = self.profile.get_untracked();
        let knob = self.knob.get_untracked();
        let frame = self.frame.get_untracked();
        let trials = self.trials.get_untracked();
        let code = self.generated_code.get_untracked().to_string();
        let prefix_owned = prefix.to_owned();
        let store = *self;

        spawn(async move {
            let res = blocking(move || -> Result<String, String> {
                let mut rep = BenchmarkReport::from_single_run(prof, knob, frame);
                rep.trials = trials;
                rep.generated_rust_code = code;
                let j = format!("{prefix_owned}.json");
                let m = format!("{prefix_owned}.md");
                let c = format!("{prefix_owned}.csv");
                rep.save_json(&j).map_err(|e| e.to_string())?;
                rep.save_markdown(&m).map_err(|e| e.to_string())?;
                rep.save_csv(&c).map_err(|e| e.to_string())?;
                Ok(format!("已落盘报告: {j}, {m}, {c}"))
            })
            .await;

            match res {
                Ok(Ok(msg)) => store.status_label.set(SharedString::from(msg)),
                Ok(Err(e)) => store
                    .status_label
                    .set(SharedString::from(format!("导出失败: {e}"))),
                Err(e) => store
                    .status_label
                    .set(SharedString::from(format!("线程调度异常: {e}"))),
            }
        });
    }

    pub fn run_net_probe(&self, endpoint: String) {
        self.cancel_active_task();
        let store = *self;
        self.running.set(true);
        self.status_label.set(SharedString::from(format!(
            "NET-PROBE · 正在对 {endpoint} 采样微秒级 DNS/TCP/TLS 握手..."
        )));

        let task = spawn(async move {
            let res = background(async move {
                probe_endpoint_handshakes(&endpoint, 4, Duration::from_secs(4))
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;

            batch(|| {
                store.running.set(false);
                match res {
                    Ok(Ok(hs)) => {
                        let msg = format!(
                            "NET-PROBE 完成 · {} DNS {:.2}ms | TCP {:.2}ms | TLS {:.2}ms | ALPN {}",
                            hs.host, hs.dns_p50_ms, hs.tcp_rtt_p50_ms, hs.tls_p50_ms, hs.negotiated_alpn
                        );
                        store.frame.update(|f| f.handshake = hs);
                        store.active_tab.set(3);
                        store.status_label.set(SharedString::from(msg));
                    }
                    Ok(Err(e)) => store
                        .status_label
                        .set(SharedString::from(format!("网络探测失败: {e}"))),
                    Err(e) => store
                        .status_label
                        .set(SharedString::from(format!("后台任务取消: {e}"))),
                }
            });
        });
        self.held_task.replace(Some(task));
    }

    pub fn run_single_probe(&self) {
        self.cancel_active_task();
        let store = *self;
        let prof = self.profile.get_untracked();
        let knob = self.knob.get_untracked();

        self.running.set(true);
        self.status_label.set(SharedString::from(format!(
            "PROFILING · {} (conc={}, chunk={}KiB)...",
            prof.workload.kind.title(),
            knob.concurrent,
            knob.chunk_bytes / 1024
        )));

        let task = spawn(async move {
            let res = background(async move {
                execute_workload_round(&prof, &knob, None)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;

            batch(|| {
                store.running.set(false);
                match res {
                    Ok(Ok(frame)) => {
                        let summary = format!(
                            "[{}] {} · {:.1} MiB/s",
                            frame.diagnosis.severity.badge(),
                            frame.diagnosis.headline,
                            frame.transport.aggregate_mib_s
                        );
                        store.frame.set(frame);
                        store.status_label.set(SharedString::from(summary));
                    }
                    Ok(Err(err)) => {
                        store
                            .status_label
                            .set(SharedString::from(format!("剖析异常: {err}")));
                    }
                    Err(join_err) => {
                        store
                            .status_label
                            .set(SharedString::from(format!("任务中止: {join_err}")));
                    }
                }
            });
        });
        self.held_task.replace(Some(task));
    }

    pub fn start_autotune_sweep(&self) {
        self.cancel_active_task();
        let store = *self;
        let prof = self.profile.get_untracked();

        self.running.set(true);
        self.status_label.set(SharedString::from(
            "AUTO-TUNING · 正在执行两阶段防 503 坐标寻优...",
        ));

        let task = spawn(async move {
            let res = background(async move {
                let engine = AutoTuneEngine::new(prof.sla.clone(), TuneSearchSpace::default());
                engine
                    .run_adaptive_sweep(&prof)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;

            batch(|| {
                store.running.set(false);
                match res {
                    Ok(Ok(outcome)) => {
                        let summary = format!(
                            "寻优完成 · conc={} chunk={}KiB gap={}KiB | P99 {:.2}ms | {:.1} MiB/s",
                            outcome.winner.knob.concurrent,
                            outcome.winner.knob.chunk_bytes / 1024,
                            outcome.winner.knob.range_gap_bytes / 1024,
                            outcome.winner.worst_p99_ms,
                            outcome.winner.aggregate_mib_s
                        );
                        store.knob.set(outcome.winner.knob);
                        store.frame.set(outcome.best_frame);
                        store.trials.set(outcome.trials);
                        store
                            .generated_code
                            .set(SharedString::from(outcome.generated_rust_code));
                        store.active_tab.set(1);
                        store.status_label.set(SharedString::from(summary));
                    }
                    Ok(Err(err)) => {
                        store
                            .status_label
                            .set(SharedString::from(format!("寻优失败: {err}")));
                    }
                    Err(join_err) => {
                        store
                            .status_label
                            .set(SharedString::from(format!("寻优中止: {join_err}")));
                    }
                }
            });
        });
        self.held_task.replace(Some(task));
    }

    pub fn render_inspector_text(&self) -> SharedString {
        let tab = self.active_tab.get();
        match tab {
            0 => self
                .frame
                .with(|f| SharedString::from(f.await_tree_dump.clone())),
            1 => self.generated_code.get(),
            2 => {
                let prof = self.profile.get();
                self.frame.with(|f| {
                    let rows = compare_multi_cloud_finops(
                        &f.transport,
                        prof.workload.kind.write_ratio(),
                        prof.workload.internet_egress_ratio,
                    );
                    let mut lines = vec![format!(
                        "{:<22} {:>12} {:>12} {:>12} {:>14} {:>15}",
                        "CLOUD PROVIDER", "STORAGE", "API FEES", "EGRESS", "TOTAL/MO", "AMP WASTE"
                    )];
                    for c in rows {
                        lines.push(format!(
                            "{:<22} {:>11.2}$ {:>11.2}$ {:>11.2}$ {:>13.2}$ {:>14.2}$",
                            c.provider_name,
                            c.monthly_storage_usd,
                            c.monthly_api_requests_usd,
                            c.monthly_egress_usd,
                            c.total_monthly_usd,
                            c.waste_from_amplification_usd
                        ));
                    }
                    SharedString::from(lines.join("\n"))
                })
            }
            _ => self.frame.with(|f| {
                let hs = &f.handshake;
                SharedString::from(format!(
                    "Endpoint: {} | TLS: {} | ALPN: {} | Cipher: {}\nDNS P50/P99: {:.2}/{:.2}ms | TCP RTT: {:.2}/{:.2}ms | TLS: {:.2}/{:.2}ms\nRecent Request-IDs: {:?}",
                    hs.endpoint,
                    hs.tls_version,
                    hs.negotiated_alpn,
                    hs.cipher_suite,
                    hs.dns_p50_ms,
                    hs.dns_p99_ms,
                    hs.tcp_rtt_p50_ms,
                    hs.tcp_rtt_p99_ms,
                    hs.tls_p50_ms,
                    hs.tls_p99_ms,
                    f.transport.recent_request_ids
                ))
            }),
        }
    }
}
