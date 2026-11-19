pub mod controls;

use nsh_base::{input, theme};
use nsh_core::prelude::*;
use oprobe::workload::WorkloadKind;

use crate::store::ProfilerStore;
use controls::{action_button, chip_toggle, kpi_card, section_frame};

pub fn workbench_screen(store: ProfilerStore) -> impl IntoElement {
    view()
        .flex_1()
        .p_4()
        .gap_3()
        // 1. 顶部标题与四大负载切换栏
        .child(
            view()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    view()
                        .gap_1()
                        .child(
                            text("oprobe · OpenDAL Storage Stack Profiler")
                                .font_weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            text(store.status_label)
                                .text_sm()
                                .text_color(move || theme().muted_foreground),
                        ),
                )
                .child(
                    view()
                        .flex_row()
                        .gap_2()
                        .child(chip_toggle(
                            "Parquet 点查",
                            computed(move || {
                                store.profile.with(|p| p.workload.kind == WorkloadKind::ParquetPointLookup)
                            }),
                            move || store.select_workload(WorkloadKind::ParquetPointLookup),
                        ))
                        .child(chip_toggle(
                            "AI 分片写入",
                            computed(move || {
                                store.profile.with(|p| p.workload.kind == WorkloadKind::CheckpointMultipart)
                            }),
                            move || store.select_workload(WorkloadKind::CheckpointMultipart),
                        ))
                        .child(chip_toggle(
                            "元数据风暴",
                            computed(move || {
                                store.profile.with(|p| p.workload.kind == WorkloadKind::MetadataPrefixStorm)
                            }),
                            move || store.select_workload(WorkloadKind::MetadataPrefixStorm),
                        ))
                        .child(chip_toggle(
                            "混合读写",
                            computed(move || {
                                store.profile.with(|p| p.workload.kind == WorkloadKind::MixedReadWrite)
                            }),
                            move || store.select_workload(WorkloadKind::MixedReadWrite),
                        ))
                        .child(action_button(
                            computed(move || {
                                if store.running.get() {
                                    SharedString::from("急停 (Esc)")
                                } else {
                                    SharedString::from("单点剖析 (Space)")
                                }
                            }),
                            false,
                            move || store.toggle_probe_session(),
                        ))
                        .child(action_button(
                            SharedString::from("自动寻优 (O)"),
                            true,
                            move || store.start_autotune_sweep(),
                        ))
                        .child(action_button(
                            SharedString::from("导出报告 (E)"),
                            false,
                            move || store.export_snapshot("oprobe_nsh_report"),
                        )),
                ),
        )
        // 2. 洋葱层与并发参数实时控制条
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(chip_toggle(
                    "Foyer 本地缓存",
                    computed(move || store.knob.get().enable_foyer),
                    move || store.update_knob(|k| k.enable_foyer = !k.enable_foyer),
                ))
                .child(chip_toggle(
                    "TailCut 对冲",
                    computed(move || store.knob.get().enable_tail_cut),
                    move || store.update_knob(|k| k.enable_tail_cut = !k.enable_tail_cut),
                ))
                .child(chip_toggle(
                    "HTTP 连接复用",
                    computed(move || store.knob.get().keep_alive),
                    move || store.update_knob(|k| k.keep_alive = !k.keep_alive),
                ))
                .child(chip_toggle(
                    "并发 -4",
                    computed(|| false),
                    move || store.update_knob(|k| k.concurrent = k.concurrent.saturating_sub(4).max(2)),
                ))
                .child(chip_toggle(
                    "并发 +8",
                    computed(|| false),
                    move || store.update_knob(|k| k.concurrent = (k.concurrent + 8).min(128)),
                ))
                .child(
                    text(move || {
                        let k = store.knob.get();
                        let scheme = store.profile.with(|p| p.target.scheme.clone());
                        format!(
                            "scheme={scheme} | concurrent={} | chunk={}KiB | gap={}KiB",
                            k.concurrent,
                            k.chunk_bytes / 1024,
                            k.range_gap_bytes / 1024
                        )
                    })
                    .text_sm()
                    .tabular_nums()
                    .text_color(move || theme().muted_foreground),
                ),
        )
        // 3. 三列根因、放大系数与宿主机 KPI 卡片
        .child(
            view()
                .flex_row()
                .gap_3()
                .child(kpi_card(
                    "主瓶颈自动定责",
                    computed(move || {
                        store.frame.with(|f| {
                            if f.diagnosis.headline.is_empty() {
                                SharedString::from("待采样")
                            } else {
                                SharedString::from(f.diagnosis.headline.clone())
                            }
                        })
                    }),
                    computed(move || {
                        store
                            .frame
                            .with(|f| SharedString::from(f.diagnosis.recommendation.clone()))
                    }),
                ))
                .child(kpi_card(
                    "连接复用与读写放大",
                    computed(move || {
                        store.frame.with(|f| {
                            SharedString::from(format!(
                                "复用 {:.0}% · 读放大 {:.2}x · RPC {:.2}x",
                                f.transport.socket_reuse_pct,
                                f.transport.read_amplification,
                                f.transport.rpc_amplification
                            ))
                        })
                    }),
                    computed(move || {
                        store.frame.with(|f| {
                            SharedString::from(format!(
                                "缓存命中 {:.0}% · 429/503 限流 {}",
                                f.transport.cache_hit_ratio_pct,
                                f.transport.http_429_count + f.transport.http_503_slowdown_count
                            ))
                        })
                    }),
                ))
                .child(kpi_card(
                    "吞吐与压测机自证",
                    computed(move || {
                        store.frame.with(|f| {
                            SharedString::from(format!(
                                "{:.1} MiB/s ({:.1}/核)",
                                f.transport.aggregate_mib_s, f.transport.mib_per_cpu_core
                            ))
                        })
                    }),
                    computed(move || {
                        store.frame.with(|f| {
                            SharedString::from(format!(
                                "RSS {} MiB · CRC32C 校验通过 {}",
                                f.transport.process_rss_mib, f.transport.crc32c_verified_ok
                            ))
                        })
                    }),
                )),
        )
        // 4. 六段式白盒耗时分解
        .child(section_frame(
            "1. 单请求六段式白盒耗时归因 (Service + HttpTransport + Socket/TLS)",
            text(move || {
                store.frame.with(|f| {
                    let w = &f.waterfall;
                    format!(
                        "排队 {:.2}ms | 缓存 {:.2}ms | DNS/TCP/TLS {:.2}ms | 远端 TTFB {:.2}ms | 分块流 {:.2}ms | 合并/CRC {:.2}ms (总计 {:.2}ms)",
                        w.semaphore_wait_us as f64 / 1000.0,
                        w.cache_lookup_us as f64 / 1000.0,
                        w.conn_tls_sign_us as f64 / 1000.0,
                        w.remote_ttfb_us as f64 / 1000.0,
                        w.body_stream_us as f64 / 1000.0,
                        w.finalize_crc_us as f64 / 1000.0,
                        w.total_ms()
                    )
                })
            })
            .text_sm()
            .tabular_nums(),
        ))
        // 5. 分操作语义独立 HDR 分位表
        .child(section_frame(
            "2. 分操作语义延迟分位谱 (绝不混算 stat 与大对象读写流)",
            text(move || {
                store.frame.with(|f| {
                    if f.verbs.is_empty() {
                        return "尚无采样，请按 Space 启动单点剖析或按 O 运行自动寻优。".to_owned();
                    }
                    let mut lines = vec![format!(
                        "{:<24} {:>7} {:>9} {:>9} {:>9} {:>9} {:>10} {:>8}",
                        "OPERATION", "COUNT", "OPS/s", "P50(ms)", "P99(ms)", "P99.9", "STREAM", "429/503"
                    )];
                    for v in &f.verbs {
                        lines.push(format!(
                            "{:<24} {:>7} {:>9.1} {:>9.2} {:>9.2} {:>9.2} {:>7.1}MiB {:>7.1}%",
                            v.verb,
                            v.count,
                            v.ops_per_sec,
                            v.p50_ms,
                            v.p99_ms,
                            v.p999_ms,
                            v.single_stream_mib_s,
                            v.throttle_429_503_pct
                        ));
                    }
                    lines.join("\n")
                })
            })
            .text_sm()
            .tabular_nums(),
        ))
        // 6. 四页签深层检查器
        .child(
            view()
                .flex_1()
                .gap_2()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(chip_toggle(
                            "AwaitTree 慢协程栈",
                            computed(move || store.active_tab.get() == 0),
                            move || store.active_tab.set(0),
                        ))
                        .child(chip_toggle(
                            "最优 Operator 代码",
                            computed(move || store.active_tab.get() == 1),
                            move || store.active_tab.set(1),
                        ))
                        .child(chip_toggle(
                            "多云 FinOps 账单",
                            computed(move || store.active_tab.get() == 2),
                            move || store.active_tab.set(2),
                        ))
                        .child(chip_toggle(
                            "Socket/TLS 握手探针",
                            computed(move || store.active_tab.get() == 3),
                            move || store.active_tab.set(3),
                        )),
                )
                .child(
                    view()
                        .flex_1()
                        .p_3()
                        .rounded(move || theme().radius)
                        .border_1()
                        .border_color(move || theme().border)
                        .bg(move || theme().card)
                        .child(
                            text(move || store.render_inspector_text())
                                .text_sm()
                                .tabular_nums(),
                        ),
                ),
        )
        // 7. 底部交互式 `/` Minibuffer 命令栏（使用 nsh_base::input）
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    input()
                        .value(move || store.command_draft.get())
                        .on_change(move |val: &str| store.command_draft.set(val.to_owned()))
                        .on_submit(move |_: &str| store.submit_command())
                        .placeholder(
                            "命令栏 (:scheme s3 | :set bucket=demo | :conc 32 | :net https://s3.amazonaws.com | :export report)",
                        )
                        .flex_1()
                        .px_3()
                        .py_2()
                        .border_1()
                        .border_color(move || theme().input)
                        .rounded(move || theme().radius)
                        .text_color(move || theme().foreground),
                )
                .child(action_button(
                    SharedString::from("执行命令"),
                    false,
                    move || store.submit_command(),
                )),
        )
}
