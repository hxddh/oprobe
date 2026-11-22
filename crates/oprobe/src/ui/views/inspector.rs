//! The inspector panel: the selected request (every field: stages, TLS, TCP, retries, ids),
//! the selected operation and its requests, a brushed selection's outlier attribution, or the
//! run's setup when nothing is selected.

use nsh_base::theme;
use nsh_core::prelude::*;

use crate::model::{OpRow, Report, ReqRow};
use crate::ui::kit::{
    Kind, Tone, btn, caption, mono, prop_row, row_button, small, stage_color, strong,
};
use crate::ui::model::fmt;
use crate::ui::model::i18n::{tr, trf};
use crate::ui::model::requests::proto;
use crate::ui::model::waterfall::{Bar, BarKind, Waterfall, build as build_waterfall};
use crate::ui::store::{Inspect, Store, store};
use crate::ui::views::overview::stage_bar;
use crate::ui::views::requests::reveal_in_timeline;

fn group(title: &'static str) -> View {
    view().gap_1().child(caption(title))
}

fn us(v: u64) -> String {
    fmt::dur_us(v as f64)
}

fn request(s: Store, i: u32, r: ReqRow) -> View {
    let stages: Vec<(&'static str, f64)> = vec![
        ("queue", r.queue_us as f64),
        ("connect", r.connect_us() as f64),
        ("send", r.send_us as f64),
        ("ttfb", r.ttfb_us as f64),
        ("transfer", r.body_us as f64),
        ("stall", r.stall_us as f64),
    ];
    let total = r.total_us.max(1) as f64;
    let pct: Vec<(&'static str, f64)> = stages
        .iter()
        .map(|(n, v)| (*n, v / total * 100.0))
        .collect();
    let mut v = view()
        .gap_3()
        .child(
            view()
                .gap_0p5()
                .child(strong(format!(
                    "{} {}",
                    r.api,
                    if r.status == 0 {
                        "—".into()
                    } else {
                        r.status.to_string()
                    }
                )))
                .child(small(format!(
                    "request #{i} · operation {} · attempt {}",
                    r.op_id, r.attempt
                ))),
        )
        .child(stage_bar(pct))
        .child(
            group("TIMING")
                .child(prop_row("Start", format!("{:.6} s", r.t_us as f64 / 1e6)))
                .child(prop_row("Total", us(r.total_us)))
                .child(prop_row("Queue", us(r.queue_us)))
                .child(prop_row("DNS", us(r.dns_us)))
                .child(prop_row("TCP", us(r.tcp_us)))
                .child(prop_row("Proxy", us(r.proxy_us)))
                .child(prop_row("TLS", us(r.tls_us)))
                .child(prop_row("Send", us(r.send_us)))
                .child(prop_row("TTFB", us(r.ttfb_us)))
                .child(prop_row("Body", us(r.body_us)))
                .child(prop_row("Stall", us(r.stall_us))),
        )
        .child(
            group("CONNECTION")
                .child(prop_row(
                    "Connection",
                    format!(
                        "#{} ({})",
                        r.conn_id,
                        if r.reused { "reused" } else { "new" }
                    ),
                ))
                .child(prop_row(
                    "Remote",
                    r.remote
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "—".into()),
                ))
                .child(prop_row("Protocol", proto(r.protocol))),
        )
        .child(
            group("HEADERS")
                .child(prop_row("Method", r.method.to_string()))
                .child(prop_row(
                    "Request id",
                    r.request_id.clone().unwrap_or_else(|| "—".into()),
                ))
                .child(prop_row(
                    "Retry-After",
                    r.retry_after_ms
                        .map(|m| fmt::dur_ms(m as f64))
                        .unwrap_or_else(|| "—".into()),
                ))
                .child(prop_row("Sent", fmt::bytes(r.bytes_up as f64)))
                .child(prop_row("Received", fmt::bytes(r.bytes_down as f64))),
        );
    if r.error_code.is_some() || r.error.is_some() {
        v = v.child(
            group("ERROR")
                .child(prop_row(
                    "Code",
                    r.error_code
                        .map(|c| c.as_str().to_owned())
                        .unwrap_or_default(),
                ))
                .child(small(r.error.clone().unwrap_or_default()).selectable(true)),
        );
    }
    if let Some(t) = &r.tls {
        v = v.child(
            group("TLS")
                .child(prop_row("Version", t.version.clone()))
                .child(prop_row(
                    "Cipher",
                    t.cipher.clone().unwrap_or_else(|| "—".into()),
                ))
                .child(prop_row(
                    "Handshake",
                    if t.hrr {
                        "full, HelloRetryRequest"
                    } else if t.resumed {
                        "resumed"
                    } else {
                        "full"
                    },
                )),
        );
    }
    if let Some(t) = &r.tcp {
        v = v.child(
            group("TCP")
                .child(prop_row(
                    "RTT",
                    format!("{} ± {}", us(t.rtt_us), us(t.rttvar_us)),
                ))
                .child(prop_row("Retransmits", t.retrans.to_string()))
                .child(prop_row("cwnd", t.cwnd.to_string()))
                .child(prop_row(
                    "Delivery",
                    format!("{}/s", fmt::bytes(t.delivery_rate_bps as f64 / 8.0)),
                )),
        );
    }
    v.child(
        view()
            .flex_row()
            .gap_2()
            .child(btn(
                "Reveal in Timeline",
                || Kind::Plain,
                move || reveal_in_timeline(s, i),
            ))
            .child(btn(
                "Copy JSON",
                || Kind::Plain,
                move || {
                    if let Ok(j) = serde_json::to_string_pretty(&r) {
                        set_clipboard(j);
                    }
                },
            )),
    )
}

