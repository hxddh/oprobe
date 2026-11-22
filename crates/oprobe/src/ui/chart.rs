//! Honest charts on a canvas: real axes with units and nice ticks, a log latency axis, stepped
//! lines (never smoothed), a hover crosshair with a readout, one colour per series; and the live
//! sparkline of a running measurement. In a terminal the same drawing has labels on whole
//! cells and no grid (a braille grid drowns the data).

use nsh_base::theme;
use nsh_core::canvas::{CanvasTextAlign, CanvasTextBaseline};
use nsh_core::prelude::*;

use crate::ui::fmt::{self, Scale};
use crate::ui::kit::{self, caption, paint, small, swatch};

/// One series of a plot.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub label: &'static str,
    /// Theme token of its colour.
    pub token: &'static str,
    /// `(x, y)` sorted by x; each point holds until the next (one-second buckets).
    pub pts: Vec<(f64, f64)>,
}

/// How the values of an axis read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Ms,
    Rate,
}

impl Unit {
    pub fn f(self) -> fn(f64) -> String {
        match self {
            Unit::Ms => fmt::dur_ms,
            Unit::Rate => |v| format!("{}/s", fmt::compact(v)),
        }
    }
}

/// What a chart shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Plot {
    pub x: Scale,
    pub y: Scale,
    /// Width of a step in x units (the last step ends there).
    pub step_w: f64,
    pub y_fmt: Unit,
    pub lines: Vec<Line>,
}

impl Plot {
    /// Each line's value at `x` (the step holding it).
    pub fn values_at(&self, x: f64) -> Vec<(&'static str, Option<f64>)> {
        self.lines
            .iter()
            .map(|l| {
                let v = l
                    .pts
                    .iter()
                    .rev()
                    .find(|(px, _)| *px <= x && x < *px + self.step_w.max(1e-12))
                    .map(|p| p.1);
                (l.label, v)
            })
            .collect()
    }
}

/// Margins of the plot area inside the canvas (points).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

impl Frame {
    /// A frame whose left gutter is as wide as the widest of `labels`.
    pub fn fit<S: AsRef<str>>(labels: &[S]) -> Self {
        let term = kit::in_terminal();
        let chars = labels
            .iter()
            .map(|l| l.as_ref().chars().count())
            .max()
            .unwrap_or(0) as f32;
        let (cw, gap) = if term { (8.0, 8.0) } else { (6.6, 7.0) };
        Self {
            left: (chars * cw + gap).max(if term { 56.0 } else { 48.0 }),
            right: if term { 8.0 } else { 10.0 },
            top: if term { 0.0 } else { 6.0 },
            bottom: if term { 16.0 } else { 18.0 },
        }
    }

    pub fn x1(&self, w: f32) -> f32 {
        (w - self.right).max(self.left + 1.0)
    }
    pub fn y0(&self, h: f32) -> f32 {
        (h - self.bottom).max(self.top + 1.0)
    }
}

/// The y ticks an axis `h` points tall shows (never closer than a label's height).
pub fn y_ticks(y: &Scale, h: f32) -> Vec<f64> {
    let mut ticks = y.ticks(if h < 120.0 { 3 } else { 4 });
    let line = if kit::in_terminal() { 16.0 } else { 14.0 };
    let room = (h / line).max(2.0) as usize;
    if ticks.len() > room && y.log {
        ticks.retain(|t| (t.log10() - t.log10().round()).abs() < 1e-6);
    }
    if ticks.len() > room {
        let k = ticks.len().div_ceil(room);
        ticks = ticks.into_iter().step_by(k).collect();
    }
    ticks
}

/// The frame of a y axis of `y` (formatted by `f`) on a canvas `h` high.
pub fn frame_for(y: &Scale, f: fn(f64) -> String, h: f32) -> Frame {
    let labels: Vec<String> = y_ticks(y, h).into_iter().map(f).collect();
    Frame::fit(&labels)
}

const FONT: &str = "11px system-ui";

