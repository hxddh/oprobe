//! The request heatmap: operations started per time bucket (x) and latency row (y, log), the
//! count as the cell's opacity, with a colour key from the fewest to the most operations in a
//! cell. It shares the time cursor and the selected range with the timeline ([`TimeLink`]): a
//! drag across it selects a range, and a click on a cell lists the operations behind it.

use std::rc::Rc;

use nsh_base::theme;
use nsh_core::TouchEvent;
use nsh_core::element::TouchClaim;
use nsh_core::prelude::*;

use crate::ui::chart::{Gesture, TimeLink, Unit, axes, frame_for, on_brush, paint_time_overlay};
use crate::ui::data::Heat;
use crate::ui::fmt::{self, Scale};
use crate::ui::i18n::trf;
use crate::ui::kit::{caption, paint, small};
use crate::ui::plots::heat_legend;

/// One cell of the heatmap: its time bucket and latency row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub t0_s: f64,
    pub t1_s: f64,
    pub lo_ms: f64,
    /// The row's upper bound (infinite for the open last row).
    pub hi_ms: f64,
    pub count: u64,
}

fn x_scale(h: &Heat) -> Scale {
    Scale {
        min: 0.0,
        max: h.span_s.max(h.bucket_s * h.counts.len().max(1) as f64),
        log: false,
    }
}

/// The latency rows that hold any operation (the axis covers those, not every row).
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

/// A cell's opacity for `n` operations when the fullest cell holds `max` (log scale, so one
/// operation still shows).
pub fn cell_alpha(n: u64, max: u64) -> f32 {
    let lmax = (max.max(1) as f32).ln_1p();
    0.15 + 0.85 * ((n as f32).ln_1p() / lmax).min(1.0)
}

fn area(h: &Heat, w: f32, hh: f32) -> (f32, f32, f32, f32) {
    let f = frame_for(&y_scale(h), Unit::Ms.f(), hh);
    (f.left, f.x1(w), f.y0(hh), f.top)
}