/// One bar of the waterfall: its label, then its stages on the operation's time axis (relative
/// widths, so the same drawing works in a window and in character cells).
fn waterfall_row(s: Store, b: Bar, total_us: u64) -> View {
    let total = total_us.max(1) as f32;
    let lead = b.start_us as f32 / total;
    let label = b.label.clone();
    let crit = b.critical;
    let failed = b.failed;
    let req = b.req;
    let label_view = text(format!("{}{}", if crit { "● " } else { "  " }, label))
        .text_size(crate::ui::kit::sz(11.0))
        .number_of_lines(1)
        .font_weight(if crit {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        })
        .text_color(move || {
            if failed {
                Tone::Crit.color()
            } else if b.kind == BarKind::Backoff {
                theme().muted_foreground
            } else {
                theme().foreground
            }
        });
    let track = view()
        .flex_row()
        .flex_1()
        .min_w(px(0.0))
        .h(px(10.0))
        .items_center()
        .child(view().w(relative(lead)).h_full())
        .children(b.segs.iter().map(move |sg| {
            view()
                .w(relative(sg.us as f32 / total))
                .min_w(px(1.0))
                .h(px(if b.kind == BarKind::Backoff { 2.0 } else { 8.0 }))
                .bg(stage_color(sg.stage))
                .label(format!("{} {}", sg.stage, fmt::dur_us(sg.us as f64)))
        }));
    let row = view()
        .flex_row()
        .items_center()
        .gap_2()
        .min_w(px(0.0))
        .child(view().w(px(150.0)).flex_shrink_0().child(label_view))
        .child(track)
        .child(
            small(fmt::dur_us(b.len_us as f64))
                .tabular_nums()
                .w(px(56.0))
                .flex_shrink_0(),
        );
    match req {
        Some(i) => view().child(
            row_button(
                format!("{label} · {}", fmt::dur_us(b.len_us as f64)),
                || false,
                move || s.inspect.set(Inspect::Request(i)),
            )
            .px(px(0.0))
            .py(px(1.0))
            .child(row),
        ),
        None => view().py(px(1.0)).child(row),
    }
}

/// The per-operation waterfall (design v5 §9 trace drilldown): the requests with their stages,
/// retries with their backoff, the critical path (●), and the comparison with a typical
/// operation of the same verb.
fn waterfall(s: Store, w: Waterfall) -> View {
    let total = w.total_us;
    view()
        .gap_1()
        .test_id("waterfall")
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(caption(tr("app.waterfall")).flex_1())
                .child(small(format!("● {}", tr("app.critical_path")))),
        )
        .children(w.bars.into_iter().map(move |b| waterfall_row(s, b, total)))
        .child(w.typical.map(|t| {
            let line = trf(
                "app.typical_line",
                &[
                    ("verb", &w.verb),
                    ("total", &fmt::dur_us(t.latency_us as f64)),
                    ("requests", &t.requests),
                ],
            );
            let slower = (t.ratio >= 1.5)
                .then(|| trf("app.slower_by", &[("x", &format!("{:.1}", t.ratio))]));
            view()
                .gap_0p5()
                .pt_1()
                .child(small(line))
                .child(slower.map(|x| small(x).text_color(move || Tone::Warn.color())))
                .child(view().flex_row().child(btn(
                    tr("app.typical_op"),
                    || Kind::Plain,
                    move || s.inspect_op(t.op_index),
                )))
        }))
}

