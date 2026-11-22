//! Timeline: a waterfall of operations (one row each, virtualized) with their requests drawn
//! by stage on a shared time axis; zoom (buttons, slider, wheel on the axis, pinch) and pan
//! (horizontal wheel, drag on the axis); a click opens the operation's waterfall.
//!
//! It opens on the p99 operations, slowest first (U8): the ones worth reading, not ten
//! thousand dots in start order. Retries draw their backoff in the retry colour, with a legend
//! entry.

use nsh_base::{slider, theme};
use nsh_core::prelude::*;

use crate::ui::chart::{FONT, fmt_s};
use crate::ui::kit::{Kind, body, btn, empty, mono, paint, small, stage_token, swatch, token};
use crate::ui::model::fmt;
use crate::ui::model::run::RunView;
use crate::ui::model::scale::Scale;
use crate::ui::store::{Inspect, Store, store};

/// Width of the label column (points).
const LABEL_W: f32 = 200.0;
const ROW_H: f32 = 20.0;

/// The run's full extent, seconds.
fn extent(v: &RunView) -> (f64, f64) {
    (
        v.t_min_us as f64 / 1e6,
        (v.t_max_us as f64 / 1e6).max(0.001),
    )
}

/// The visible window: the store's, or the whole run.
fn window(s: Store) -> Option<(f64, f64)> {
    let full = s.current.with(|c| c.as_ref().map(|c| extent(c)))?;
    Some(s.tl_window.get().unwrap_or(full))
}

/// Zoom by `factor` (< 1 zooms in) around `at` (0–1 across the window).
pub fn zoom(s: Store, factor: f64, at: f64) {
    let Some(full) = s.current.with_untracked(|c| c.as_ref().map(|c| extent(c))) else {
        return;
    };
    let (a, b) = s.tl_window.get_untracked().unwrap_or(full);
    let w = ((b - a) * factor).clamp(1e-4, (full.1 - full.0).max(1e-3));
    let center = a + (b - a) * at.clamp(0.0, 1.0);
    let mut na = center - w * at;
    na = na.clamp(full.0, (full.1 - w).max(full.0));
    s.tl_window.set(Some((na, na + w)));
}

/// Pan by `frac` of the window (positive: later).
pub fn pan(s: Store, frac: f64) {
    let Some(full) = s.current.with_untracked(|c| c.as_ref().map(|c| extent(c))) else {
        return;
    };
    let (a, b) = s.tl_window.get_untracked().unwrap_or(full);
    let w = b - a;
    let na = (a + w * frac).clamp(full.0, (full.1 - w).max(full.0));
    s.tl_window.set(Some((na, na + w)));
}

fn x_of(win: (f64, f64), w: f32, t: f64) -> f32 {
    ((t - win.0) / (win.1 - win.0).max(1e-9)) as f32 * w
}

