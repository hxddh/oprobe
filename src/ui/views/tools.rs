//! Tools reachable from the toolbar, the palette and the Run menu: the tuner (its confirmation
//! run is stored like any run) and the handshake probe.

use nsh_base::theme;
use nsh_core::prelude::*;

use crate::ui::kit::{
    Kind, Tone, badge, body, btn, caption, empty, field, form_row, mono, num, picker, prop_row,
    small, strong, title,
};
use crate::ui::model::fmt;
use crate::ui::store::{JobKind, Phase, store};

pub fn tune_page() -> impl IntoElement {
    let s = store();
    let running = move || {
        s.live.with(|l| {
            l.as_ref()
                .is_some_and(|l| l.kind == JobKind::Tune && l.phase == Phase::Running)
        })
    };
    scroll().flex_1().child(
        view()
            .p_4()
            .gap_4()
            .max_w(px(900.0))
            .child(title("Tune"))
            .child(small("Searches client settings (concurrency ladder, then I/O, chunk, HTTP) on the shown run's target, aware of throttling. The winner is confirmed with a run that lands in the history."))
            .child(
                view()
                    .gap_2()
                    .max_w(px(520.0))
                    .child(form_row("Target", body(move || {
                        s.current.with(|c| c.as_ref().map(|c| c.run.report.target.display.clone()))
                            .unwrap_or_else(|| s.form.target.get())
                    })))
                    .child(form_row("Objective", picker("Objective", vec![
                        ("throughput".into(), "Throughput (within the SLA)".into()),
                        ("latency".into(), "Latency (p99)".into()),
                        ("cost".into(), "Cost per operation".into()),
                    ], s.tune_objective)))
                    .child(form_row("Trial length", field("Trial length", "e.g. 5s", s.tune_trial)))
                    .child(form_row("Max trials", field("Max trials", "e.g. 8", s.tune_max)))
                    .child(form_row("", view().flex_row().gap_2()
                        .child(btn(move || if running() { "Stop" } else { "Start tuning" }, move || if running() { Kind::Danger } else { Kind::Primary }, move || {
                            if running() { s.stop() } else { s.start_tune() }
                        }).accessibility_label(move || Some(SharedString::from(if running() { "Stop" } else { "Start tuning" })))))),
            )
            .child(dynamic(move || {
                let trials = s.live.with(|l| l.as_ref().filter(|l| l.kind == JobKind::Tune).map(|l| l.trials.clone())).unwrap_or_default();
                untracked(|| {
                    if trials.is_empty() {
                        return view().into_element();
                    }
                    view()
                        .gap_0p5()
                        .child(caption("TRIALS"))
                        .child(view().flex_row().gap_3().child(small("#").w(px(28.0))).child(small("Stage").w(px(110.0))).child(small("Settings").flex_1()).child(small("ops/s").w(px(70.0))).child(small("p99").w(px(70.0))).child(small("throttled").w(px(70.0))))
                        .children(trials.into_iter().map(|t| {
                            let ok = t.feasible;
                            view()
                                .flex_row()
                                .gap_3()
                                .child(num(format!("#{}", t.id)).w(px(28.0)))
                                .child(body(t.stage.clone()).w(px(110.0)))
                                .child(mono(t.knobs.summary()).flex_1().number_of_lines(1))
                                .child(num(fmt::compact(t.ops_per_s)).w(px(70.0)))
                                .child(num(fmt::dur_ms(t.p99_ms)).w(px(70.0)))
                                .child(num(fmt::ratio(t.throttle_rate)).w(px(70.0)).text_color(move || if ok { theme().foreground } else { Tone::Warn.color() }))
                        }))
                        .into_element()
                })
            }))
            .child(dynamic(move || {
                let t = s.tuned.get();
                untracked(|| match t {
                    None => view().into_element(),
                    Some(t) => {
                        let toml = t.profile_toml.clone();
                        let rust = t.rust.clone();
                        view()
                            .gap_2()
                            .child(caption("RECOMMENDED CONFIGURATION"))
                            .child(mono(t.knobs.summary()).selectable(true))
                            .child(view().flex_row().gap_2()
                                .child(btn("Save profile…", || Kind::Plain, move || crate::ui::files::save(s, "tuned.toml".into(), toml.clone().into_bytes())))
                                .child(btn("Save Rust snippet…", || Kind::Plain, move || crate::ui::files::save(s, "storage.rs".into(), rust.clone().into_bytes()))))
                            .into_element()
                    }
                })
            })),
    )
}

pub fn probe_page() -> impl IntoElement {
    let s = store();
    let running = move || {
        s.live.with(|l| {
            l.as_ref()
                .is_some_and(|l| l.kind == JobKind::Probe && l.phase == Phase::Running)
        })
    };
    scroll().flex_1().child(
        view()
            .p_4()
            .gap_4()
            .max_w(px(900.0))
            .child(title("Probe"))
            .child(small("Measures DNS, TCP, TLS and time-to-first-byte of an endpoint, cold and warm, and the clock skew signatures depend on."))
            .child(
                view()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .max_w(px(620.0))
                    .child(field("Endpoint", "https://s3.eu-west-1.amazonaws.com", s.probe_endpoint).flex_1())
                    .child(btn(move || if running() { "Probing…" } else { "Probe" }, || Kind::Primary, move || s.start_probe())),
            )
            .child(dynamic(move || {
                let r = s.probe.get();
                let failure = s.live.with(|l| l.as_ref().filter(|l| l.kind == JobKind::Probe).and_then(|l| l.failure.clone()));
                untracked(|| {
                    if let Some(f) = failure {
                        return view().gap_1().child(badge("failed", || Tone::Crit)).child(body(f.error)).into_element();
                    }
                    let Some(r) = r else {
                        return empty("No probe yet", "Enter an endpoint and press Probe.").into_element();
                    };
                    let q = |q: &crate::trace::Quantiles| format!("p50 {} · p99 {}", fmt::dur_ms(q.p50), fmt::dur_ms(q.p99));
                    view()
                        .gap_1()
                        .child(strong(format!("{} ({}:{})", r.endpoint, r.host, r.port)))
                        .child(prop_row("Resolved", r.resolved.join(", ")))
                        .child(prop_row("TLS", if r.tls { format!("{} {} {}", r.tls_version, r.cipher, r.alpn) } else { "none".into() }))
                        .child(prop_row("DNS", q(&r.dns)))
                        .child(prop_row("TCP", q(&r.tcp)))
                        .child(prop_row("TLS handshake", q(&r.tls_handshake)))
                        .child(prop_row("Cold TTFB", q(&r.cold_ttfb)))
                        .child(prop_row("Warm TTFB", q(&r.warm_ttfb)))
                        .child(prop_row("Cold request", fmt::dur_ms(r.cold_request_ms)))
                        .child(prop_row("Warm request", fmt::dur_ms(r.warm_request_ms)))
                        .child(prop_row("Clock skew", r.clock_skew_s.map(|s| format!("{s:+.1} s")).unwrap_or_else(|| "—".into())))
                        .into_element()
                })
            })),
    )
}