fn operation(s: Store, o: OpRow, reqs: Vec<(u32, ReqRow)>, all_ops: &[OpRow]) -> View {
    let refs: Vec<(u32, &ReqRow)> = reqs.iter().map(|(i, r)| (*i, r)).collect();
    let wf = build_waterfall(&o, &refs, all_ops);
    let b = o.breakdown;
    let total = o.latency_us.max(1) as f64;
    let parts = vec![
        ("queue", b.queue_us as f64 / total * 100.0),
        ("cache", b.cache_us as f64 / total * 100.0),
        ("server", b.backend_us as f64 / total * 100.0),
        ("retry", b.retry_us as f64 / total * 100.0),
        ("client", b.client_us as f64 / total * 100.0),
    ];
    view()
        .gap_3()
        .child(
            view()
                .gap_0p5()
                .child(strong(format!(
                    "{} · {}",
                    o.verb,
                    fmt::dur_us(o.latency_us as f64)
                )))
                .child(mono(o.key.clone()).selectable(true))
                .child(small(format!(
                    "operation {} · {} requests · {} attempts{}",
                    o.op_id,
                    o.requests,
                    o.attempts,
                    if o.throttled { " · throttled" } else { "" }
                ))),
        )
        .child(stage_bar(parts))
        .child(waterfall(s, wf))
        .child(
            group("OPERATION")
                .child(prop_row("Start", format!("{:.6} s", o.t_us as f64 / 1e6)))
                .child(prop_row("Bytes", fmt::bytes(o.bytes as f64)))
                .child(prop_row(
                    "Result",
                    if o.ok {
                        "ok".to_owned()
                    } else {
                        o.error
                            .clone()
                            .or(o.error_code.map(|c| c.as_str().to_owned()))
                            .unwrap_or_else(|| "failed".into())
                    },
                ))
                .child(prop_row("Queue", us(b.queue_us)))
                .child(prop_row("Cache", us(b.cache_us)))
                .child(prop_row("Backend", us(b.backend_us)))
                .child(prop_row("Retry", us(b.retry_us)))
                .child(prop_row("Client", us(b.client_us))),
        )
        .child(
            group("REQUESTS").children(reqs.into_iter().map(move |(i, r)| {
                let label = format!(
                    "{} {} · {} · {}",
                    r.api,
                    if r.status == 0 {
                        "—".into()
                    } else {
                        r.status.to_string()
                    },
                    fmt::dur_us(r.total_us as f64),
                    if r.reused { "warm" } else { "cold" }
                );
                row_button(
                    label.clone(),
                    || false,
                    move || s.inspect.set(Inspect::Request(i)),
                )
                .child(small(label).text_color(move || theme().foreground))
            })),
        )
        .child(o.await_tree.map(|t| {
            group("CALL STACK WHILE SLOW").child(mono(t).text_size(px(10.0)).selectable(true))
        }))
}

fn attribution(s: Store, what: String, a: crate::analysis::outliers::Attribution) -> View {
    view()
        .gap_3()
        .child(view().flex_row().child(btn(
            "Waterfall of the Slowest Operation Here",
            || Kind::Plain,
            move || s.waterfall_slowest(),
        )))
        .child(
            view()
                .gap_0p5()
                .child(strong("What is different about these requests"))
                .child(small(format!(
                    "{what} · compared with the other {}",
                    a.rest_count
                ))),
        )
        .child(if a.factors.is_empty() {
            small("Nothing stands out: these requests look like the rest.").into_element()
        } else {
            view()
                .gap_2()
                .children(a.factors.into_iter().map(|f| {
                    view()
                        .gap_0p5()
                        .child(
                            view()
                                .flex_row()
                                .gap_2()
                                .child(
                                    strong(format!("{} = {}", f.attribute, f.value))
                                        .flex_1()
                                        .min_w(px(0.0)),
                                )
                                .child(
                                    strong(format!("{}×", fmt::sig3(f.lift)))
                                        .text_color(move || theme().primary),
                                ),
                        )
                        .child(small(format!(
                            "{} of the selection ({}) vs {} of the rest",
                            fmt::ratio(f.selected_ratio),
                            f.selected_count,
                            fmt::ratio(f.rest_ratio)
                        )))
                        .child(
                            view()
                                .h(px(4.0))
                                .rounded_full()
                                .bg(move || theme().muted)
                                .child(
                                    view()
                                        .h_full()
                                        .rounded_full()
                                        .w(relative(f.selected_ratio.clamp(0.0, 1.0) as f32))
                                        .bg(move || theme().primary),
                                ),
                        )
                }))
                .into_element()
        })
}