/// One operation's row: its span, and its requests by stage in lanes.
fn paint_row(v: &RunView, op_i: u32, win: (f64, f64), w: f32, selected: bool) -> Painter {
    let mut p = Painter::new();
    let Some(op) = v.run.ops.get(op_i as usize) else {
        return p;
    };
    let t0 = op.t_us as f64 / 1e6;
    let t1 = t0 + op.latency_us as f64 / 1e6;
    if selected {
        p = p
            .global_alpha(0.25)
            .fill_color(paint("selection"))
            .fill_rect(0.0, 0.0, w, ROW_H)
            .global_alpha(1.0);
    }
    let (x0, x1) = (x_of(win, w, t0), x_of(win, w, t1));
    if x1 < 0.0 || x0 > w {
        return p;
    }
    // The operation's span: a faint bar, red when it failed.
    p = p
        .global_alpha(0.22)
        .fill_color(paint(if op.ok { "axis" } else { "crit" }))
        .fill_rect(
            x0.max(0.0),
            3.0,
            (x1.min(w) - x0.max(0.0)).max(1.0),
            ROW_H - 6.0,
        )
        .global_alpha(1.0);
    // Requests in up to three lanes; the wait before a retry in the retry colour.
    let reqs = v.reqs_of_op_index(op_i);
    for &ri in reqs {
        let r = &v.run.reqs[ri as usize];
        if r.attempt < 2 {
            continue;
        }
        let prev = reqs
            .iter()
            .map(|&x| &v.run.reqs[x as usize])
            .find(|x| x.api == r.api && x.attempt + 1 == r.attempt);
        if let Some(pr) = prev {
            let (a, b) = (
                (pr.t_us as f64 + pr.total_us as f64) / 1e6,
                r.t_us as f64 / 1e6,
            );
            if b > a {
                let (xa, xb) = (x_of(win, w, a), x_of(win, w, b));
                if xb >= 0.0 && xa <= w {
                    p = p.fill_color(paint(stage_token("retry"))).fill_rect(
                        xa.max(0.0),
                        ROW_H / 2.0 - 0.75,
                        (xb.min(w) - xa.max(0.0)).max(1.0),
                        1.5,
                    );
                }
            }
        }
    }
    let mut lane_end = [f64::MIN; 3];
    for &ri in reqs {
        let r = &v.run.reqs[ri as usize];
        let rs = r.t_us as f64 / 1e6;
        let lane = lane_end.iter().position(|e| *e <= rs).unwrap_or(2);
        lane_end[lane] = rs + r.total_us as f64 / 1e6;
        let y = 4.0 + lane as f32 * 4.5;
        let mut t = rs;
        for (stage, us) in [
            ("queue", r.queue_us),
            ("connect", r.connect_us()),
            ("send", r.send_us),
            ("ttfb", r.ttfb_us),
            ("body", r.body_us + r.stall_us),
        ] {
            if us == 0 {
                continue;
            }
            let te = t + us as f64 / 1e6;
            let (a, b) = (x_of(win, w, t), x_of(win, w, te));
            if b >= 0.0 && a <= w {
                p = p.fill_color(paint(stage_token(stage))).fill_rect(
                    a.max(0.0),
                    y,
                    (b.min(w) - a.max(0.0)).max(1.0),
                    3.5,
                );
            }
            t = te;
        }
        if r.is_error() {
            let a = x_of(win, w, rs);
            p = p
                .fill_color(paint("crit"))
                .fill_rect(a.max(0.0), ROW_H - 4.0, 2.0, 3.0);
        }
    }
    p
}

/// The time axis strip: ticks of the visible window; wheel zooms, horizontal wheel and drag pan.
fn axis(s: Store) -> impl IntoElement {
    let width = signal(0.0_f32);
    let drag_from = signal(None::<(f32, (f64, f64))>);
    canvas(move || {
        let w = width.get();
        let Some(win) = window(s) else {
            return Painter::new();
        };
        let _ = theme();
        let sc = Scale {
            min: win.0,
            max: win.1,
            log: false,
        };
        let mut p = Painter::new()
            .font(FONT)
            .text_align(nsh_core::canvas::CanvasTextAlign::Center)
            .text_baseline(nsh_core::canvas::CanvasTextBaseline::Top)
            .stroke_color(paint("axis"))
            .line_width(1.0);
        for t in sc.ticks(((w / 100.0) as usize).clamp(2, 10)) {
            let x = x_of(win, w, t);
            p = p
                .begin_path()
                .move_to(x.round() + 0.5, 14.0)
                .line_to(x.round() + 0.5, 20.0)
                .stroke()
                .fill_color(paint("axis"))
                .fill_text(&fmt_s(t), x.clamp(16.0, (w - 22.0).max(16.0)), 1.0);
        }
        p.begin_path().move_to(0.0, 19.5).line_to(w, 19.5).stroke()
    })
    .h(px(20.0))
    .flex_1()
    .min_w(px(0.0))
    .label("Time axis: wheel zooms, drag pans")
    .on_layout(move |f: &Bounds<Points>| width.set(f.size.width.0))
    .on_wheel(move |e: &WheelEvent| {
        let (dx, dy) = e.delta.xy();
        let w = width.get_untracked().max(1.0);
        if dx.abs() > dy.abs() {
            pan(s, dx as f64 / w as f64);
        } else if dy != 0.0 {
            zoom(
                s,
                if dy > 0.0 { 1.25 } else { 0.8 },
                (e.position.x.0 / w) as f64,
            );
        }
    })
    .claim_touches(TouchClaim::on_start())
    .on_responder_grant(move |e: &ResponderEvent| {
        if let Some(win) = window(s) {
            drag_from.set(Some((e.location.x.0, win)));
        }
    })
    .on_responder_move(move |e: &ResponderEvent| {
        if let Some((x, (a, b))) = drag_from.get_untracked() {
            let w = width.get_untracked().max(1.0);
            let dt = (x - e.location.x.0) as f64 / w as f64 * (b - a);
            let full = s.current.with_untracked(|c| c.as_ref().map(|c| extent(c)));
            if let Some(full) = full {
                let na = (a + dt).clamp(full.0, (full.1 - (b - a)).max(full.0));
                s.tl_window.set(Some((na, na + (b - a))));
            }
        }
    })
    .on_responder_release(move |_: &ResponderEvent| drag_from.set(None))
}

