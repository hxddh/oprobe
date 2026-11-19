//! The workbench: configure a target and workload, run it, and see where the time goes.

use nsh_base::plot::{area, grid, line, plot};
use nsh_base::theme;
use nsh_core::element::{RowAccessor, Rows};
use nsh_core::prelude::*;
use oprobe::analysis::diagnosis::{Severity, time_shares};
use oprobe::trace::{Protocol, Snapshot};
use oprobe::util::{fmt_bytes, fmt_ms, fmt_usd};

use crate::store::{Status, store};
use crate::ui::*;

pub const PRESETS: [(&str, &str); 5] = [
    ("parquet", "Parquet scan"),
    ("checkpoint", "Checkpoint"),
    ("small-files", "Small files"),
    ("metadata", "Metadata"),
    ("mixed", "Mixed"),
];

/// Target + workload + knobs form, shared by the workbench and the tuner.
pub fn run_form(primary_label: &'static str, on_primary: impl Fn() + Copy + 'static) -> View {
    let s = store();
    let f = s.form;
    card("Target & workload")
        .child(
            row()
                .items_end()
                .child(
                    field(
                        "Storage URL",
                        "s3://bucket/prefix · memory:// · fs:///tmp/x",
                        f.target,
                    )
                    .flex_1(),
                )
                .child(picker("Preset", &PRESETS, f.preset).w(px(170.0)))
                .child(field("Duration", "10s", f.duration).w(px(90.0)))
                .child(
                    when(
                        move || s.status.get().busy(),
                        move || btn("Stop", Kind::Danger, move || s.stop()),
                    )
                    .otherwise(move || btn(primary_label, Kind::Primary, on_primary)),
                ),
        )
        .child(
            row()
                .items_end()
                .flex_wrap()
                .child(
                    field(
                        "Endpoint (optional)",
                        "https://minio.local:9000",
                        f.endpoint,
                    )
                    .flex_1()
                    .min_w(px(200.0)),
                )
                .child(field("Region", "us-east-1", f.region).w(px(120.0)))
                .child(
                    field(
                        "Existing prefix (read-only)",
                        "warehouse/events/",
                        f.existing,
                    )
                    .w(px(220.0)),
                ),
        )
        .child(
            row()
                .items_end()
                .flex_wrap()
                .child(field("Concurrency", "16", f.concurrency).w(px(110.0)))
                .child(field("I/O per op", "4", f.io_concurrency).w(px(100.0)))
                .child(field("Chunk", "8MiB", f.chunk).w(px(100.0)))
                .child(toggle("Keep-alive", f.keep_alive))
                .child(toggle("Cache", f.cache))
                .child(toggle("Anonymous", f.anonymous))
                .child(toggle("Skip TLS verify", f.insecure)),
        )
}

/// Error banner with the actionable hint.
pub fn failure_banner() -> impl IntoElement {
    let s = store();
    dynamic(move || {
        let Some(fail) = s.failure.get() else {
            return view().into_element();
        };
        let mut v = view()
            .gap_2()
            .p_4()
            .rounded(move || px(theme().radius.0))
            .border_1()
            .border_color(token("crit"))
            .child(
                text("Something went wrong")
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(token("crit")),
            )
            .child(mono(fail.error.clone()).number_of_lines(6));
        if let Some((t, steps)) = fail.hint {
            v = v.child(heading(format!("Hint: {t}")));
            for st in steps {
                v = v.child(text(format!("• {st}")).text_sm());
            }
        }
        v.into_element()
    })
}

fn verdict() -> impl IntoElement {
    let s = store();
    dynamic(move || {
        let Some(d) = s.diagnosis() else {
            return view()
                .p_4()
                .rounded(move || px(theme().radius.0))
                .bg(move || theme().muted)
                .child(text(move || match s.status.get() {
                    Status::Running => {
                        "Preparing the dataset and warming up connections…".to_owned()
                    }
                    _ => "Choose a target and press Run. Try memory:// for a zero-setup demo."
                        .to_owned(),
                }))
                .into_element();
        };
        let tone = match d.worst() {
            Severity::Critical => Tone::Crit,
            Severity::Warning => Tone::Warn,
            Severity::Info => Tone::Ok,
        };
        let label = match tone {
            Tone::Crit => "ATTENTION",
            Tone::Warn => "REVIEW",
            _ => "HEALTHY",
        };
        row()
            .p_4()
            .rounded(move || px(theme().radius.0))
            .border_1()
            .border_color(move || tone.color())
            .child(badge(label, tone))
            .child(
                text(d.headline)
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .flex_1(),
            )
            .child(muted(move || s.status.get().label()))
            .into_element()
    })
}