/// Axes, grid and tick labels over a `w`×`h` canvas (x in seconds).
pub fn axes(
    p: Painter,
    f: Frame,
    w: f32,
    h: f32,
    x: &Scale,
    y: &Scale,
    y_fmt: fn(f64) -> String,
) -> Painter {
    let (x0, x1, y0, y1) = (f.left, f.x1(w), f.y0(h), f.top);
    let (grid, axis) = (paint("grid"), paint("axis"));
    let term = kit::in_terminal();
    let mut p = p
        .font(FONT)
        .line_width(1.0)
        .text_align(CanvasTextAlign::Right)
        .text_baseline(CanvasTextBaseline::Middle);
    for t in y_ticks(y, y0 - y1) {
        let py = y0 - (y.norm(t) as f32) * (y0 - y1);
        if !term {
            p = p
                .stroke_color(grid)
                .begin_path()
                .move_to(x0, py.round() + 0.5)
                .line_to(x1, py.round() + 0.5)
                .stroke();
        }
        p = p.fill_color(axis).fill_text(&y_fmt(t), x0 - 5.0, py);
    }
    let cw = if term { 8.0 } else { 6.6 };
    let target = ((x1 - x0) / (8.0_f32 * cw).max(48.0)).clamp(2.0, 8.0) as usize;
    p = p
        .text_align(CanvasTextAlign::Center)
        .text_baseline(CanvasTextBaseline::Top);
    for t in x.ticks(target) {
        let px = x0 + (x.norm(t) as f32) * (x1 - x0);
        p = p
            .stroke_color(axis)
            .begin_path()
            .move_to(px.round() + 0.5, y0)
            .line_to(px.round() + 0.5, y0 + 3.0)
            .stroke()
            .fill_color(axis)
            .fill_text(&fmt::clock_s(t), px.min(w - 18.0), y0 + 4.0);
    }
    p.stroke_color(axis)
        .begin_path()
        .move_to(x0 + 0.5, y1)
        .line_to(x0 + 0.5, y0 + 0.5)
        .line_to(x1, y0 + 0.5)
        .stroke()
}

/// The plot's axes and its stepped lines.
pub fn paint_plot(plot: &Plot, w: f32, h: f32) -> Painter {
    let f = frame_for(&plot.y, plot.y_fmt.f(), h);
    let (x0, x1, y0, y1) = (f.left, f.x1(w), f.y0(h), f.top);
    let sx = |v: f64| x0 + (plot.x.norm(v) as f32) * (x1 - x0);
    let sy = |v: f64| y0 - (plot.y.norm(v) as f32) * (y0 - y1);
    let mut p = axes(Painter::new(), f, w, h, &plot.x, &plot.y, plot.y_fmt.f())
        .save()
        .begin_path()
        .rect(bounds(
            point(px(x0), px(y1)),
            size(px(x1 - x0), px(y0 - y1)),
        ))
        .clip();
    for l in plot.lines.iter().filter(|l| !l.pts.is_empty()) {
        p = p.stroke_color(paint(l.token)).line_width(1.5).begin_path();
        for (i, (x, y)) in l.pts.iter().enumerate() {
            let next = l.pts.get(i + 1).map(|n| n.0).unwrap_or(x + plot.step_w);
            p = if i == 0 {
                p.move_to(sx(*x), sy(*y))
            } else {
                p.line_to(sx(*x), sy(*y))
            };
            p = p.line_to(sx(next), sy(*y));
        }
        p = p.stroke();
    }
    p.restore()
}

/// The hover crosshair at canvas x `hx` (its own layer: a mouse move repaints only this).
fn paint_crosshair(plot: &Plot, w: f32, h: f32, hx: f32) -> Painter {
    let f = frame_for(&plot.y, plot.y_fmt.f(), h);
    let (x0, x1, y0, y1) = (f.left, f.x1(w), f.y0(h), f.top);
    if hx < x0 || hx > x1 {
        return Painter::new();
    }
    let xv = plot.x.value(((hx - x0) / (x1 - x0)) as f64);
    let mut p = Painter::new()
        .stroke_color(paint("axis"))
        .line_width(1.0)
        .begin_path()
        .move_to(hx.round() + 0.5, y1)
        .line_to(hx.round() + 0.5, y0)
        .stroke();
    for ((_, v), l) in plot.values_at(xv).into_iter().zip(&plot.lines) {
        if let Some(v) = v {
            let py = y0 - (plot.y.norm(v) as f32) * (y0 - y1);
            p = p
                .fill_color(paint(l.token))
                .begin_path()
                .arc(hx, py, 3.0, 0.0, std::f32::consts::TAU)
                .fill();
        }
    }
    p
}