fn paint_heat(h: &Heat, w: f32, hh: f32) -> Painter {
    let x = x_scale(h);
    let y = y_scale(h);
    let f = frame_for(&y, Unit::Ms.f(), hh);
    let (x0, x1, y0, y1) = (f.left, f.x1(w), f.y0(hh), f.top);
    let base = paint("series-a");
    let cw = (x1 - x0) * (h.bucket_s / x.max) as f32;
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
            p = p
                .global_alpha(cell_alpha(*n, h.max))
                .fill_color(base)
                .fill_rect(
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

/// The cell under canvas point `at`.
fn cell_at(h: &Heat, w: f32, hh: f32, at: (f32, f32)) -> Option<Cell> {
    cell_in(h, area(h, w, hh), at)
}

/// The cell under `at` of a heatmap drawn over the plot area `(x0, x1, y0, y1)`.
fn cell_in(h: &Heat, area: (f32, f32, f32, f32), at: (f32, f32)) -> Option<Cell> {
    let y = y_scale(h);
    let (x0, x1, y0, y1) = area;
    if at.0 < x0 || at.0 > x1 || at.1 < y1 || at.1 > y0 {
        return None;
    }
    let t = x_scale(h).value(((at.0 - x0) / (x1 - x0)) as f64);
    let c = ((t / h.bucket_s.max(1e-9)) as usize).min(h.counts.len().saturating_sub(1));
    let ms = y.value(((y0 - at.1) / (y0 - y1)) as f64);
    let r = h.bounds_ms.iter().rposition(|b| *b <= ms).unwrap_or(0);
    Some(Cell {
        t0_s: c as f64 * h.bucket_s,
        t1_s: (c + 1) as f64 * h.bucket_s,
        lo_ms: h.bounds_ms[r],
        hi_ms: h.bounds_ms.get(r + 1).copied().unwrap_or(f64::INFINITY),
        count: h.counts.get(c)?.get(r).copied().unwrap_or(0),
    })
}

fn describe(c: &Cell) -> String {
    let hi = if c.hi_ms.is_finite() {
        fmt::dur_ms(c.hi_ms)
    } else {
        "∞".into()
    };
    format!(
        "{} · {}–{} · {} ops",
        fmt::clock_s(c.t0_s),
        fmt::dur_ms(c.lo_ms),
        hi,
        c.count
    )
}

/// The colour key: the fewest operations in a cell, a ramp, the most.
fn legend(h: &Heat) -> View {
    let (lo, hi) = heat_legend(h);
    view()
        .flex_row()
        .items_center()
        .gap_1()
        .flex_shrink_0()
        .test_id("heatmap-legend")
        .child(small(fmt::count(lo)).tabular_nums())
        .child(
            canvas(move || {
                let _ = theme();
                let mut p = Painter::new().fill_color(paint("series-a"));
                for i in 0..12 {
                    let n = (lo as f64
                        * ((hi.max(1) as f64 / lo.max(1) as f64).powf(i as f64 / 11.0)))
                    .round() as u64;
                    p = p
                        .global_alpha(cell_alpha(n, hi))
                        .fill_rect(i as f32 * 5.0, 1.0, 5.0, 8.0);
                }
                p.global_alpha(1.0)
            })
            .w(px(60.0))
            .h(px(10.0)),
        )
        .child(small(trf("heat.ops_per_cell", &[&fmt::count(hi)])).tabular_nums())
}

/// The heatmap: `link` shares the time cursor and range with the timeline (a drag selects a
/// range), `on_cell` hears a click on a cell.
pub fn heatmap(
    heading: &'static str,
    height: f32,
    data: Heat,
    link: Option<TimeLink>,
    on_cell: Option<Rc<dyn Fn(Cell)>>,
) -> View {
    let size = signal((0.0_f32, 0.0_f32));
    let hover = signal(None::<(f32, f32)>);
    let h = Rc::new(data);
    let (h1, h2, h3, h4, h5) = (h.clone(), h.clone(), h.clone(), h.clone(), h.clone());
    let cursor = link.map(|l| l.cursor);
    let gesture = Gesture::new();
    let label = trf(
        "heat.a11y",
        &[&heading, &fmt::clock_s(x_scale(&h).max), &fmt::count(h.max)],
    );
    let overlay = canvas(move || {
        let (w, hh) = size.get();
        let c = cursor.and_then(|c| c.get());
        let band = link.and_then(|l| l.shaded());
        if w <= 0.0 {
            return Painter::new();
        }
        paint_time_overlay(Painter::new(), &x_scale(&h3), area(&h3, w, hh), c, band)
    })
    .absolute()
    .top(px(0.0))
    .left(px(0.0))
    .right(px(0.0))
    .bottom(px(0.0))
    .test_id("heatmap")
    .role(AccessibilityRole::Image)
    .label(label)
    .on_mouse_move(move |e: &MouseEvent| {
        let at = (e.position.x.0, e.position.y.0);
        hover.set(Some(at));
        if let Some(c) = cursor {
            let (w, hh) = size.get_untracked();
            let (x0, x1, _, _) = area(&h4, w, hh);
            let t = (at.0 >= x0 && at.0 <= x1)
                .then(|| x_scale(&h4).value(((at.0 - x0) / (x1 - x0)) as f64));
            c.set(t);
        }
    })
    .on_mouse_leave(move |_: &MouseEvent| {
        hover.set(None);
        if let Some(c) = cursor {
            c.set(None);
        }
    });
    let overlay = match link {
        Some(link) => {
            overlay
                .claim_touches(TouchClaim::on_start())
                .on_touch(move |e: &TouchEvent| {
                    let (w, hh) = size.get_untracked();
                    let (x0, x1, _, _) = area(&h5, w, hh);
                    let xs = x_scale(&h5);
                    let to_t = |hx: f32| {
                        (hx >= x0 - 1.0 && hx <= x1 + 1.0)
                            .then(|| xs.value(((hx - x0) / (x1 - x0)).clamp(0.0, 1.0) as f64))
                    };
                    let tap = |x: f32, y: f32| {
                        if let (Some(f), Some(c)) = (&on_cell, cell_at(&h5, w, hh, (x, y))) {
                            f(c);
                        }
                    };
                    on_brush(e, link, gesture, &to_t, &tap);
                })
        }
        None => overlay,
    };
    view()
        .gap_1()
        .min_w(px(0.0))
        .child(
            view()
                .flex_row()
                .gap_3()
                .h(px(16.0))
                .items_center()
                .min_w(px(0.0))
                .child(caption(heading).flex_shrink_0())
                .child(legend(&h))
                .child(
                    small(move || {
                        let (w, hh) = size.get_untracked();
                        SharedString::from(
                            hover
                                .get()
                                .and_then(|at| cell_at(&h1, w, hh, at))
                                .map(|c| describe(&c))
                                .unwrap_or_default(),
                        )
                    })
                    .tabular_nums()
                    .number_of_lines(1)
                    .flex_1()
                    .min_w(px(0.0)),
                ),
        )
        .child(
            view()
                .h(px(height))
                .min_w(px(0.0))
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
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .on_layout(move |frame: &Bounds<Points>| {
                        size.set((frame.size.width.0, frame.size.height.0))
                    }),
                )
                .child(overlay),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_reads_its_bucket_and_row() {
        let h = Heat {
            bucket_s: 1.0,
            bounds_ms: vec![0.0, 1.0, 10.0, 100.0],
            counts: vec![vec![0, 3, 0, 0], vec![0, 0, 5, 0]],
            max: 5,
            span_s: 2.0,
            fine: false,
        };
        assert!(cell_alpha(1, 5) < cell_alpha(5, 5));
        assert!((cell_alpha(5, 5) - 1.0).abs() < 1e-6);
        let area = (50.0, 390.0, 130.0, 6.0);
        let (x0, x1, y0, y1) = area;
        let y = y_scale(&h);
        let py = |ms: f64| y0 - (y.norm(ms) as f32) * (y0 - y1);
        let c = cell_in(&h, area, (x0 + (x1 - x0) * 0.75, py(30.0))).unwrap();
        assert_eq!(
            (c.t0_s, c.t1_s, c.lo_ms, c.hi_ms, c.count),
            (1.0, 2.0, 10.0, 100.0, 5)
        );
        assert!(cell_in(&h, area, (0.0, 0.0)).is_none());
    }
}