fn kpis() -> impl IntoElement {
    let s = store();
    let get = move |f: fn(&Snapshot) -> String| {
        move || {
            s.snapshot
                .with(|x| x.as_ref().map(f).unwrap_or_else(|| "–".into()))
        }
    };
    row()
        .flex_wrap()
        .items_stretch()
        .child(stat(
            "Operations/s",
            get(|x| format!("{:.1}", x.summary.ops_per_s)),
            || Tone::Neutral,
        ))
        .child(stat(
            "Throughput",
            get(|x| format!("{:.1} MiB/s", x.summary.mib_per_s)),
            || Tone::Neutral,
        ))
        .child(stat("p50", get(|x| fmt_ms(x.summary.latency.p50)), || {
            Tone::Neutral
        }))
        .child(stat("p99", get(|x| fmt_ms(x.summary.latency.p99)), || {
            Tone::Neutral
        }))
        .child(stat(
            "Failed ops",
            get(|x| x.summary.failed_ops.to_string()),
            move || {
                if s.snapshot
                    .with(|x| x.as_ref().is_some_and(|x| x.summary.failed_ops > 0))
                {
                    Tone::Crit
                } else {
                    Tone::Neutral
                }
            },
        ))
        .child(stat(
            "429/503",
            get(|x| x.resilience.throttled_responses.to_string()),
            move || {
                if s.snapshot.with(|x| {
                    x.as_ref()
                        .is_some_and(|x| x.resilience.throttled_responses > 0)
                }) {
                    Tone::Warn
                } else {
                    Tone::Neutral
                }
            },
        ))
}

fn chart(
    label: &'static str,
    key: &'static str,
    pick: fn(&crate::store::Sample) -> f64,
    stroke: impl Fn() -> AdaptiveColor + Copy + 'static,
) -> View {
    let s = store();
    card(label)
        .flex_1()
        .min_w(px(260.0))
        .child(dynamic(move || {
            let h = s.history.get();
            if h.len() < 2 {
                return view()
                    .h(px(120.0))
                    .justify_center()
                    .items_center()
                    .child(muted("waiting for samples…"))
                    .into_element();
            }
            let values: Vec<f64> = h.iter().map(pick).collect();
            let last = values.last().copied().unwrap_or_default();
            let rows = Rows::from_columns(values.len(), [(key.into(), values)]);
            view()
                .gap_1()
                .child(mono(if key == "p99" {
                    fmt_ms(last)
                } else {
                    format!("{last:.1}")
                }))
                .child(
                    plot()
                        .data(rows)
                        .y(RowAccessor::key(key))
                        .interactive(false)
                        .accessibility_label(label)
                        .h(px(110.0))
                        .child(grid().stroke_color(move || theme().border))
                        .child(area().fill_color(move || stroke().opacity(0.15)))
                        .child(line().stroke_color(stroke).stroke_width(px(2.0))),
                )
                .into_element()
        }))
}

fn breakdown() -> impl IntoElement {
    let s = store();
    card("Where the time goes")
        .flex_1()
        .min_w(px(280.0))
        .child(dynamic(move || {
            let Some(snap) = s.snapshot.get() else {
                return muted("no data yet").into_element();
            };
            let sh = time_shares(&snap);
            let rows = [
                ("queue", sh.queue, "waiting for permits / connections"),
                ("connect", sh.connect, "DNS + TCP + proxy + TLS"),
                ("upload", sh.upload, "sending request bodies"),
                ("server", sh.server, "time to first byte"),
                ("download", sh.download, "receiving bodies"),
                ("client", sh.client + sh.cache, "OpenDAL, CRC, consumer"),
            ];
            let max = rows.iter().map(|r| r.1).fold(0.0, f64::max).max(1e-9);
            let mut v = view().gap_1p5();
            for (name, share, desc) in rows {
                v = v.child(
                    view()
                        .gap_0p5()
                        .child(
                            row()
                                .child(text(name).text_sm().w(px(72.0)))
                                .child(meter((share / max) as f32, stage_color(name)))
                                .child(mono(format!("{:>5.1}%", share * 100.0)).w(px(54.0))),
                        )
                        .child(muted(desc).number_of_lines(1)),
                );
            }
            v.into_element()
        }))
}

