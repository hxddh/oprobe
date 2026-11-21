//! The Connections, Findings and Cost tabs.

use nsh_base::{
    DataTableAlign, DataTableClasses, DataTableColumn, DataTableRows, data_table, theme,
};
use nsh_core::prelude::*;

use crate::ui::kit::{Tone, body, caption, empty, num, prop_row, small, strong};
use crate::ui::model::fmt;
use crate::ui::model::run::{ConnRow, RemoteRow};
use crate::ui::store::{Sel, store};
use crate::ui::views::overview::finding_row;

fn col(key: &'static str, title: &'static str, w: f32, numeric: bool) -> DataTableColumn {
    let mut c = DataTableColumn::default();
    c.key = key.into();
    c.title = Some(title.into());
    c.width = Some(px(w));
    if numeric {
        c.align = DataTableAlign::End;
    }
    c
}

pub fn table_classes() -> DataTableClasses {
    let mut c = DataTableClasses::default();
    c.header = Some("bg-secondary border-b border-border".into());
    c.header_cell = Some("px-2".into());
    c.cell = Some("px-2".into());
    c.row_odd = Some("bg-muted".into());
    c.row_selected = Some("bg-accent".into());
    c.row_hover = Some("bg-secondary".into());
    c
}

fn opt_us(v: u64) -> String {
    if v == 0 {
        String::new()
    } else {
        fmt::dur_us(v as f64)
    }
}

fn remote_rows(r: &[RemoteRow]) -> DataTableRows {
    DataTableRows::new(r.len())
        .texts("ip", r.iter().map(|r| r.ip.clone()))
        .texts("conns", r.iter().map(|r| r.conns.to_string()))
        .texts(
            "requests",
            r.iter().map(|r| fmt::compact(r.requests as f64)),
        )
        .texts("errors", r.iter().map(|r| r.errors.to_string()))
        .texts("reuse", r.iter().map(|r| fmt::pct(r.reuse_pct)))
        .texts("p50", r.iter().map(|r| fmt::dur_ms(r.p50_ms)))
        .texts("p99", r.iter().map(|r| fmt::dur_ms(r.p99_ms)))
        .texts(
            "dial",
            r.iter().map(|r| {
                if r.dial_p50_ms > 0.0 {
                    fmt::dur_ms(r.dial_p50_ms)
                } else {
                    String::new()
                }
            }),
        )
}

fn conn_rows(c: &[ConnRow]) -> DataTableRows {
    DataTableRows::new(c.len())
        .texts("conn", c.iter().map(|c| format!("#{}", c.conn_id)))
        .texts("remote", c.iter().map(|c| c.remote.clone()))
        .texts("proto", c.iter().map(|c| c.protocol.to_owned()))
        .texts("requests", c.iter().map(|c| c.requests.to_string()))
        .texts("reused", c.iter().map(|c| c.reused.to_string()))
        .texts("errors", c.iter().map(|c| c.errors.to_string()))
        .texts("dns", c.iter().map(|c| opt_us(c.dns_us)))
        .texts("tcp", c.iter().map(|c| opt_us(c.tcp_us)))
        .texts("tls", c.iter().map(|c| opt_us(c.tls_us)))
        .texts(
            "tls_kind",
            c.iter().map(|c| {
                if c.tls_version.is_empty() {
                    c.tls_kind.to_owned()
                } else {
                    format!("{} {}", c.tls_kind, c.tls_version)
                }
            }),
        )
        .texts(
            "rtt",
            c.iter()
                .map(|c| c.rtt_us.map(|r| fmt::dur_us(r as f64)).unwrap_or_default()),
        )
        .texts(
            "retrans",
            c.iter()
                .map(|c| c.retrans.map(|r| r.to_string()).unwrap_or_default()),
        )
        .texts(
            "active",
            c.iter().map(|c| {
                format!(
                    "{} → {}",
                    fmt::clock_s(c.first_t_us as f64 / 1e6),
                    fmt::clock_s(c.last_t_us as f64 / 1e6)
                )
            }),
        )
}