fn legend() -> View {
    view().flex_row().gap_3().children(
        [
            ("queue", "queue"),
            ("connect", "connect"),
            ("send", "send"),
            ("ttfb", "server"),
            ("body", "transfer"),
            ("retry", "retry backoff"),
        ]
        .into_iter()
        .map(|(st, label)| {
            view()
                .flex_row()
                .items_center()
                .gap_1()
                .child(swatch(token(stage_token(st))))
                .child(small(label))
        }),
    )
}

pub fn timeline() -> impl IntoElement {
    let s = store();
    let count = move || s.tl_order.with(|o| o.0.len());
    let all = move || {
        s.current
            .with(|c| c.as_ref().map(|c| c.op_order.len()).unwrap_or(0))
    };
    let range_label = move || {
        window(s)
            .map(|(a, b)| {
                format!(
                    "{} – {} ({})",
                    fmt::clock_s(a),
                    fmt::clock_s(b),
                    fmt::dur_us((b - a) * 1e6)
                )
            })
            .unwrap_or_default()
    };
    // The zoom slider: the visible share of the run, on a log scale (0 = all, 1 = 1/10000).
    let zoom_level = move || {
        let full = s.current.with(|c| c.as_ref().map(|c| extent(c)));
        match (full, s.tl_window.get()) {
            (Some(f), Some((a, b))) => {
                let share = ((b - a) / (f.1 - f.0).max(1e-9)).clamp(1e-4, 1.0);
                (-share.log10() / 4.0) as f32
            }
            _ => 0.0,
        }
    };
    view()
        .flex_1()
        .min_w(px(0.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .child(btn("−", || Kind::Plain, move || zoom(s, 1.5, 0.0)).accessibility_label("Zoom out"))
                .child(btn("+", || Kind::Plain, move || zoom(s, 1.0 / 1.5, 0.0)).accessibility_label("Zoom in"))
                .child(
                    slider()
                        .value(move || Some(nsh_base::SliderValue::Single(zoom_level() as f64)))
                        .min(0.0)
                        .max(1.0)
                        .step(0.01)
                        .accessibility_label("Zoom")
                        .on_change(move |v: &nsh_base::SliderValue| {
                            let v = match v {
                                nsh_base::SliderValue::Single(x) => *x as f32,
                                nsh_base::SliderValue::Range(_, x) => *x as f32,
                            };
                            let v = &v;
                            let full = s.current.with_untracked(|c| c.as_ref().map(|c| extent(c)));
                            if let Some(f) = full {
                                let share = 10f64.powf(-(*v as f64) * 4.0);
                                let (a, b) = s.tl_window.get_untracked().unwrap_or(f);
                                let mid = (a + b) / 2.0;
                                let w = (f.1 - f.0) * share;
                                let na = (mid - w / 2.0).clamp(f.0, (f.1 - w).max(f.0));
                                s.tl_window.set(Some((na, na + w)));
                            }
                        })
                        .w(px(140.0))
                        .h(px(20.0))
                        .track_style(|st| st.flex_1().h(px(4.0)).rounded_full().bg(move || theme().input))
                        .range_style(|st| st.h_full().rounded_full().bg(move || theme().primary))
                        .thumb_style(|st| {
                            st.size(px(14.0))
                                .rounded_full()
                                .bg(move || theme().background)
                                .border_1()
                                .border_color(move || theme().input)
                                .shadow_sm()
                        }),
                )
                .child(btn("Fit", || Kind::Plain, move || s.tl_window.set(None)).accessibility_label("Fit the whole run"))
                .child(
                    btn(
                        move || if s.tl_p99_only.get() { "p99 operations" } else { "All operations" },
                        || Kind::Plain,
                        move || s.tl_p99_only.update(|v| *v = !*v),
                    )
                    .accessibility_label("Show only the p99 operations, or all"),
                )
                .child(
                    btn(
                        move || if s.tl_slowest.get() { "Slowest first" } else { "By start time" },
                        || Kind::Plain,
                        move || s.tl_slowest.update(|v| *v = !*v),
                    )
                    .accessibility_label("Order the operations by duration or by start"),
                )
                .child(small(range_label).tabular_nums().test_id("timeline-range"))
                .child(view().flex_1())
                .child(legend()),
        )
        .child(
            view()
                .flex_row()
                .border_b_1()
                .border_color(move || theme().border)
                .child(
                    view()
                        .w(px(LABEL_W))
                        .px_3()
                        .justify_center()
                        .child(small(move || {
                            if count() == all() {
                                format!("{} operations", fmt::compact(count() as f64))
                            } else {
                                format!("{} of {} operations", fmt::compact(count() as f64), fmt::compact(all() as f64))
                            }
                        })),
                )
                .child(axis(s)),
        )
        .child(dynamic(move || {
            let n = count();
            untracked(|| {
                if n == 0 {
                    return empty("No operations", "This run kept no operation rows.").into_element();
                }
                let width = signal(0.0_f32);
                let list = element_ref();
                // An operation inspected elsewhere (Reveal in Timeline): bring its row into view.
                effect(move || {
                    let Inspect::Op(i) = s.inspect.get() else { return };
                    let pos = untracked(|| s.tl_order.with(|o| o.0.iter().position(|x| *x == i)));
                    if let Some(pos) = pos {
                        list.scroll_to_index(pos as u32, percentage(0.3), px(0.0), true);
                    }
                });
                view()
                    .flex_1()
                    .on_pinch(move |e: &PinchEvent| zoom(s, 1.0 / (1.0 + e.delta.0 as f64), 0.5))
                    .child(
                        virtual_list(count, move |at| {
                            let op_i = move || s.tl_order.with(|o| o.0.get(at.index()).copied());
                            let label = move || {
                                let Some(i) = op_i() else { return String::new() };
                                s.current.with(|c| {
                                    let c = c.as_ref()?;
                                    let o = c.run.ops.get(i as usize)?;
                                    Some(format!("{} {}", o.verb, o.key.rsplit('/').next().unwrap_or("")))
                                })
                                .unwrap_or_default()
                            };
                            let lat = move || {
                                let Some(i) = op_i() else { return String::new() };
                                s.current.with(|c| {
                                    let c = c.as_ref()?;
                                    c.run.ops.get(i as usize).map(|o| fmt::dur_us(o.latency_us as f64))
                                })
                                .unwrap_or_default()
                            };
                            nsh_base::button()
                                .flex_row()
                                .h(px(ROW_H))
                                .items_center()
                                .accessibility_label(move || Some(SharedString::from(format!("{} · {}", label(), lat()))))
                                .on_press(move || {
                                    if let Some(i) = op_i() {
                                        s.inspect_op(i);
                                    }
                                })
                                .hover(|st| st.bg(move || theme().secondary))
                                .focus_visible(|st| st.outline(px(1.0)).outline_color(move || theme().ring))
                                .child(
                                    view()
                                        .w(px(LABEL_W))
                                        .px_3()
                                        .flex_row()
                                        .gap_2()
                                        .child(mono(label).text_size(px(11.0)).flex_1().number_of_lines(1))
                                        .child(small(lat).tabular_nums()),
                                )
                                .child(
                                    canvas(move || {
                                        let w = width.get();
                                        let win = window(s);
                                        let sel = s.inspect.with(|i| match i {
                                            Inspect::Op(o) => Some(*o),
                                            _ => None,
                                        });
                                        let _ = theme();
                                        let i = op_i();
                                        s.current.with(|c| match (c, win, i) {
                                            (Some(c), Some(win), Some(i)) if w > 0.0 => {
                                                paint_row(c, i, win, w, sel == Some(i))
                                            }
                                            _ => Painter::new(),
                                        })
                                    })
                                    .flex_1()
                                    .h(px(ROW_H))
                                    .on_layout(move |f: &Bounds<Points>| width.set(f.size.width.0)),
                                )
                        })
                        .estimated_item_size(px(ROW_H))
                        .element_ref(&list)
                        .flex_1(),
                    )
                    .into_element()
            })
        }))
        .child(
            view()
                .px_3()
                .py_1()
                .border_t_1()
                .border_color(move || theme().border)
                .child(body("Each row is an operation; bars are its requests by stage, the thin line a retry's backoff. Press a row for its waterfall; wheel or drag on the axis to zoom and pan.").text_size(crate::ui::kit::sz(11.0)).text_color(move || theme().muted_foreground)),
        )
}