const OPS_COLS: [(&str, f32); 9] = [
    ("operation", 120.0),
    ("count", 70.0),
    ("ops/s", 70.0),
    ("MiB/s", 70.0),
    ("p50", 70.0),
    ("p99", 70.0),
    ("max", 70.0),
    ("req/op", 60.0),
    ("errors", 60.0),
];

fn operations(snap: &Snapshot) -> View {
    let mut v = view().child(table_head(&OPS_COLS));
    for o in &snap.verbs {
        v = v.child(table_row(
            &OPS_COLS,
            vec![
                cell(o.verb.clone()),
                cell(o.count.to_string()),
                cell(format!("{:.1}", o.ops_per_s)),
                cell(format!("{:.1}", o.mib_per_s)),
                cell(fmt_ms(o.latency.p50)),
                cell(fmt_ms(o.latency.p99)),
                cell(fmt_ms(o.latency.max)),
                cell(format!("{:.2}", o.requests_per_op)),
                if o.errors > 0 {
                    cell_tone(o.errors.to_string(), Tone::Crit)
                } else {
                    cell("0")
                },
            ],
        ));
    }
    v
}

const REQ_COLS: [(&str, f32); 10] = [
    ("request", 170.0),
    ("count", 64.0),
    ("warm", 52.0),
    ("queue", 64.0),
    ("connect", 64.0),
    ("send", 64.0),
    ("ttfb", 64.0),
    ("transfer", 70.0),
    ("stall", 60.0),
    ("p99", 64.0),
];

fn requests(snap: &Snapshot) -> View {
    let mut v = view().child(table_head(&REQ_COLS));
    for a in &snap.apis {
        let st = &a.stages;
        v = v.child(table_row(
            &REQ_COLS,
            vec![
                cell_tone(
                    a.api.label(),
                    if a.errors > 0 {
                        Tone::Crit
                    } else {
                        Tone::Neutral
                    },
                ),
                cell(a.count.to_string()),
                cell(format!("{:.0}%", a.reused_pct)),
                cell(fmt_ms(st.queue.mean)),
                cell(if st.connect.count > 0 {
                    fmt_ms(st.connect.mean)
                } else {
                    "–".into()
                }),
                cell(fmt_ms(st.send.mean)),
                cell(fmt_ms(st.ttfb.mean)),
                cell(fmt_ms(st.body.mean)),
                cell(fmt_ms(st.stall.mean)),
                cell(fmt_ms(st.total.p99)),
            ],
        ));
    }
    if snap.apis.is_empty() {
        v = v.child(muted("no backend requests yet"));
    }
    v
}

fn connections(snap: &Snapshot) -> View {
    let c = &snap.connections;
    let b = &snap.bytes;
    let line = |k: &'static str, val: String| {
        row()
            .child(muted(k).w(px(170.0)))
            .child(text(val).text_sm().flex_1())
    };
    view()
        .gap_1p5()
        .child(line(
            "Protocol",
            format!(
                "{} · {} · {}",
                if c.alpn.is_empty() { "–" } else { &c.alpn },
                if c.tls_version.is_empty() {
                    "plain"
                } else {
                    &c.tls_version
                },
                c.cipher
            ),
        ))
        .child(line(
            "Proxy",
            c.proxy.clone().unwrap_or_else(|| "none".into()),
        ))
        .child(line(
            "Connections",
            format!(
                "{} new · {:.1}% of requests warm · peak {} in flight",
                c.dialed, c.reuse_pct, c.peak_inflight_requests
            ),
        ))
        .child(line(
            "Dial p50 / p99",
            format!(
                "dns {}/{} · tcp {}/{} · proxy {}/{} · tls {}/{}",
                fmt_ms(c.dns.p50),
                fmt_ms(c.dns.p99),
                fmt_ms(c.tcp.p50),
                fmt_ms(c.tcp.p99),
                fmt_ms(c.proxy_connect.p50),
                fmt_ms(c.proxy_connect.p99),
                fmt_ms(c.tls.p50),
                fmt_ms(c.tls.p99)
            ),
        ))
        .child(line(
            "Remote",
            if c.remote_addrs.is_empty() {
                "–".into()
            } else {
                c.remote_addrs.join(", ")
            },
        ))
        .child(line(
            "Bytes",
            format!(
                "read {} · write {} · down {} · up {}",
                fmt_bytes(b.logical_read),
                fmt_bytes(b.logical_write),
                fmt_bytes(b.backend_down),
                fmt_bytes(b.backend_up)
            ),
        ))
        .child(line(
            "Amplification",
            format!(
                "read {:.2}× · over-read {:.2}× · {:.2} requests/op",
                b.read_amplification, b.overread_ratio, b.request_amplification
            ),
        ))
        .child(line(
            "Cache",
            if snap.cache.enabled {
                format!(
                    "{:.1}% hit · {} lookups",
                    snap.cache.hit_ratio * 100.0,
                    snap.cache.lookups
                )
            } else {
                "off".into()
            },
        ))
        .child(line(
            "Client",
            format!(
                "{:.0}% CPU of one core ({} cores) · {:.0} MiB RSS",
                snap.client.cpu_pct, snap.client.cpu_cores, snap.client.rss_mib
            ),
        ))
}

