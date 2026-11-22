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
                left: 52.0,
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

#[allow(clippy::too_many_arguments)]
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
    let mut y_ticks = y.ticks(if h < 120.0 { 3 } else { 4 });
    // Never closer than a label's height: a log axis keeps its decades first.
    let room = ((y0 - y1) / 14.0).max(2.0) as usize;
    if y_ticks.len() > room && y.log {
        y_ticks.retain(|t| (t.log10() - t.log10().round()).abs() < 1e-6);
    }
    if y_ticks.len() > room {
        let k = y_ticks.len().div_ceil(room);
        y_ticks = y_ticks.into_iter().step_by(k).collect();
    }
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
            .fill_text(&x_fmt(t), px.min(w - 18.0), y0 + 4.0);
    }
    // The axes themselves.
    p.stroke_color(axis)
        .begin_path()
        .move_to(x0 + 0.5, y1)
        .line_to(x0 + 0.5, y0 + 0.5)
        .line_to(x1, y0 + 0.5)
        .stroke()
}

/// A line projected onto the canvas: its color token, dash, and its path in canvas points.
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    pub token: &'static str,
    pub dashed: bool,
    pub pts: Vec<(f32, f32)>,
}

/// The lines of `plot` projected onto a `w`×`h` canvas (steps expanded). Computed once per
/// plot and size, not on every paint or mouse move.
pub fn project(plot: &Plot, w: f32, h: f32) -> Vec<Path> {
    let f = Frame::standard();
    let (x0, x1, y0, y1) = (f.x0(), f.x1(w), f.y0(h), f.y1());
    let sx = |v: f64| x0 + (plot.x.norm(v) as f32) * (x1 - x0);
    let sy = |v: f64| y0 - (plot.y.norm(v) as f32) * (y0 - y1);
    plot.lines
        .iter()
        .filter(|l| !l.pts.is_empty())
        .map(|l| {
            let mut pts = Vec::with_capacity(l.pts.len() * if l.step { 2 } else { 1 });
            for (i, (x, y)) in l.pts.iter().enumerate() {
                let py = sy(*y);
                pts.push((sx(*x), py));
                if l.step {
                    let next = l.pts.get(i + 1).map(|n| n.0).unwrap_or(x + plot.step_w);
                    pts.push((sx(next), py));
                }
            }
            Path {
                token: l.token,
                dashed: l.dashed,
                pts,
            }
        })
        .collect()
}

/// The plot's axes and its (already projected) lines.
pub fn paint_plot(plot: &Plot, paths: &[Path], w: f32, h: f32) -> Painter {
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
    p = p
        .save()
        .begin_path()
        .rect(bounds(
            point(px(x0), px(y1)),
            size(px(x1 - x0), px(y0 - y1)),
        ))
        .clip();
    for l in paths {
        p = p
            .stroke_color(paint(l.token))
            .line_width(1.5)
            .line_dash(if l.dashed { DASH } else { &[] })
            .begin_path();
        for (i, (x, y)) in l.pts.iter().enumerate() {
            p = if i == 0 {
                p.move_to(*x, *y)
            } else {
                p.line_to(*x, *y)
            };
        }
        p = p.stroke();
    }
    p.line_dash(&[]).restore()
}

/// The crosshair at canvas x `hx` with a dot on each line (its own layer over the plot, so a
/// mouse move repaints only this).
pub fn paint_crosshair(plot: &Plot, w: f32, h: f32, hx: f32) -> Painter {
    let f = Frame::standard();
    let (x0, x1, y0, y1) = (f.x0(), f.x1(w), f.y0(h), f.y1());
    if hx < x0 || hx > x1 {
        return Painter::new();
    }
    let sy = |v: f64| y0 - (plot.y.norm(v) as f32) * (y0 - y1);
    let xv = plot.x.value(((hx - x0) / (x1 - x0)) as f64);
    let mut p = Painter::new()
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
    p
}

/// What a chart says to assistive technology: per line, its range and mean over the axis.
pub fn summary(heading: &str, p: &Plot) -> String {
    let yf = p.y_fmt.f();
    let xf = p.x_fmt.f();
    let mut s = format!("{heading}, {} to {}", xf(p.x.min), xf(p.x.max));
    for l in &p.lines {
        if l.pts.is_empty() {
            continue;
        }
        let (mut lo, mut hi, mut sum) = (f64::INFINITY, f64::NEG_INFINITY, 0.0);
        for (_, y) in &l.pts {
            lo = lo.min(*y);
            hi = hi.max(*y);
            sum += y;
        }
        let mean = sum / l.pts.len() as f64;
        let name = if l.dashed && l.token == "series-b" {
            format!("{} (B)", l.label)
        } else {
            l.label.to_owned()
        };
        s += &format!(
            "; {name}: min {}, mean {}, max {}",
            yf(lo),
            yf(mean),
            yf(hi)
        );
    }
    s
}