pub fn connections() -> impl IntoElement {
    let s = store();
    dynamic(move || {
        let data = s.current.with(|c| {
            c.as_ref().map(|c| {
                (
                    c.remotes.clone(),
                    c.conns.clone(),
                    c.run.report.connections.clone(),
                )
            })
        });
        untracked(|| {
            let Some((remotes, conns, stats)) = data else {
                return empty("Connections show when the run has finished", "").into_element();
            };
            if conns.is_empty() {
                return empty("No connections", "This run kept no request rows.").into_element();
            }
            let n_remote = remotes.len();
            view()
                .flex_1()
                .min_w(px(0.0))
                .p_3()
                .gap_3()
                .child(
                    view()
                        .flex_row()
                        .flex_wrap()
                        .gap_4()
                        .child(kv(
                            "Dialed",
                            format!(
                                "{} connections · {} failed",
                                stats.dialed, stats.dial_failures
                            ),
                        ))
                        .child(kv("Reuse", fmt::pct(stats.reuse_pct)))
                        .child(kv(
                            "Protocol",
                            format!("h1 {} · h2 {}", stats.http1_requests, stats.http2_requests),
                        ))
                        .child(kv(
                            "TLS",
                            if stats.tls_version.is_empty() {
                                "—".to_owned()
                            } else {
                                format!("{} {}", stats.tls_version, stats.alpn)
                            },
                        )),
                )
                .child(caption(format!("BY REMOTE IP ({n_remote})")))
                .child(
                    data_table()
                        .columns(vec![
                            col("ip", "Remote IP", 160.0, false),
                            col("conns", "Conns", 64.0, true),
                            col("requests", "Requests", 80.0, true),
                            col("errors", "Errors", 64.0, true),
                            col("reuse", "Reuse", 72.0, true),
                            col("p50", "p50", 80.0, true),
                            col("p99", "p99", 80.0, true),
                            col("dial", "Dial p50", 80.0, true),
                        ])
                        .data(remote_rows(&remotes))
                        .row_height(px(22.0))
                        .font_size(px(12.0))
                        .classes(table_classes())
                        .accessibility_label("Remote IPs")
                        .h(px((24.0 + 22.0 * n_remote.min(6) as f32).max(70.0)))
                        .border_1()
                        .border_color(move || theme().border),
                )
                .child(caption(format!("BY CONNECTION ({})", conns.len())))
                .child(
                    data_table()
                        .columns(vec![
                            {
                                let mut c = col("conn", "Conn", 64.0, true);
                                c.fixed = true.into();
                                c
                            },
                            col("remote", "Remote", 150.0, false),
                            col("proto", "Proto", 54.0, false),
                            col("requests", "Requests", 72.0, true),
                            col("reused", "Reused", 64.0, true),
                            col("errors", "Errors", 60.0, true),
                            col("dns", "DNS", 72.0, true),
                            col("tcp", "TCP", 72.0, true),
                            col("tls", "TLS", 72.0, true),
                            col("tls_kind", "Handshake", 110.0, false),
                            col("rtt", "RTT", 72.0, true),
                            col("retrans", "Retrans", 64.0, true),
                            col("active", "Active", 130.0, false),
                        ])
                        .data(conn_rows(&conns))
                        .row_height(px(22.0))
                        .font_size(px(12.0))
                        .classes(table_classes())
                        .accessibility_label("Connections")
                        .flex_1()
                        .min_h(px(120.0))
                        .border_1()
                        .border_color(move || theme().border),
                )
                .into_element()
        })
    })
}

pub fn findings() -> impl IntoElement {
    let s = store();
    scroll().flex_1().child(dynamic(move || {
        let data = s
            .current
            .with(|c| c.as_ref().map(|c| c.run.report.findings.clone()));
        let live = s.sel.get() == Sel::Live;
        untracked(|| match data {
            _ if live => empty("Findings show when the run has finished", "").into_element(),
            None => view().into_element(),
            Some(f) if f.is_empty() => {
                empty("No findings", "Nothing in this run needs attention.").into_element()
            }
            Some(mut f) => {
                f.sort_by_key(|f| std::cmp::Reverse(f.severity));
                view()
                    .p_4()
                    .gap_2()
                    .children(f.into_iter().map(|f| {
                        finding_row(s, f, true)
                            .border_b_1()
                            .border_color(move || theme().border)
                    }))
                    .into_element()
            }
        })
    }))
}

/// Below this width the Cost tab's three columns become one (the terminal at 120×30 with
/// the sidebar, narrow windows): names and numbers never wrap.
const COST_COLUMNS_MIN_W: f32 = 760.0;