/// The slowest operation as a waterfall: one track per backend request, stages colored.
fn slowest(snap: &Snapshot) -> View {
    let Some(op) = snap.slow_ops.first() else {
        return view().child(muted("no operations yet"));
    };
    let total_us = (op.latency_ms * 1000.0).max(1.0);
    let mut v =
        view()
            .gap_2()
            .child(
                row()
                    .child(heading(format!("{} · {}", op.verb, fmt_ms(op.latency_ms))))
                    .child(muted(format!(
                        "at {:.1}s · queue {} · backend {} · client {}",
                        op.at_s,
                        fmt_ms(op.breakdown.queue_ms),
                        fmt_ms(op.breakdown.backend_ms),
                        fmt_ms(op.breakdown.client_ms + op.breakdown.cache_ms)
                    ))),
            )
            .child(row().flex_wrap().children(
                ["queue", "connect", "send", "ttfb", "transfer"].map(|k| {
                    row()
                        .gap_1()
                        .child(
                            view()
                                .w(px(10.0))
                                .h(px(10.0))
                                .rounded_sm()
                                .bg(stage_color(k)),
                        )
                        .child(muted(k))
                }),
            ));
    for (i, r) in op.requests.iter().take(24).enumerate() {
        let t = &r.timing;
        let frac = |us: u64| (us as f64 / total_us) as f32;
        let mut track = view()
            .flex_row()
            .flex_1()
            .h(px(12.0))
            .min_w(px(120.0))
            .rounded_sm()
            .bg(move || theme().muted)
            .child(view().w(relative(frac(r.start_offset_us).min(1.0))));
        for (stage, us) in [
            ("queue", t.queue_us),
            ("connect", t.connect_us()),
            ("send", t.send_us),
            ("ttfb", t.ttfb_us),
            ("transfer", t.body_us + t.stall_us),
        ] {
            if us > 0 {
                track = track.child(
                    view()
                        .h_full()
                        .w(relative(frac(us).min(1.0)))
                        .bg(stage_color(stage)),
                );
            }
        }
        let conn = match r.protocol {
            Protocol::InProcess => "local".to_owned(),
            p => format!(
                "{} #{} {}",
                if p == Protocol::Http2 { "h2" } else { "h1" },
                r.conn_id,
                if r.reused { "warm" } else { "cold" }
            ),
        };
        v = v.child(
            row()
                .child(
                    mono(format!("#{:<2} {}", i + 1, r.api.label()))
                        .w(px(190.0))
                        .number_of_lines(1),
                )
                .child(cell_tone(
                    r.status.to_string(),
                    if r.is_error() { Tone::Crit } else { Tone::Ok },
                ))
                .child(track)
                .child(
                    mono(format!("{} · {conn}", fmt_ms(t.total_us as f64 / 1000.0)))
                        .w(px(170.0))
                        .number_of_lines(1),
                ),
        );
    }
    if let Some(tree) = &op.await_tree {
        v = v
            .child(muted("Async call stack captured while in flight"))
            .child(
                view()
                    .p_3()
                    .rounded_md()
                    .bg(move || theme().muted)
                    .child(mono(tree.lines().take(18).collect::<Vec<_>>().join("\n"))),
            );
    }
    v
}

fn findings() -> View {
    let s = store();
    let Some(d) = untracked(|| s.diagnosis()) else {
        return view().child(muted("no data yet"));
    };
    let mut v = view().gap_3();
    for f in d.findings {
        let tone = match f.severity {
            Severity::Critical => Tone::Crit,
            Severity::Warning => Tone::Warn,
            Severity::Info => Tone::Info,
        };
        v = v.child(
            view()
                .gap_1()
                .child(
                    row()
                        .child(badge(f.severity.label(), tone))
                        .child(heading(f.title)),
                )
                .child(muted(f.evidence))
                .child(text(format!("→ {}", f.action)).text_sm()),
        );
    }
    v
}

