//! The request heatmap: operations started per time bucket (x) and latency row (y, log), the
//! count as the cell's opacity; hovering reads a cell out.

use nsh_base::theme;
use nsh_core::prelude::*;

use crate::ui::chart::{Unit, axes, frame_for};
use crate::ui::data::Heat;
use crate::ui::fmt::{self, Scale};
use crate::ui::kit::{caption, paint, small};

fn scales(h: &Heat) -> (Scale, Scale) {
    let x = Scale {
        min: 0.0,
        max: h.bucket_s * h.counts.len().max(1) as f64,
        log: false,
    };
    let lo = h.bounds_ms.get(1).copied().unwrap_or(0.1).max(0.05);
    let hi = h.bounds_ms.last().copied().unwrap_or(1000.0).max(lo * 10.0);
    (x, Scale::log(lo, hi))
}

/// The latency rows that hold any operation (the axis covers those, not all twenty).
fn used(h: &Heat) -> (usize, usize) {
    let rows = h.bounds_ms.len();
    let any = |r: usize| h.counts.iter().any(|c| c.get(r).copied().unwrap_or(0) > 0);
    let first = (0..rows).find(|r| any(*r)).unwrap_or(0);
    let last = (0..rows)
        .rev()
        .find(|r| any(*r))
        .unwrap_or(rows.saturating_sub(1));
    (first, last)
}

fn y_scale(h: &Heat) -> Scale {
    let (first, last) = used(h);
    let lo = h.bounds_ms.get(first).copied().unwrap_or(0.1).max(0.05);
    let hi = h
        .bounds_ms
        .get(last + 1)
        .copied()
        .unwrap_or_else(|| h.bounds_ms.last().copied().unwrap_or(1000.0) * 2.0);
    Scale::log(lo, hi.max(lo * 10.0))
}

fn paint_heat(h: &Heat, w: f32, hh: f32) -> Painter {
    let (x, _) = scales(h);
    let y = y_scale(h);
    let f = frame_for(&y, Unit::Ms.f(), hh);
    let (x0, x1, y0, y1) = (f.left, f.x1(w), f.y0(hh), f.top);
    let base = paint("series-a");
    let cw = (x1 - x0) / h.counts.len().max(1) as f32;
    let lmax = (h.max.max(1) as f32).ln_1p();
    let mut p = Painter::new();
    for (c, col) in h.counts.iter().enumerate() {
        for (r, n) in col.iter().enumerate() {
            if *n == 0 {
                continue;
            }
            let lo = h.bounds_ms[r].max(y.min);
            let hi = h.bounds_ms.get(r + 1).copied().unwrap_or(y.max);
            let py_lo = y0 - (y.norm(lo) as f32) * (y0 - y1);
            let py_hi = y0 - (y.norm(hi) as f32) * (y0 - y1);
            let a = 0.15 + 0.85 * ((*n as f32).ln_1p() / lmax);
            p = p.global_alpha(a).fill_color(base).fill_rect(
                x0 + c as f32 * cw,
                py_hi,
                cw.max(1.0),
                (py_lo - py_hi).max(1.0),
            );
        }
    }
    axes(
        p.global_alpha(1.0),
        f,
        w,
        hh,
        &x,
        &y,
        Unit::Clock.f(),
        Unit::Ms.f(),
    )
}

/// What the cell under canvas point `at` holds.
fn read_out(h: &Heat, w: f32, hh: f32, at: (f32, f32)) -> Option<String> {
    let y = y_scale(h);
    let f = frame_for(&y, Unit::Ms.f(), hh);
    let (x0, x1, y0, y1) = (f.left, f.x1(w), f.y0(hh), f.top);
    if at.0 < x0 || at.0 > x1 || at.1 < y1 || at.1 > y0 {
        return None;
    }
    let c = (((at.0 - x0) / (x1 - x0)) * h.counts.len() as f32) as usize;
    let ms = y.value(((y0 - at.1) / (y0 - y1)) as f64);
    let r = h.bounds_ms.iter().rposition(|b| *b <= ms).unwrap_or(0);
    let n = h.counts.get(c)?.get(r).copied().unwrap_or(0);
    let hi = h
        .bounds_ms
        .get(r + 1)
        .map(|b| fmt::dur_ms(*b))
        .unwrap_or_else(|| "∞".into());
    Some(format!(
        "{} · {}–{} · {n} ops",
        fmt::clock_s(c as f64 * h.bucket_s),
        fmt::dur_ms(h.bounds_ms[r]),
        hi
    ))
}

pub fn heatmap(heading: &'static str, height: f32, data: Heat) -> View {
    let size = signal((0.0_f32, 0.0_f32));
    let hover = signal(None::<(f32, f32)>);
    let h = std::rc::Rc::new(data);
    let (h1, h2) = (h.clone(), h);
    view()
        .gap_1()
        .min_w(px(0.0))
        .child(
            view()
                .flex_row()
                .gap_3()
                .h(px(16.0))
                .items_center()
                .child(caption(heading).flex_shrink_0())
                .child(
                    small(move || {
                        let (w, hh) = size.get_untracked();
                        SharedString::from(
                            hover
                                .get()
                                .and_then(|at| read_out(&h1, w, hh, at))
                                .unwrap_or_default(),
                        )
                    })
                    .tabular_nums()
                    .number_of_lines(1),
                ),
        )
        .child(
            canvas(move || {
                let (w, hh) = size.get();
                let _ = theme();
                if w > 0.0 {
                    paint_heat(&h2, w, hh)
                } else {
                    Painter::new()
                }
            })
            .h(px(height))
            .min_w(px(0.0))
            .test_id("heatmap")
            .on_layout(move |frame: &Bounds<Points>| {
                size.set((frame.size.width.0, frame.size.height.0))
            })
            .on_mouse_move(move |e: &MouseEvent| hover.set(Some((e.position.x.0, e.position.y.0))))
            .on_mouse_leave(move |_: &MouseEvent| hover.set(None)),
        )
}