pub fn cost() -> impl IntoElement {
    let s = store();
    let width = signal(0.0_f32);
    scroll()
        .flex_1()
        .on_layout(move |f: &Bounds<Points>| width.set(f.size.width.0))
        .child(dynamic(move || {
            let wide = width.get() >= COST_COLUMNS_MIN_W;
            let data = s.current.with(|c| {
                c.as_ref().map(|c| {
                    (
                        c.run.report.cost.clone(),
                        crate::ui::model::metrics::storage_note(&c.run.report),
                    )
                })
            });
            untracked(|| {
                let Some((c, assumed)) = data else {
                    return empty("Cost shows when the run has finished", "").into_element();
                };
                if !c.applicable {
                    return empty(
                        "Cost not applicable",
                        c.not_applicable_reason
                            .unwrap_or_else(|| "No price table applies to this target.".into()),
                    )
                    .into_element();
                }
                let sel = c.selected.clone();
                let u = c.per_op.clone();
                view()
                    .p_4()
                    .gap_4()
                    .child(
                        view()
                            .gap_1()
                            .child(caption(format!(
                                "PROJECTED MONTHLY COST · {} · prices as of {}",
                                sel.provider, c.pricing_as_of
                            )))
                            .child(
                                strong(format!("{} / month", fmt::usd(sel.total_usd)))
                                    .text_size(px(22.0)),
                            )
                            .child(small(format!(
                                "at {} operations a month{}, {} of reads leaving the cloud",
                                fmt::compact(c.monthly_ops),
                                if assumed.is_empty() {
                                    format!(
                                        ", {} stored",
                                        fmt::bytes(c.stored_gb * 1_073_741_824.0)
                                    )
                                } else {
                                    format!(" {assumed}")
                                },
                                fmt::ratio(c.egress_fraction)
                            ))),
                    )
                    // Three columns when they fit, one under the other when they do not.
                    .child(
                        if wide {
                            view().flex_row().gap_6()
                        } else {
                            view().gap_3()
                        }
                        .child(
                            view()
                                .gap_1()
                                .flex_1()
                                .min_w(px(190.0))
                                .child(caption("BREAKDOWN"))
                                .child(prop_row("Requests", fmt::usd(sel.requests_usd)))
                                .child(prop_row("Egress", fmt::usd(sel.egress_usd)))
                                .child(prop_row("Storage", fmt::usd(sel.storage_usd)))
                                .child(prop_row("Per 1M ops", fmt::usd(sel.per_million_ops_usd))),
                        )
                        .child(
                            view()
                                .gap_1()
                                .flex_1()
                                .min_w(px(190.0))
                                .child(caption("WASTE"))
                                .child(prop_row("Failed req.", fmt::usd(sel.failed_requests_usd)))
                                .child(prop_row("Over-read", fmt::usd(sel.overread_usd)))
                                .child(prop_row("Cache saved", fmt::usd(sel.cache_savings_usd))),
                        )
                        .child(
                            view()
                                .gap_1()
                                .flex_1()
                                .min_w(px(190.0))
                                .child(caption("PER OPERATION"))
                                .child(prop_row("Reads", fmt::sig3(u.read_requests)))
                                .child(prop_row("Writes", fmt::sig3(u.write_requests)))
                                .child(prop_row("Lists", fmt::sig3(u.list_requests)))
                                .child(prop_row("Failed", fmt::sig3(u.failed_requests)))
                                .child(prop_row("Down", fmt::bytes(u.download_bytes)))
                                .child(prop_row("Over-read", fmt::bytes(u.overread_bytes))),
                        ),
                    )
                    .child(caption("THE SAME WORKLOAD ELSEWHERE"))
                    .child(view().gap_0p5().children(c.comparison.iter().map(|p| {
                        let ratio = if sel.total_usd > 0.0 {
                            p.total_usd / sel.total_usd
                        } else {
                            1.0
                        };
                        let tone = if ratio < 0.95 {
                            Tone::Ok
                        } else if ratio > 1.05 {
                            Tone::Warn
                        } else {
                            Tone::Neutral
                        };
                        view()
                            .flex_row()
                            .gap_3()
                            .min_w(px(0.0))
                            .child(
                                body(p.provider.clone())
                                    .number_of_lines(1)
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .max_w(px(180.0)),
                            )
                            .child(
                                num(fmt::usd(p.total_usd))
                                    .number_of_lines(1)
                                    .w(px(90.0))
                                    .flex_shrink_0(),
                            )
                            .child(
                                small(format!("{}× this target", fmt::sig3(ratio)))
                                    .number_of_lines(1)
                                    .text_color(move || match tone {
                                        Tone::Neutral => theme().muted_foreground,
                                        t => t.color(),
                                    }),
                            )
                    })))
                    .into_element()
            })
        }))
}

/// A name and its value on one line (summary rows).
fn kv(name: &'static str, value: impl Into<SharedString>) -> View {
    view()
        .flex_row()
        .gap_1()
        .items_center()
        .child(small(name))
        .child(num(value.into()).text_size(px(12.0)))
}
