//! The endpoint probe: DNS, TCP, proxy, TLS and cold vs warm time to first byte.

use nsh_base::theme;
use nsh_core::prelude::*;
use oprobe::util::fmt_ms;

use super::workbench::failure_banner;
use crate::store::store;
use crate::ui::*;

fn results() -> impl IntoElement {
    let s = store();
    dynamic(move || {
        let Some(r) = s.probe.get() else {
            return view().into_element();
        };
        let stages = [
            ("dns", "DNS", r.dns.p50, r.dns.p99),
            ("connect", "TCP connect", r.tcp.p50, r.tcp.p99),
            (
                "queue",
                "Proxy CONNECT",
                r.proxy_connect.p50,
                r.proxy_connect.p99,
            ),
            (
                "upload",
                "TLS handshake",
                r.tls_handshake.p50,
                r.tls_handshake.p99,
            ),
            (
                "server",
                "TTFB, new connection",
                r.cold_ttfb.p50,
                r.cold_ttfb.p99,
            ),
            (
                "download",
                "TTFB, pooled connection",
                r.warm_ttfb.p50,
                r.warm_ttfb.p99,
            ),
        ];
        let max = stages.iter().map(|x| x.3).fold(0.0, f64::max).max(1e-9);
        let mut c = card("Handshake breakdown (p50 bar, p99 value)").child(muted(format!(
            "{}:{} {} · {} {} {}",
            r.host,
            r.port,
            r.proxy
                .as_ref()
                .map(|p| format!("via proxy {p}"))
                .unwrap_or_else(|| format!("→ {}", r.resolved.join(", "))),
            r.tls_version,
            r.alpn,
            r.cipher
        )));
        for (color, name, p50, p99) in stages {
            if p99 <= 0.0 {
                continue;
            }
            c = c.child(
                row()
                    .child(text(name).text_sm().w(px(180.0)))
                    .child(meter((p50 / max) as f32, stage_color(color)))
                    .child(mono(format!("{} / {}", fmt_ms(p50), fmt_ms(p99))).w(px(150.0))),
            );
        }
        let ratio = if r.warm_request_ms > 0.0 {
            r.cold_request_ms / r.warm_request_ms
        } else {
            0.0
        };
        c = c.child(
            text(format!(
                "A request on a new connection costs {} vs {} on a pooled one ({ratio:.1}×).",
                fmt_ms(r.cold_request_ms),
                fmt_ms(r.warm_request_ms)
            ))
            .text_sm()
            .font_weight(FontWeight::MEDIUM),
        );
        if let Some(skew) = r.clock_skew_s {
            let tone = if skew.abs() > 900.0 {
                Tone::Crit
            } else {
                Tone::Neutral
            };
            c = c.child(cell_tone(
                format!("Clock skew vs server: {skew:+.1}s"),
                tone,
            ));
        }
        c.into_element()
    })
}

pub fn probe_page() -> impl IntoElement {
    let s = store();
    scroll().flex_1().child(
        view()
            .p_6()
            .gap_4()
            .child(title("Probe"))
            .child(muted("Five fresh connections, each timed phase by phase, then a cold and a warm request on each. Honors HTTPS_PROXY / NO_PROXY."))
            .child(
                card("Endpoint").child(
                    row()
                        .items_end()
                        .child(field("URL", "https://s3.us-east-1.amazonaws.com", s.probe_endpoint).flex_1())
                        .child(
                            when(move || s.status.get().busy(), || btn_with("Probing…", Kind::Secondary, true, || {}))
                                .otherwise(move || btn("Run probe", Kind::Primary, move || s.probe())),
                        ),
                ),
            )
            .child(failure_banner())
            .child(results())
            .child(view().h(px(1.0)).bg(move || theme().border)),
    )
}
