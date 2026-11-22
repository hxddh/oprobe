//! The request heatmap (time × log latency) with a brush: drag across it to select a time
//! window and a latency floor; the selection's outlier attribution opens in the inspector.
//! The brush is keyboard-accessible too (U15): `[` `]` move it in time, `{` `}` lower or raise
//! its latency floor ([`nudge`]).

use std::rc::Rc;

use nsh_base::theme;
use nsh_core::prelude::*;

use crate::ui::chart::{Frame, axes, fmt_ms, fmt_s};
use crate::ui::kit::{caption, paint, small};
use crate::ui::model::run::ReqHeatmap;
use crate::ui::model::scale::Scale;
use crate::ui::store::Brush;

fn scales(h: &ReqHeatmap) -> (Scale, Scale) {
    let x = Scale {
        min: 0.0,
        max: h.bucket_s * h.columns as f64,
        log: false,
    };
    let lo = h.bounds_ms.first().copied().unwrap_or(0.1).max(0.05);
    let hi = h
        .bounds_ms
        .last()
        .map(|b| (b * 2.0).max(lo * 10.0))
        .unwrap_or(1000.0);
    (x, Scale::log(lo, hi))
}

/// The brush after key `key` (`[` earlier, `]` later, `{` lower floor, `}` higher floor),
/// from `brush` (a tenth of the run, from its start or end, when there is none yet).
pub fn nudge(h: &ReqHeatmap, brush: Option<Brush>, key: char) -> Option<Brush> {
    let total = h.bucket_s * h.columns as f64;
    if total <= 0.0 {
        return None;
    }
    let tenth = (total / 10.0).max(h.bucket_s);
    let mut b = brush.unwrap_or(match key {
        ']' => Brush {
            from_s: (total - tenth).max(0.0),
            to_s: total,
            min_ms: 0.0,
        },
        '[' => Brush {
            from_s: 0.0,
            to_s: tenth,
            min_ms: 0.0,
        },
        _ => Brush {
            from_s: 0.0,
            to_s: total,
            min_ms: 0.0,
        },
    });
    let w = (b.to_s - b.from_s).max(h.bucket_s);
    match (key, brush.is_some()) {
        ('[', true) => {
            b.from_s = (b.from_s - w).max(0.0);
            b.to_s = b.from_s + w;
        }
        (']', true) => {
            b.to_s = (b.to_s + w).min(total);
            b.from_s = (b.to_s - w).max(0.0);
        }
        ('{', _) => {
            b.min_ms = h
                .bounds_ms
                .iter()
                .rev()
                .find(|x| **x < b.min_ms)
                .copied()
                .unwrap_or(0.0);
            if b.min_ms <= h.bounds_ms.first().copied().unwrap_or(0.0) {
                b.min_ms = 0.0;
            }
        }
        ('}', _) => {
            if let Some(x) = h.bounds_ms.iter().find(|x| **x > b.min_ms) {
                b.min_ms = *x;
            }
        }
        _ => {}
    }
    Some(b)
}

/// Paint the heatmap and the brush.
pub fn paint_heatmap(h: &ReqHeatmap, w: f32, hh: f32, brush: Option<Brush>) -> Painter {
    let f = Frame::standard();
    let (x, y) = scales(h);
    let (x0, x1, y0, y1) = (f.x0(), f.x1(w), f.y0(hh), f.y1());
    let mut p = Painter::new();
    let base = paint("series-a");
    let rows = h.rows();
    let cw = (x1 - x0) / h.columns as f32;
    let lmax = (h.max.max(1) as f32).ln_1p();
    for c in 0..h.columns {
        for r in 0..rows {
            let n = h.at(c, r);
            if n == 0 {
                continue;
            }
            let lo = h.bounds_ms[r].max(y.min);
            let hi = h.bounds_ms.get(r + 1).copied().unwrap_or(y.max);
            let py_lo = y0 - (y.norm(lo) as f32) * (y0 - y1);
            let py_hi = y0 - (y.norm(hi) as f32) * (y0 - y1);
            let a = 0.15 + 0.85 * ((n as f32).ln_1p() / lmax);
            p = p.global_alpha(a).fill_color(base).fill_rect(
                x0 + c as f32 * cw,
                py_hi,
                cw.max(1.0),
                (py_lo - py_hi).max(1.0),
            );
        }
    }
    p = p.global_alpha(1.0);
    p = axes(p, f, w, hh, &x, &y, fmt_s, fmt_ms);
    if let Some(b) = brush {
        let bx0 = x0 + (x.norm(b.from_s) as f32) * (x1 - x0);
        let bx1 = x0 + (x.norm(b.to_s) as f32) * (x1 - x0);
        let by = if b.min_ms > 0.0 {
            y0 - (y.norm(b.min_ms) as f32) * (y0 - y1)
        } else {
            y0
        };
        p = p
            .global_alpha(0.15)
            .fill_color(paint("brush"))
            .fill_rect(bx0, y1, (bx1 - bx0).max(1.0), by - y1)
            .global_alpha(1.0)
            .stroke_color(paint("brush"))
            .line_width(1.0)
            .stroke_rect(bounds(
                point(px(bx0), px(y1)),
                size(px((bx1 - bx0).max(1.0)), px(by - y1)),
            ));
    }
    p
}