/// A chart: its heading, legend and hover readout above the canvas.
pub fn chart(
    heading: impl IntoReactive<SharedString>,
    height: f32,
    data: impl Fn() -> Option<Plot> + 'static,
) -> View {
    let plot = computed(data);
    let size = signal((0.0_f32, 0.0_f32));
    let hover = signal(None::<f32>);
    let readout = move || -> SharedString {
        let Some(hx) = hover.get() else {
            return "".into();
        };
        let (w, h) = size.get_untracked();
        plot.with_untracked(|p| {
            let Some(p) = p else {
                return SharedString::default();
            };
            let f = frame_for(&p.y, p.y_fmt.f(), h);
            let t = ((hx - f.left) / (f.x1(w) - f.left)).clamp(0.0, 1.0) as f64;
            let xv = p.x.value(t);
            let mut s = fmt::clock_s(xv);
            for (label, v) in p.values_at(xv) {
                let v = v.map(p.y_fmt.f()).unwrap_or_else(|| "—".into());
                s += &format!(" · {label} {v}");
            }
            s.into()
        })
    };
    let legend = move || {
        plot.with(|p| {
            p.as_ref()
                .map(|p| {
                    p.lines
                        .iter()
                        .map(|l| (l.label, l.token))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    };
    view()
        .gap_1()
        .min_w(px(0.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_3()
                .h(px(16.0))
                .min_w(px(0.0))
                .child(caption(heading).number_of_lines(1).flex_shrink_0())
                .child(dynamic(move || {
                    let items = legend();
                    untracked(|| {
                        view()
                            .flex_row()
                            .gap_3()
                            .children(items.into_iter().map(|(label, tok)| {
                                view()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .child(swatch(tok))
                                    .child(small(label))
                            }))
                            .into_element()
                    })
                }))
                .child(
                    small(readout)
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
                        let (w, h) = size.get();
                        let _ = theme();
                        plot.with(|p| match p {
                            Some(p) if w > 0.0 => paint_plot(p, w, h),
                            _ => Painter::new(),
                        })
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
                .child(
                    canvas(move || {
                        let (w, h) = size.get();
                        let Some(hx) = hover.get() else {
                            return Painter::new();
                        };
                        plot.with(|p| match p {
                            Some(p) if w > 0.0 => paint_crosshair(p, w, h, hx),
                            _ => Painter::new(),
                        })
                    })
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .on_mouse_move(move |e: &MouseEvent| hover.set(Some(e.position.x.0)))
                    .on_mouse_leave(move |_: &MouseEvent| hover.set(None)),
                ),
        )
}

/// The live sparkline of a running measurement: the values so far, scaled to their maximum,
/// with the area under them tinted.
pub fn sparkline(height: f32, values: impl Fn() -> Vec<f64> + 'static) -> impl IntoElement {
    let size = signal((0.0_f32, 0.0_f32));
    let values = computed(values);
    canvas(move || {
        let (w, h) = size.get();
        let _ = theme();
        values.with(|v| {
            if v.len() < 2 || w <= 0.0 {
                return Painter::new();
            }
            let max = v.iter().copied().fold(0.0, f64::max).max(1e-9);
            let step = w / (v.len() - 1) as f32;
            let y = |x: f64| h - 1.0 - (x / max) as f32 * (h - 2.0);
            let mut p = Painter::new()
                .global_alpha(0.18)
                .fill_color(paint("series-a"))
                .begin_path()
                .move_to(0.0, h);
            for (i, x) in v.iter().enumerate() {
                p = p.line_to(i as f32 * step, y(*x));
            }
            p = p
                .line_to(w, h)
                .close_path()
                .fill()
                .global_alpha(1.0)
                .stroke_color(paint("series-a"))
                .line_width(1.5)
                .begin_path();
            for (i, x) in v.iter().enumerate() {
                let px = i as f32 * step;
                p = if i == 0 {
                    p.move_to(px, y(*x))
                } else {
                    p.line_to(px, y(*x))
                };
            }
            p.stroke()
        })
    })
    .h(px(height))
    .min_w(px(0.0))
    .test_id("sparkline")
    .on_layout(move |frame: &Bounds<Points>| size.set((frame.size.width.0, frame.size.height.0)))
}