/// The plot as a table: one row per x, one column per line (the chart's data, readable).
pub fn table(p: &Plot) -> (Vec<String>, Vec<Vec<String>>) {
    let mut head = vec!["at".to_owned()];
    head.extend(p.lines.iter().map(|l| {
        if l.dashed && l.token == "series-b" {
            format!("{} (B)", l.label)
        } else {
            l.label.to_owned()
        }
    }));
    let mut xs: Vec<f64> = p
        .lines
        .iter()
        .flat_map(|l| l.pts.iter().map(|q| q.0))
        .collect();
    xs.sort_by(f64::total_cmp);
    xs.dedup();
    let yf = p.y_fmt.f();
    let rows = xs
        .into_iter()
        .map(|x| {
            let mut r = vec![(p.x_fmt.f())(x)];
            for l in &p.lines {
                r.push(
                    l.pts
                        .iter()
                        .find(|q| q.0 == x)
                        .map(|q| yf(q.1))
                        .unwrap_or_else(|| "—".into()),
                );
            }
            r
        })
        .collect();
    (head, rows)
}

/// A chart: its title, legend and hover readout above the canvas. The plot and the crosshair
/// are two layers; the canvas carries a text summary, and "Data" shows the numbers as a
/// table (accessibility, design v4 §11).
pub fn chart(
    heading: &'static str,
    height: f32,
    data: impl Fn() -> Option<Plot> + 'static,
) -> View {
    let plot = computed(data);
    let size = signal((0.0_f32, 0.0_f32));
    let hover = signal(None::<f32>);
    let show_table = signal(false);
    // Projected once per plot and size (not per paint or mouse move).
    let paths = computed(move || {
        let (w, h) = size.get();
        plot.with(|p| match p {
            Some(p) if w > 0.0 => project(p, w, h),
            _ => Vec::new(),
        })
    });
    let readout = move || -> SharedString {
        let Some(hx) = hover.get() else {
            return "".into();
        };
        let (w, _) = size.get_untracked();
        plot.with_untracked(|p| {
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
    let described = move || -> SharedString {
        plot.with(|p| match p {
            Some(p) => summary(heading, p).into(),
            None => format!("{heading}: no data yet").into(),
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
                            .children(items.into_iter().map(|(label, tok, dashed)| {
                                view()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .child(swatch(token(tok)))
                                    .child(small(if dashed && tok == "series-b" {
                                        format!("{label} (B)")
                                    } else {
                                        label.to_owned()
                                    }))
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
                )
                .child(
                    nsh_base::button()
                        .on_press(move || show_table.set(!show_table.get_untracked()))
                        .accessibility_label(format!("{heading}: show the data as a table"))
                        .px(px(4.0))
                        .rounded(px(4.0))
                        .flex_shrink_0()
                        .hover(|st| st.bg(move || theme().secondary))
                        .focus_visible(kit::ring)
                        .child(small(
                            move || {
                                if show_table.get() { "Chart" } else { "Data" }
                            },
                        )),
                ),
        )
        .child(dynamic(move || {
            if show_table.get() {
                let t = plot.with(|p| p.as_ref().map(table));
                return untracked(|| data_table(heading, height, t).into_element());
            }
            untracked(|| {
                view()
                    .h(px(height))
                    .min_w(px(0.0))
                    .child(
                        canvas(move || {
                            let (w, h) = size.get();
                            // Repaint when the appearance changes (token colors).
                            let _ = theme();
                            paths.with(|ps| {
                                plot.with(|p| match p {
                                    Some(p) if w > 0.0 => paint_plot(p, ps, w, h),
                                    _ => Painter::new(),
                                })
                            })
                        })
                        .absolute()
                        .top(px(0.0))
                        .left(px(0.0))
                        .right(px(0.0))
                        .bottom(px(0.0))
                        .label(described)
                        .on_layout(move |frame: &Bounds<Points>| {
                            size.set((frame.size.width.0, frame.size.height.0))
                        }),
                    )
                    .child(
                        // The crosshair layer: only this repaints as the mouse moves.
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
                    )
                    .into_element()
            })
        }))
}

/// A chart's numbers as a scrollable table of the same height.
fn data_table(
    heading: &'static str,
    height: f32,
    t: Option<(Vec<String>, Vec<Vec<String>>)>,
) -> View {
    let Some((head, rows)) = t else {
        return view().h(px(height)).child(small("No data yet"));
    };
    let cell = |s: String| {
        small(s)
            .tabular_nums()
            .number_of_lines(1)
            .w(px(90.0))
            .flex_shrink_0()
    };
    view().h(px(height)).min_w(px(0.0)).child(
        scroll().flex_1().label(format!("{heading} data")).child(
            view()
                .child(
                    view()
                        .flex_row()
                        .gap_2()
                        .children(head.into_iter().map(|h| caption(h).w(px(90.0)))),
                )
                .children(
                    rows.into_iter()
                        .map(move |r| view().flex_row().gap_2().children(r.into_iter().map(cell))),
                ),
        ),
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
    crate::ui::model::fmt::compact(v)
}