fn cost() -> View {
    let s = store();
    let Some(r) = untracked(|| s.report.get()) else {
        return view().child(muted("Cost is projected when the run finishes."));
    };
    let c = &r.cost;
    let sel = &c.selected;
    let mut cmp = c.comparison.clone();
    cmp.sort_by(|a, b| a.total_usd.total_cmp(&b.total_usd));
    let max = cmp
        .iter()
        .map(|x| x.total_usd)
        .fold(0.0, f64::max)
        .max(1e-9);
    let mut v = view()
        .gap_2()
        .child(heading(format!(
            "{} — {} per 1M ops · {} / month at {:.0}M ops",
            sel.provider,
            fmt_usd(sel.per_million_ops_usd),
            fmt_usd(sel.total_usd),
            c.monthly_ops / 1e6
        )))
        .child(muted(format!(
            "requests {} · egress {} · storage {} · failed requests {} · over-read {} · cache saved {}",
            fmt_usd(sel.requests_usd),
            fmt_usd(sel.egress_usd),
            fmt_usd(sel.storage_usd),
            fmt_usd(sel.failed_requests_usd),
            fmt_usd(sel.overread_usd),
            fmt_usd(sel.cache_savings_usd)
        )))
        .child(muted(format!("prices as of {}; override in the profile's [cost.pricing]", c.pricing_as_of)));
    for p in cmp {
        v = v.child(
            row()
                .child(
                    text(p.provider.clone())
                        .text_sm()
                        .w(px(220.0))
                        .number_of_lines(1),
                )
                .child(meter((p.total_usd / max) as f32, move || theme().chart_1))
                .child(mono(fmt_usd(p.total_usd)).w(px(90.0))),
        );
    }
    v
}

fn details() -> impl IntoElement {
    let s = store();
    card("Details")
        .child(tab_row(
            s.tab,
            &[
                ("operations", "Operations"),
                ("requests", "Requests"),
                ("connections", "Connections"),
                ("slowest", "Slowest op"),
                ("findings", "Findings"),
                ("cost", "Cost"),
            ],
        ))
        .child(dynamic(move || {
            let tab = s.tab.get();
            let Some(snap) = s.snapshot.get() else {
                return muted("Run a workload to see details.").into_element();
            };
            untracked(|| match tab.as_ref() {
                "requests" => requests(&snap).into_element(),
                "connections" => connections(&snap).into_element(),
                "slowest" => slowest(&snap).into_element(),
                "findings" => findings().into_element(),
                "cost" => cost().into_element(),
                _ => operations(&snap).into_element(),
            })
        }))
}

fn log_panel() -> impl IntoElement {
    let s = store();
    card("Log").child(dynamic(move || {
        let l = s.log.get();
        let tail: Vec<SharedString> = l.iter().rev().take(6).rev().cloned().collect();
        view()
            .gap_0p5()
            .children(tail.into_iter().map(|line| mono(line).number_of_lines(1)))
            .into_element()
    }))
}

fn export_bar() -> impl IntoElement {
    let s = store();
    when(
        move || s.report.with(|r| r.is_some()),
        move || {
            row()
                .child(btn("Save report…", Kind::Secondary, move || {
                    crate::files::save_report(s)
                }))
                .child(muted(move || s.notice.get().unwrap_or_default()))
        },
    )
}

pub fn workbench() -> impl IntoElement {
    scroll().flex_1().child(
        view()
            .p_6()
            .gap_4()
            .child(
                row()
                    .justify_between()
                    .child(title("Workbench"))
                    .child(export_bar()),
            )
            .child(run_form("Run", move || store().run()))
            .child(failure_banner())
            .child(verdict())
            .child(kpis())
            .child(
                row()
                    .items_stretch()
                    .flex_wrap()
                    .child(chart(
                        "Operations / s",
                        "ops",
                        |x| x.ops,
                        move || theme().chart_1,
                    ))
                    .child(chart(
                        "p99 latency (ms)",
                        "p99",
                        |x| x.p99,
                        move || theme().chart_2,
                    ))
                    .child(breakdown()),
            )
            .child(details())
            .child(log_panel()),
    )
}
