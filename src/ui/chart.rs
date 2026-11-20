//! Honest charts on a canvas, shared by every chart of the app: real axes with units and nice
//! ticks, a log latency axis, straight or stepped lines (never smoothed), a hover crosshair
//! with a readout, one color per stage or series. Data arrives downsampled (`model::series`).
//! In a terminal the same drawing is braille dots with labels on whole cells.

use nsh_base::theme;
use nsh_core::canvas::{CanvasTextAlign, CanvasTextBaseline};
use nsh_core::prelude::*;

use crate::ui::kit::{self, caption, paint, small, swatch, token};
use crate::ui::model::scale::Scale;

/// One series of a plot.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub label: &'static str,
    /// Theme token of its color.
    pub token: &'static str,
    /// `(x, y)` sorted by x. With `step`, each point holds until the next (bucket aggregates).
    pub pts: Vec<(f64, f64)>,
    pub step: bool,
    pub dashed: bool,
}

/// The unit of an axis (how its values read).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Seconds,
    Ms,
    Rate,
    Mib,
}

impl Unit {
    pub fn f(self) -> fn(f64) -> String {
        match self {
            Unit::Seconds => fmt_s,
            Unit::Ms => fmt_ms,
            Unit::Rate => fmt_rate,
            Unit::Mib => fmt_mib,
        }
    }
}

/// What a chart shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Plot {
    pub x: Scale,
    pub y: Scale,
    /// Width of a step (the bucket), in x units; the last step ends there.
    pub step_w: f64,
    pub x_fmt: Unit,
    pub y_fmt: Unit,
    pub lines: Vec<Line>,
}

