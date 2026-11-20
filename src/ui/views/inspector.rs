//! The inspector panel: the selected request (every field: stages, TLS, TCP, retries, ids),
//! the selected operation and its requests, a brushed selection's outlier attribution, or the
//! run's setup when nothing is selected.

use nsh_base::theme;
use nsh_core::prelude::*;

use crate::model::{OpRow, Report, ReqRow};
use crate::ui::kit::{Kind, btn, caption, mono, prop_row, row_button, small, strong};
use crate::ui::model::fmt;
use crate::ui::model::requests::proto;
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

fn operation(s: Store, o: OpRow, reqs: Vec<(u32, ReqRow)>) -> View {
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

fn attribution(what: String, a: crate::analysis::outliers::Attribution) -> View {
    view()
        .gap_3()
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
                            operation(s, o.clone(), reqs).into_element()
                        }
                        None => small("Nothing selected.").into_element(),
                    },
                    Inspect::Attribution { what, attr } => attribution(what, attr).into_element(),
                    Inspect::None => setup(c.run.report.clone()).into_element(),
                }
            })
        }))))
}