/// Where a canvas point falls: (seconds, ms).
pub fn heat_at(h: &ReqHeatmap, w: f32, hh: f32, at: (f32, f32)) -> (f64, f64) {
    let f = Frame::standard();
    let (x, y) = scales(h);
    let (x0, x1, y0, y1) = (f.x0(), f.x1(w), f.y0(hh), f.y1());
    let tx = ((at.0 - x0) / (x1 - x0)).clamp(0.0, 1.0) as f64;
    let ty = ((y0 - at.1) / (y0 - y1)).clamp(0.0, 1.0) as f64;
    (x.value(tx), y.value(ty))
}

/// A brush from two canvas points: the time between them, latency from the lower one up
/// (from zero when it is at the bottom).
fn brush_of(h: &ReqHeatmap, w: f32, hh: f32, a: (f32, f32), b: (f32, f32)) -> Brush {
    let (t0, l0) = heat_at(h, w, hh, a);
    let (t1, l1) = heat_at(h, w, hh, b);
    let lo = l0.min(l1);
    let floor = if lo <= h.bounds_ms.first().copied().unwrap_or(0.0).max(0.05) * 1.05 {
        0.0
    } else {
        lo
    };
    let from = t0.min(t1);
    Brush {
        from_s: from,
        to_s: t0.max(t1).max(from + h.bucket_s),
        min_ms: floor,
    }
}

pub fn heatmap(
    height: f32,
    data: impl Fn() -> Option<ReqHeatmap> + 'static,
    brush: Signal<Option<Brush>>,
    on_brush: impl Fn(Brush) + 'static,
) -> View {
    let h = computed(data);
    let size = signal((0.0_f32, 0.0_f32));
    let drag = signal(None::<((f32, f32), (f32, f32))>);
    let to_brush = move |a: (f32, f32), b: (f32, f32)| -> Option<Brush> {
        let (w, hh) = size.get_untracked();
        h.with_untracked(|h| h.as_ref().map(|h| brush_of(h, w, hh, a, b)))
    };
    let on_brush = Rc::new(on_brush);
    let shown = move || match drag.get() {
        Some((a, b)) => to_brush(a, b),
        None => brush.get(),
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
                .child(caption("REQUEST LATENCY OVER TIME"))
                .child(small(
                    "drag to select, or [ ] { } from the keyboard · the inspector shows what sets them apart",
                )),
        )
        .child(
            canvas(move || {
                let (w, hh) = size.get();
                let b = shown();
                let _ = theme();
                h.with(|h| match h {
                    Some(h) if w > 0.0 => paint_heatmap(h, w, hh, b),
                    _ => Painter::new(),
                })
            })
            .h(px(height))
            .min_w(px(0.0))
            .label("Request latency heatmap")
            .test_id("heatmap")
            .on_layout(move |frame: &Bounds<Points>| {
                size.set((frame.size.width.0, frame.size.height.0))
            })
            .claim_touches(TouchClaim::on_start())
            .on_responder_grant(move |e: &ResponderEvent| {
                let p = (e.location.x.0, e.location.y.0);
                drag.set(Some((p, p)));
            })
            .on_responder_move(move |e: &ResponderEvent| {
                drag.update(|d| {
                    if let Some((_, b)) = d {
                        *b = (e.location.x.0, e.location.y.0);
                    }
                })
            })
            .on_responder_release(move |e: &ResponderEvent| {
                let Some((a, _)) = drag.get_untracked() else {
                    return;
                };
                drag.set(None);
                if let Some(br) = to_brush(a, (e.location.x.0, e.location.y.0)) {
                    on_brush(br);
                }
            })
            .on_responder_terminate(move |_: &ResponderEvent| drag.set(None)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heat() -> ReqHeatmap {
        ReqHeatmap {
            bucket_s: 0.5,
            columns: 20,
            bounds_ms: vec![0.1, 1.0, 10.0, 100.0],
            counts: vec![0; 80],
            max: 0,
        }
    }

    #[test]
    fn the_brush_moves_and_resizes_from_the_keyboard() {
        let h = heat();
        let b = nudge(&h, None, ']').unwrap();
        assert_eq!((b.from_s, b.to_s, b.min_ms), (9.0, 10.0, 0.0));
        let b = nudge(&h, Some(b), '[').unwrap();
        assert_eq!((b.from_s, b.to_s), (8.0, 9.0));
        let b = nudge(&h, Some(b), '}').unwrap();
        assert_eq!(b.min_ms, 0.1);
        let b = nudge(&h, Some(b), '}').unwrap();
        assert_eq!(b.min_ms, 1.0);
        let b = nudge(&h, Some(b), '{').unwrap();
        assert_eq!(b.min_ms, 0.0);
        // It stays inside the run.
        let mut b = nudge(&h, None, '[').unwrap();
        for _ in 0..30 {
            b = nudge(&h, Some(b), '[').unwrap();
        }
        assert_eq!(b.from_s, 0.0);
        let empty = ReqHeatmap::default();
        assert!(nudge(&empty, None, '[').is_none());
    }
}