impl Plot {
    /// Each line's value at `x` (the step holding it, or the nearest point).
    pub fn values_at(&self, x: f64) -> Vec<(&'static str, &'static str, Option<f64>)> {
        self.lines
            .iter()
            .map(|l| {
                let v = if l.step {
                    l.pts
                        .iter()
                        .rev()
                        .find(|(px, _)| *px <= x && x < *px + self.step_w.max(1e-12))
                        .map(|p| p.1)
                } else {
                    l.pts
                        .iter()
                        .min_by(|a, b| (a.0 - x).abs().total_cmp(&(b.0 - x).abs()))
                        .map(|p| p.1)
                };
                (l.label, l.token, v)
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
    pub fn standard() -> Self {
        if kit::in_terminal() {
            Self {
                left: 56.0,
                right: 8.0,
                top: 0.0,
                bottom: 16.0,
            }
        } else {
            Self {
                left: 46.0,
                right: 10.0,
                top: 6.0,
                bottom: 18.0,
            }
        }
    }
    pub fn x0(&self) -> f32 {
        self.left
    }
    pub fn x1(&self, w: f32) -> f32 {
        (w - self.right).max(self.left + 1.0)
    }
    pub fn y0(&self, h: f32) -> f32 {
        (h - self.bottom).max(self.top + 1.0)
    }
    pub fn y1(&self) -> f32 {
        self.top
    }
}

const DASH: &[Points] = &[Points(4.0), Points(3.0)];

pub const FONT: &str = "11px system-ui";

/// Axes, grid and tick labels for `x` / `y` over a `w`×`h` canvas.
pub fn axes(
    p: Painter,
    f: Frame,
    w: f32,
    h: f32,
    x: &Scale,
    y: &Scale,
    x_fmt: fn(f64) -> String,
    y_fmt: fn(f64) -> String,
) -> Painter {
    let (x0, x1, y0, y1) = (f.x0(), f.x1(w), f.y0(h), f.y1());
    let grid = paint("grid");
    let axis = paint("axis");
    let mut p = p.font(FONT).line_width(1.0);
    // Horizontal grid and y labels.
    let y_ticks = y.ticks(if h < 120.0 { 3 } else { 4 });
    p = p
        .text_align(CanvasTextAlign::Right)
        .text_baseline(CanvasTextBaseline::Middle);
    for t in &y_ticks {
        let py = y0 - (y.norm(*t) as f32) * (y0 - y1);
        p = p
            .stroke_color(grid)
            .begin_path()
            .move_to(x0, py.round() + 0.5)
            .line_to(x1, py.round() + 0.5)
            .stroke()
            .fill_color(axis)
            .fill_text(&y_fmt(*t), x0 - 5.0, py);
    }
    // x labels (and short ticks).
    let target = ((x1 - x0) / 90.0).clamp(2.0, 8.0) as usize;
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
            .fill_text(&x_fmt(t), px, y0 + 4.0);
    }
    // The axes themselves.
    p.stroke_color(axis)
        .begin_path()
        .move_to(x0 + 0.5, y1)
        .line_to(x0 + 0.5, y0 + 0.5)
        .line_to(x1, y0 + 0.5)
        .stroke()
}

/// The whole plot, with a crosshair at canvas x `hover`.
pub fn paint_plot(plot: &Plot, w: f32, h: f32, hover: Option<f32>) -> Painter {
    let f = Frame::standard();
    let (x0, x1, y0, y1) = (f.x0(), f.x1(w), f.y0(h), f.y1());
    let mut p = axes(
        Painter::new(),
        f,
        w,
        h,
        &plot.x,
        &plot.y,
        plot.x_fmt.f(),
        plot.y_fmt.f(),
    );
    let sx = |v: f64| x0 + (plot.x.norm(v) as f32) * (x1 - x0);
    let sy = |v: f64| y0 - (plot.y.norm(v) as f32) * (y0 - y1);
    p = p
        .save()
        .begin_path()
        .rect(bounds(
            point(px(x0), px(y1)),
            size(px(x1 - x0), px(y0 - y1)),
        ))
        .clip();
    for l in &plot.lines {
        if l.pts.is_empty() {
            continue;
        }
        p = p
            .stroke_color(paint(l.token))
            .line_width(1.5)
            .line_dash(if l.dashed { DASH } else { &[] })
            .begin_path();
        let mut first = true;
        for (i, (x, y)) in l.pts.iter().enumerate() {
            let (px_, py) = (sx(*x), sy(*y));
            if first {
                p = p.move_to(px_, py);
                first = false;
            } else {
                p = p.line_to(px_, py);
            }
            if l.step {
                let next = l.pts.get(i + 1).map(|n| n.0).unwrap_or(x + plot.step_w);
                p = p.line_to(sx(next), py);
            }
        }
        p = p.stroke();
    }
    p = p.line_dash(&[]).restore();
    if let Some(hx) = hover.filter(|hx| *hx >= x0 && *hx <= x1) {
        let xv = plot.x.value(((hx - x0) / (x1 - x0)) as f64);
        p = p
            .stroke_color(paint("axis"))
            .line_width(1.0)
            .begin_path()
            .move_to(hx.round() + 0.5, y1)
            .line_to(hx.round() + 0.5, y0)
            .stroke();
        for (_, tok, v) in plot.values_at(xv) {
            if let Some(v) = v {
                p = p
                    .fill_color(paint(tok))
                    .begin_path()
                    .arc(hx, sy(v), 3.0, 0.0, std::f32::consts::TAU)
                    .fill();
            }
        }
    }
    p
}

/// A chart: its title, legend and hover readout above the canvas.
pub fn chart(
    heading: &'static str,
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
        let (w, _) = size.get();
        plot.with(|p| {
            let Some(p) = p else {
                return SharedString::default();
            };
            let f = Frame::standard();
            let t = ((hx - f.x0()) / (f.x1(w) - f.x0())).clamp(0.0, 1.0) as f64;
            let xv = p.x.value(t);
            let mut s = format!("at {}", (p.x_fmt.f())(xv));
            for (label, _, v) in p.values_at(xv) {
                s += &format!(
                    " · {label} {}",
                    v.map(|v| (p.y_fmt.f())(v)).unwrap_or_else(|| "—".into())
                );
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
                        .map(|l| (l.label, l.token, l.dashed))
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
                .child(caption(heading))
                .child(dynamic(move || {
                    let items = legend();
                    untracked(|| {
                        view()
                            .flex_row()
                            .gap_3()
                            .children(items.into_iter().map(|(label, tok, dashed)| {
                                view()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .child(swatch(token(tok)))
                                    .child(small(if dashed {
                                        format!("{label} (B)")
                                    } else {
                                        label.to_owned()
                                    }))
                            }))
                            .into_element()
                    })
                }))
                .child(view().flex_1())
                .child(small(readout).tabular_nums().number_of_lines(1)),
        )
        .child(
            canvas(move || {
                let (w, h) = size.get();
                let hx = hover.get();
                // Repaint when the appearance changes (token colors).
                let _ = theme();
                plot.with(|p| match p {
                    Some(p) if w > 0.0 => paint_plot(p, w, h, hx),
                    _ => Painter::new(),
                })
            })
            .h(px(height))
            .min_w(px(0.0))
            .label(heading)
            .on_layout(move |frame: &Bounds<Points>| {
                size.set((frame.size.width.0, frame.size.height.0))
            })
            .on_mouse_move(move |e: &MouseEvent| hover.set(Some(e.position.x.0)))
            .on_mouse_leave(move |_: &MouseEvent| hover.set(None)),
        )
}

/// Time axis label (seconds).
pub fn fmt_s(v: f64) -> String {
    crate::ui::model::fmt::clock_s(v)
}

/// Latency axis label (ms).
pub fn fmt_ms(v: f64) -> String {
    crate::ui::model::fmt::dur_ms(v)
}

/// Rate axis label.
pub fn fmt_rate(v: f64) -> String {
    format!("{}/s", crate::ui::model::fmt::compact(v))
}

/// MiB/s axis label.
pub fn fmt_mib(v: f64) -> String {
    format!("{} MiB/s", crate::ui::model::fmt::compact(v))
}