fn setup(r: Report) -> View {
    let k = &r.knobs.requested;
    let w = &r.workload;
    view()
        .gap_3()
        .child(
            view()
                .gap_0p5()
                .child(strong(
                    r.target
                        .name
                        .clone()
                        .unwrap_or_else(|| r.target.display.clone()),
                ))
                .child(small(format!("run {} · {}", r.run.id, r.run.started_at))),
        )
        .child(
            group("WORKLOAD")
                .child(prop_row("Preset", w.preset.clone()))
                .child(prop_row(
                    "Window",
                    format!(
                        "{} (+{} warmup)",
                        fmt::dur_us(w.duration_s * 1e6),
                        fmt::dur_us(w.warmup_s * 1e6)
                    ),
                ))
                .child(prop_row(
                    "Objects",
                    format!(
                        "{} × {}",
                        w.dataset.objects_count,
                        fmt::bytes(w.dataset.object_size_bytes as f64)
                    ),
                ))
                .child(prop_row("Access", w.access.clone())),
        )
        .child(
            group("CLIENT")
                .child(prop_row("Concurrency", k.concurrency_count.to_string()))
                .child(prop_row("I/O per op", k.io_concurrency_count.to_string()))
                .child(prop_row("Chunk", fmt::bytes(k.chunk_bytes as f64)))
                .child(prop_row(
                    "HTTP",
                    format!("{:?}", k.http_version).to_lowercase(),
                ))
                .child(prop_row("Retries", k.retries_count.to_string()))
                .child(prop_row("Timeout", fmt::dur_ms(k.timeout_ms as f64))),
        )
        .child(
            group("HOST")
                .child(prop_row(
                    "Machine",
                    format!(
                        "{} {} · {} CPUs",
                        r.run.host.os, r.run.host.arch, r.run.host.cpus_count
                    ),
                ))
                .child(prop_row("oprobe", r.run.oprobe_version.clone()))
                .child(prop_row(
                    "Trace",
                    format!(
                        "{} of {} requests kept",
                        r.run.trace.requests_kept_count, r.run.trace.requests_seen_count
                    ),
                )),
        )
        // The 5.0 sections, whatever they hold (the client that made the load, distributed
        // workers, the open-loop schedule, oprobe's own cost, key dimensions): shown generically
        // so they appear as the measurement fills them.
        .children(
            [
                ("LOAD GENERATOR", serde_json::to_value(&r.client).ok()),
                ("WORKERS", serde_json::to_value(&r.workers).ok()),
                ("SCHEDULE", serde_json::to_value(&r.schedule).ok()),
                ("OPROBE ITSELF", serde_json::to_value(&r.self_stats).ok()),
                ("DIMENSIONS", serde_json::to_value(&r.dimensions).ok()),
            ]
            .into_iter()
            .filter_map(|(title, v)| {
                let rows = v.map(|v| fmt::section_rows(&v, 6)).unwrap_or_default();
                (!rows.is_empty())
                    .then(|| group(title).children(rows.into_iter().map(|(k, v)| prop_row(k, v))))
            }),
        )
        .child(small(
            "Select a request, an operation, or drag across the heatmap to inspect it.",
        ))
}

pub fn inspector() -> impl IntoElement {
    let s = store();
    view()
        .flex_1()
        .min_w(px(0.0))
        .bg(move || theme().background)
        .child(
            view()
                .flex_row()
                .items_center()
                .h(px(30.0))
                .px_3()
                .border_b_1()
                .border_color(move || theme().border)
                .child(caption("INSPECTOR").flex_1())
                .child(dynamic(move || {
                    let has = s.inspect.with(|i| *i != Inspect::None);
                    untracked(|| {
                        if has {
                            btn(
                                "Clear",
                                || Kind::Ghost,
                                move || s.inspect.set(Inspect::None),
                            )
                            .accessibility_label("Clear the inspector")
                            .into_element()
                        } else {
                            view().into_element()
                        }
                    })
                })),
        )
        .child(scroll().flex_1().child(view().p_3().child(dynamic(move || {
            let what = s.inspect.get();
            let cur = s.current.get();
            untracked(|| {
                let Some(c) = cur else {
                    return small("Nothing selected.").into_element();
                };
                match what {
                    Inspect::Request(i) => match c.run.reqs.get(i as usize) {
                        Some(r) => request(s, i, r.clone()).into_element(),
                        None => small("Nothing selected.").into_element(),
                    },
                    Inspect::Op(i) => match c.run.ops.get(i as usize) {
                        Some(o) => {
                            let reqs = c
                                .reqs_of_op_index(i)
                                .iter()
                                .map(|ri| (*ri, c.run.reqs[*ri as usize].clone()))
                                .collect();
                            operation(s, o.clone(), reqs, &c.run.ops).into_element()
                        }
                        None => small("Nothing selected.").into_element(),
                    },
                    Inspect::Attribution { what, attr } => {
                        attribution(s, what, attr).into_element()
                    }
                    Inspect::None => setup(c.run.report.clone()).into_element(),
                }
            })
        }))))
}
