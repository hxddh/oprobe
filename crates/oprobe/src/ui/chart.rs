//! Honest charts on a canvas: real axes with units and nice ticks, a log latency axis, stepped
//! lines (never smoothed), plain polylines and dots, shaded bands (an interval, a noise floor,
//! the seconds something happened), reference rules, a hover crosshair with a readout, one
//! colour per series; and the live sparkline of a running measurement. In a terminal the same
//! drawing has labels on whole cells and no grid (a braille grid drowns the data).

use nsh_base::theme;
use nsh_core::canvas::{CanvasTextAlign, CanvasTextBaseline};
use nsh_core::element::TouchClaim;
use nsh_core::prelude::*;
use nsh_core::{TouchEvent, TouchPhase};

use crate::ui::fmt::{self, Scale};
use crate::ui::kit::{self, caption, paint, small, swatch};

/// How a series is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Each point holds until the next (time buckets).
    Step,
    /// Points joined by straight lines (a distribution, a running estimate).
    Line,
    /// Points alone (one per A/B pair, one per in-flight level).
    Dots,
}

/// One series of a plot.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub label: String,
    /// Theme token of its colour.
    pub token: &'static str,
    /// `(x, y)` sorted by x.
    pub pts: Vec<(f64, f64)>,
    pub mark: Mark,
    /// Drawn faint: context behind the main series (the attempted rate behind goodput).
    pub faint: bool,
    /// Per point: too few samples to be an estimate (drawn faint and dashed). Empty: every
    /// point holds.
    pub weak: Vec<bool>,
}

impl Line {
    pub fn step(label: impl Into<String>, token: &'static str, pts: Vec<(f64, f64)>) -> Self {
        Self {
            label: label.into(),
            token,
            pts,
            mark: Mark::Step,
            faint: false,
            weak: Vec::new(),
        }
    }
    /// The same line drawn faint.
    pub fn faint(mut self) -> Self {
        self.faint = true;
        self
    }
    /// The same line with the points where `weak` holds muted.
    pub fn weak(mut self, weak: Vec<bool>) -> Self {
        self.weak = weak;
        self
    }
    fn is_weak(&self, i: usize) -> bool {
        self.weak.get(i).copied().unwrap_or(false)
    }
    pub fn joined(label: impl Into<String>, token: &'static str, pts: Vec<(f64, f64)>) -> Self {
        Self {
            mark: Mark::Line,
            ..Self::step(label, token, pts)
        }
    }
    pub fn dots(label: impl Into<String>, token: &'static str, pts: Vec<(f64, f64)>) -> Self {
        Self {
            mark: Mark::Dots,
            ..Self::step(label, token, pts)
        }
    }
}

/// A shaded region between `lo` and `hi` along x: `(x, lo, hi)` sorted by x. A band whose
/// `lo` and `hi` are the y axis's ends shades the whole height (the seconds something
/// happened, a range of in-flight levels).
#[derive(Clone, Debug, PartialEq)]
pub struct Band {
    /// In the legend (empty: not listed).
    pub label: String,
    pub token: &'static str,
    pub pts: Vec<(f64, f64, f64)>,
    /// Each point holds until the next (time buckets), else straight edges.
    pub step: bool,
    pub alpha: f32,
}

/// A reference rule across the plot.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub token: &'static str,
    /// A vertical rule at this x, else a horizontal one at `y`.
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// Drawn at the rule's end (`p99`, `limit`).
    pub label: String,
    pub dashed: bool,
}

impl Rule {
    pub fn at_x(x: f64, token: &'static str, label: impl Into<String>) -> Self {
        Self {
            token,
            x: Some(x),
            y: None,
            label: label.into(),
            dashed: true,
        }
    }
    pub fn at_y(y: f64, token: &'static str, label: impl Into<String>) -> Self {
        Self {
            token,
            x: None,
            y: Some(y),
            label: label.into(),
            dashed: false,
        }
    }
}

/// How the values of an axis read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Ms,
    Rate,
    /// Seconds from the window start.
    Clock,
    /// MiB per second.
    MibRate,
    /// A share, 0–1, as a percentage.
    Share,
    /// A signed relative change (`+12.0 %`).
    Change,
    /// A whole count (in-flight requests, a pair number).
    Count,
    /// A small share on a log axis (a tail plot): `100 %`, `1 %`, `0.01 %`, `3.47 %`.
    Tail,
}

/// A share as a percentage with its significant digits and no trailing zeros: `0.0001` →
/// `0.01 %`, `0.0347` → `3.47 %`, `1` → `100 %`.
pub fn tail_share(v: f64) -> String {
    let p = v * 100.0;
    if !p.is_finite() || p <= 0.0 {
        return "0 %".into();
    }
    let s = fmt::sig3(p);
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    };
    format!("{s} %")
}

impl Unit {
    pub fn f(self) -> fn(f64) -> String {
        match self {
            Unit::Ms => fmt::dur_ms,
            Unit::Rate => |v| format!("{}/s", fmt::compact(v)),
            Unit::Clock => fmt::clock_s,
            Unit::MibRate => |v| format!("{} MiB/s", fmt::sig3(v)),
            Unit::Share => |v| {
                if v == 0.0 {
                    "0 %".into()
                } else {
                    fmt::ratio(v)
                }
            },
            Unit::Change => fmt::delta,
            Unit::Tail => tail_share,
            Unit::Count => |v| {
                let v = v.round();
                if v.abs() < 1000.0 {
                    format!("{v:.0}")
                } else {
                    fmt::compact(v)
                }
            },
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
    pub x_fmt: Unit,
    pub y_fmt: Unit,
    pub lines: Vec<Line>,
    pub bands: Vec<Band>,
    pub rules: Vec<Rule>,
    /// What the readout adds over `[from, to)` of x (`n=12, too few for p99`).
    pub notes: Vec<(f64, f64, String)>,
}

impl Plot {
    /// A time series: x in seconds over `[0, window]`, one-second steps.
    pub fn over_time(window_s: f64, y: Scale, y_fmt: Unit, lines: Vec<Line>) -> Self {
        Self {
            x: Scale {
                min: 0.0,
                max: window_s.max(1.0),
                log: false,
            },
            y,
            step_w: 1.0,
            x_fmt: Unit::Clock,
            y_fmt,
            lines,
            bands: Vec::new(),
            rules: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// The readout's note at `x`, if any.
    pub fn note_at(&self, x: f64) -> Option<&str> {
        self.notes
            .iter()
            .find(|(a, b, _)| *a <= x && x < *b)
            .map(|n| n.2.as_str())
    }

    /// What a screen reader says of the chart: `Goodput over 8 s, 1.0k/s–1.1k/s` (a time
    /// series), else the heading and the range of x.
    pub fn summary(&self, heading: &str) -> String {
        let ys: Vec<f64> = self
            .lines
            .iter()
            .find(|l| !l.faint && !l.pts.is_empty())
            .map(|l| l.pts.iter().map(|p| p.1).collect())
            .unwrap_or_default();
        let (lo, hi) = ys
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| {
                (a.min(*v), b.max(*v))
            });
        if self.x_fmt == Unit::Clock {
            if !lo.is_finite() {
                return crate::ui::i18n::trf(
                    "chart.a11y_empty",
                    &[&heading, &self.x_fmt.f()(self.x.max)],
                );
            }
            let f = self.y_fmt.f();
            crate::ui::i18n::trf(
                "chart.a11y_time",
                &[&heading, &self.x_fmt.f()(self.x.max), &f(lo), &f(hi)],
            )
        } else {
            let f = self.x_fmt.f();
            crate::ui::i18n::trf(
                "chart.a11y_range",
                &[&heading, &f(self.x.min), &f(self.x.max)],
            )
        }
    }

    /// Each line's value at `x`: the step holding it, or the nearest point of a line or dots
    /// (within a twentieth of the axis).
    pub fn values_at(&self, x: f64) -> Vec<(&str, Option<f64>)> {
        let tx = self.x.norm(x);
        self.lines
            .iter()
            .map(|l| {
                let v = match l.mark {
                    Mark::Step => l
                        .pts
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(i, (px, _))| {
                            let end = l
                                .pts
                                .get(i + 1)
                                .map(|n| n.0)
                                .unwrap_or(*px + self.step_w.max(1e-12));
                            *px <= x && x < end
                        })
                        .map(|(_, p)| p.1),
                    Mark::Line | Mark::Dots => l
                        .pts
                        .iter()
                        .map(|p| ((self.x.norm(p.0) - tx).abs(), p.1))
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                        .filter(|(d, _)| *d <= 0.05)
                        .map(|(_, v)| v),
                };
                (l.label.as_str(), v)
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

/// Axes, grid and tick labels over a `w`×`h` canvas.
#[allow(clippy::too_many_arguments)]
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
    let target = ((x1 - x0) / (10.0_f32 * cw).max(56.0)).clamp(2.0, 8.0) as usize;
    p = p
        .text_align(CanvasTextAlign::Center)
        .text_baseline(CanvasTextBaseline::Top);
    // A tick whose label would read as another value (a count's 1.5 reads "2") is left out.
    let honest = |t: &f64| x_fmt(*t) != x_fmt(t.round()) || (t - t.round()).abs() < 1e-9;
    for t in x.ticks(target).into_iter().filter(honest) {
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
    p.stroke_color(axis)
        .begin_path()
        .move_to(x0 + 0.5, y1)
        .line_to(x0 + 0.5, y0 + 0.5)
        .line_to(x1, y0 + 0.5)
        .stroke()
}

/// The plot: bands under everything, then the axes, the series and the rules.
pub fn paint_plot(plot: &Plot, w: f32, h: f32) -> Painter {
    let f = frame_for(&plot.y, plot.y_fmt.f(), h);
    let (x0, x1, y0, y1) = (f.left, f.x1(w), f.y0(h), f.top);
    let sx = |v: f64| x0 + (plot.x.norm(v) as f32) * (x1 - x0);
    let sy = |v: f64| y0 - (plot.y.norm(v) as f32) * (y0 - y1);
    let clip = |p: Painter| {
        p.save()
            .begin_path()
            .rect(bounds(
                point(px(x0), px(y1)),
                size(px(x1 - x0), px(y0 - y1)),
            ))
            .clip()
    };
    let mut p = clip(Painter::new());
    for b in plot.bands.iter().filter(|b| !b.pts.is_empty()) {
        let next = |i: usize| {
            b.pts
                .get(i + 1)
                .map(|n| n.0)
                .unwrap_or(b.pts[i].0 + plot.step_w)
        };
        p = p.global_alpha(b.alpha).fill_color(paint(b.token));
        if b.step {
            // One rectangle per step (a step whose lo meets hi draws nothing).
            for (i, (x, lo, hi)) in b.pts.iter().enumerate() {
                let (top, bottom) = (sy(*hi), sy(*lo));
                if bottom - top > 0.0 {
                    let (l, r) = (sx(*x), sx(next(i)));
                    p = p.fill_rect(l, top, (r - l).max(1.0), bottom - top);
                }
            }
        } else {
            p = p.begin_path();
            for (i, (x, _, hi)) in b.pts.iter().enumerate() {
                p = if i == 0 {
                    p.move_to(sx(*x), sy(*hi))
                } else {
                    p.line_to(sx(*x), sy(*hi))
                };
            }
            for (x, lo, _) in b.pts.iter().rev() {
                p = p.line_to(sx(*x), sy(*lo));
            }
            p = p.close_path().fill();
        }
    }
    p = p.global_alpha(1.0).restore();
    p = axes(p, f, w, h, &plot.x, &plot.y, plot.x_fmt.f(), plot.y_fmt.f());
    p = clip(p);
    for l in plot.lines.iter().filter(|l| !l.pts.is_empty()) {
        p = p.stroke_color(paint(l.token)).fill_color(paint(l.token));
        match l.mark {
            Mark::Dots => {
                for (x, y) in &l.pts {
                    p = p
                        .begin_path()
                        .arc(sx(*x), sy(*y), 2.5, 0.0, std::f32::consts::TAU)
                        .fill();
                }
            }
            Mark::Step | Mark::Line if l.weak.iter().any(|w| *w) => {
                // Segment by segment: a weak point's step (or the segment into it) is faint
                // and dashed, so a quantile from too few operations does not read as one.
                let dash = [px(3.0), px(3.0)];
                for (i, (x, y)) in l.pts.iter().enumerate() {
                    let weak = l.is_weak(i);
                    p = p
                        .global_alpha(if weak { 0.35 } else { 1.0 })
                        .line_dash(if weak && !kit::in_terminal() {
                            &dash[..]
                        } else {
                            &[]
                        })
                        .line_width(if weak { 1.0 } else { 1.5 })
                        .begin_path();
                    if l.mark == Mark::Step {
                        let next = l.pts.get(i + 1).map(|n| n.0).unwrap_or(x + plot.step_w);
                        p = p.move_to(sx(*x), sy(*y)).line_to(sx(next), sy(*y));
                        if let Some(n) = l.pts.get(i + 1) {
                            p = p.line_to(sx(n.0), sy(n.1));
                        }
                    } else if let Some(n) = l.pts.get(i + 1) {
                        p = p.move_to(sx(*x), sy(*y)).line_to(sx(n.0), sy(n.1));
                    }
                    p = p.stroke();
                }
                p = p.global_alpha(1.0).line_dash(&[]);
            }
            Mark::Step | Mark::Line => {
                p = p
                    .global_alpha(if l.faint { 0.4 } else { 1.0 })
                    .line_width(if l.faint { 1.0 } else { 1.5 })
                    .begin_path();
                for (i, (x, y)) in l.pts.iter().enumerate() {
                    p = if i == 0 {
                        p.move_to(sx(*x), sy(*y))
                    } else {
                        p.line_to(sx(*x), sy(*y))
                    };
                    if l.mark == Mark::Step {
                        let next = l.pts.get(i + 1).map(|n| n.0).unwrap_or(x + plot.step_w);
                        p = p.line_to(sx(next), sy(*y));
                    }
                }
                p = p.stroke().global_alpha(1.0);
            }
        }
    }
    p = p.restore().font(FONT).line_width(1.0);
    let dash = [px(4.0), px(3.0)];
    for r in &plot.rules {
        let c = paint(r.token);
        p = p
            .stroke_color(c)
            .fill_color(c)
            .line_dash(if r.dashed && !kit::in_terminal() {
                &dash[..]
            } else {
                &[]
            });
        if let Some(x) = r.x {
            let px_ = sx(x).round() + 0.5;
            p = p
                .begin_path()
                .move_to(px_, y1)
                .line_to(px_, y0)
                .stroke()
                .line_dash(&[]);
            if !r.label.is_empty() {
                let right = px_ + 4.0 + r.label.chars().count() as f32 * 6.6 < x1;
                p = p
                    .text_align(if right {
                        CanvasTextAlign::Left
                    } else {
                        CanvasTextAlign::Right
                    })
                    .text_baseline(CanvasTextBaseline::Top)
                    .fill_text(
                        &r.label,
                        if right { px_ + 4.0 } else { px_ - 4.0 },
                        y1 + 1.0,
                    );
            }
        } else if let Some(y) = r.y {
            let py = sy(y).round() + 0.5;
            p = p
                .begin_path()
                .move_to(x0, py)
                .line_to(x1, py)
                .stroke()
                .line_dash(&[]);
            if !r.label.is_empty() {
                p = p
                    .text_align(CanvasTextAlign::Right)
                    .text_baseline(CanvasTextBaseline::Bottom)
                    .fill_text(&r.label, x1 - 2.0, py - 1.0);
            }
        }
    }
    p.line_dash(&[])
}

/// The time the charts of one view share (design v6.3 B5): the crosshair, the range selected by
/// a drag, and the drag in progress. Every time chart and the heatmap given the same link show
/// the same time cursor and shade the same range.
#[derive(Clone, Copy)]
pub struct TimeLink {
    /// The time under the pointer, seconds into the window.
    pub cursor: Signal<Option<f64>>,
    /// The selected range `[from, to)`.
    pub range: Signal<Option<(f64, f64)>>,
    /// The range a drag is selecting (from where it began to where it is).
    pub drag: Signal<Option<(f64, f64)>>,
}

impl TimeLink {
    pub fn new() -> Self {
        Self {
            cursor: signal(None),
            range: signal(None),
            drag: signal(None),
        }
    }

    /// The range shaded now: the drag in progress, else the selection.
    pub fn shaded(&self) -> Option<(f64, f64)> {
        self.drag
            .get()
            .map(|(a, b)| (a.min(b), a.max(b)))
            .or_else(|| self.range.get())
    }

    pub fn clear(&self) {
        self.range.set(None);
        self.drag.set(None);
    }
}

impl Default for TimeLink {
    fn default() -> Self {
        Self::new()
    }
}

/// A pointer's gesture over a linked canvas: where it went down (canvas points) and when the
/// last tap ended (for a double tap).
#[derive(Clone, Copy)]
pub struct Gesture {
    start: Signal<Option<(f32, f32)>>,
    last_tap: Signal<f64>,
}

impl Gesture {
    pub fn new() -> Self {
        Self {
            start: signal(None),
            last_tap: signal(f64::NEG_INFINITY),
        }
    }
}

impl Default for Gesture {
    fn default() -> Self {
        Self::new()
    }
}

/// The pointer moves this far (points) before a press becomes a drag.
const DRAG_SLOP: f32 = 3.0;
/// Two taps this close (ms) are a double tap.
const DOUBLE_TAP_MS: f64 = 400.0;

/// Brushing on a linked canvas: a drag selects a time range (`to_t` maps a canvas x to a time,
/// `None` off the plot), a double tap clears it, a tap calls `tap` with where it landed.
pub fn on_brush(
    e: &TouchEvent,
    link: TimeLink,
    g: Gesture,
    to_t: &dyn Fn(f32) -> Option<f64>,
    tap: &dyn Fn(f32, f32),
) {
    let Some(p) = e.points.iter().find(|p| p.changed) else {
        return;
    };
    let (x, y) = (p.location.x.0, p.location.y.0);
    match e.phase {
        TouchPhase::Start => {
            g.start.set(Some((x, y)));
            link.drag.set(None);
        }
        TouchPhase::Move => {
            let Some((x0, _)) = g.start.get_untracked() else {
                return;
            };
            if (x - x0).abs() >= DRAG_SLOP
                && let (Some(a), Some(b)) = (to_t(x0), to_t(x))
            {
                link.drag.set(Some((a, b)));
                link.cursor.set(Some(b));
            }
        }
        TouchPhase::End => {
            let start = g.start.get_untracked();
            g.start.set(None);
            let drag = link.drag.get_untracked();
            link.drag.set(None);
            match (start, drag) {
                (Some((x0, _)), Some((a, b))) if (x - x0).abs() >= DRAG_SLOP => {
                    let b = to_t(x).unwrap_or(b);
                    let (lo, hi) = (a.min(b), a.max(b));
                    if hi > lo {
                        link.range.set(Some((lo, hi)));
                    }
                }
                _ => {
                    if e.timestamp - g.last_tap.get_untracked() < DOUBLE_TAP_MS {
                        link.range.set(None);
                        g.last_tap.set(f64::NEG_INFINITY);
                    } else {
                        g.last_tap.set(e.timestamp);
                        tap(x, y);
                    }
                }
            }
        }
        TouchPhase::Cancel => {
            g.start.set(None);
            link.drag.set(None);
        }
    }
}

/// The time cursor and the shaded range over a plot area `[x0, x1] × [y1, y0]` of x scale
/// `x` (its own layer: a pointer move repaints only this).
pub fn paint_time_overlay(
    mut p: Painter,
    x: &Scale,
    area: (f32, f32, f32, f32),
    cursor: Option<f64>,
    band: Option<(f64, f64)>,
) -> Painter {
    let (x0, x1, y0, y1) = area;
    let sx = |v: f64| x0 + (x.norm(v).clamp(0.0, 1.0) as f32) * (x1 - x0);
    if let Some((a, b)) = band {
        let (l, r) = (sx(a), sx(b));
        p = p
            .global_alpha(0.16)
            .fill_color(paint("series-a"))
            .fill_rect(l, y1, (r - l).max(1.0), y0 - y1)
            .global_alpha(0.7)
            .stroke_color(paint("series-a"))
            .line_width(1.0)
            .begin_path()
            .move_to(l.round() + 0.5, y1)
            .line_to(l.round() + 0.5, y0)
            .move_to(r.round() - 0.5, y1)
            .line_to(r.round() - 0.5, y0)
            .stroke()
            .global_alpha(1.0);
    }
    if let Some(t) = cursor.filter(|t| *t >= x.min && *t <= x.max) {
        let px_ = sx(t).round() + 0.5;
        p = p
            .stroke_color(paint("axis"))
            .line_width(1.0)
            .begin_path()
            .move_to(px_, y1)
            .line_to(px_, y0)
            .stroke();
    }
    p
}

/// The value of each line at `xv` as dots on the cursor.
fn paint_dots(mut p: Painter, plot: &Plot, w: f32, h: f32, xv: f64) -> Painter {
    let f = frame_for(&plot.y, plot.y_fmt.f(), h);
    let (x0, x1, y0, y1) = (f.left, f.x1(w), f.y0(h), f.top);
    if xv < plot.x.min || xv > plot.x.max {
        return p;
    }
    let hx = x0 + (plot.x.norm(xv) as f32) * (x1 - x0);
    for ((_, v), l) in plot.values_at(xv).into_iter().zip(&plot.lines) {
        if let Some(v) = v {
            let py = y0 - (plot.y.norm(v) as f32) * (y0 - y1);
            p = p
                .global_alpha(if l.faint { 0.5 } else { 1.0 })
                .fill_color(paint(l.token))
                .begin_path()
                .arc(hx, py, 3.0, 0.0, std::f32::consts::TAU)
                .fill();
        }
    }
    p.global_alpha(1.0)
}

/// The x value under canvas x `hx` (`None` off the plot area).
fn x_at(plot: &Plot, w: f32, h: f32, hx: f32) -> Option<f64> {
    let f = frame_for(&plot.y, plot.y_fmt.f(), h);
    let (x0, x1) = (f.left, f.x1(w));
    (hx >= x0 - 1.0 && hx <= x1 + 1.0)
        .then(|| plot.x.value(((hx - x0) / (x1 - x0)).clamp(0.0, 1.0) as f64))
}

/// The readout at x value `xv`: x, each line's value and the note there.
pub fn readout_at(p: &Plot, xv: f64) -> String {
    let mut s = p.x_fmt.f()(xv);
    for (label, v) in p.values_at(xv) {
        let v = v.map(p.y_fmt.f()).unwrap_or_else(|| "—".into());
        s += &format!(" · {label} {v}");
    }
    if let Some(n) = p.note_at(xv) {
        s += &format!(" · {n}");
    }
    s
}

/// A chart: its heading, legend and hover readout above the canvas.
pub fn chart(
    heading: impl IntoReactive<SharedString>,
    height: f32,
    data: impl Fn() -> Option<Plot> + 'static,
) -> View {
    chart_with(heading, height, data, None, "chart")
}

/// A chart on a time axis that shares its crosshair and range with the others of `link`; a
/// drag on it selects a range (`id`: its canvas's test id).
pub fn linked_chart(
    heading: impl IntoReactive<SharedString>,
    height: f32,
    data: impl Fn() -> Option<Plot> + 'static,
    link: TimeLink,
    id: &'static str,
) -> View {
    chart_with(heading, height, data, Some(link), id)
}

fn chart_with(
    heading: impl IntoReactive<SharedString>,
    height: f32,
    data: impl Fn() -> Option<Plot> + 'static,
    link: Option<TimeLink>,
    id: &'static str,
) -> View {
    let plot = computed(data);
    let size = signal((0.0_f32, 0.0_f32));
    // The x value under the pointer: the link's time, or this chart's own.
    let cursor = link
        .map(|l| l.cursor)
        .unwrap_or_else(|| signal(None::<f64>));
    let heading = heading.into_reactive();
    let a11y_heading = heading.clone();
    let readout = move || -> SharedString {
        let Some(xv) = cursor.get() else {
            return "".into();
        };
        plot.with_untracked(|p| {
            p.as_ref()
                .map(|p| SharedString::from(readout_at(p, xv)))
                .unwrap_or_default()
        })
    };
    let legend = move || {
        plot.with(|p| {
            p.as_ref()
                .map(|p| {
                    // A series that is zero throughout draws on the axis: no legend entry.
                    p.lines
                        .iter()
                        .filter(|l| l.pts.iter().any(|p| p.1 != 0.0))
                        .map(|l| (l.label.clone(), l.token))
                        .chain(
                            p.bands
                                .iter()
                                .filter(|b| !b.label.is_empty())
                                .map(|b| (b.label.clone(), b.token)),
                        )
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    };
    let a11y = move || -> SharedString {
        let h = a11y_heading.get();
        plot.with(|p| {
            p.as_ref()
                .map(|p| SharedString::from(p.summary(&h)))
                .unwrap_or_else(|| h.clone())
        })
    };
    let gesture = Gesture::new();
    let overlay = canvas(move || {
        let (w, h) = size.get();
        let c = cursor.get();
        let band = link.and_then(|l| l.shaded());
        plot.with(|p| match p {
            Some(p) if w > 0.0 => {
                let f = frame_for(&p.y, p.y_fmt.f(), h);
                let area = (f.left, f.x1(w), f.y0(h), f.top);
                let layer = paint_time_overlay(Painter::new(), &p.x, area, c, band);
                match c {
                    Some(xv) => paint_dots(layer, p, w, h, xv),
                    None => layer,
                }
            }
            _ => Painter::new(),
        })
    })
    .absolute()
    .top(px(0.0))
    .left(px(0.0))
    .right(px(0.0))
    .bottom(px(0.0))
    .test_id(id)
    .role(AccessibilityRole::Image)
    .label(a11y)
    .on_mouse_move(move |e: &MouseEvent| {
        let (w, h) = size.get_untracked();
        let xv = plot.with_untracked(|p| p.as_ref().and_then(|p| x_at(p, w, h, e.position.x.0)));
        cursor.set(xv);
    })
    .on_mouse_leave(move |_: &MouseEvent| cursor.set(None));
    let focused = signal(false);
    if let Some(link) = link {
        let plot_k = plot;
        effect(move || {
            if !focused.get() {
                return;
            }
            let opts = KeyOptions {
                when: nsh_core::keymap::When::NotEditing,
                repeat: true,
                for_focus: true,
                ..Default::default()
            };
            let step_by = move |dir: f64, extend: bool| {
                plot_k.with_untracked(|p| {
                    let Some(p) = p else { return };
                    let step = p.step_w.max(0.1);
                    let cur = link.cursor.get_untracked().unwrap_or(p.x.min);
                    let next = (cur + dir * step).clamp(p.x.min, (p.x.max - step).max(p.x.min));
                    link.cursor.set(Some(next));
                    if extend {
                        let (a, b) = link.range.get_untracked().unwrap_or((cur, cur + step));
                        let lo = a.min(next).max(p.x.min);
                        let hi = b.max(next + step).min(p.x.max);
                        if hi > lo {
                            link.range.set(Some((lo, hi)));
                        }
                    }
                });
            };
            on_key_with("left", opts, move |_| step_by(-1.0, false));
            on_key_with("right", opts, move |_| step_by(1.0, false));
            on_key_with("shift+left", opts, move |_| step_by(-1.0, true));
            on_key_with("shift+right", opts, move |_| step_by(1.0, true));
            on_key_with("enter", opts, move |_| {
                plot_k.with_untracked(|p| {
                    let Some(p) = p else { return };
                    let step = p.step_w.max(0.1);
                    let cur = link.cursor.get_untracked().unwrap_or(p.x.min);
                    let hi = (cur + step).min(p.x.max);
                    if hi > cur {
                        link.range.set(Some((cur, hi)));
                    }
                });
            });
        });
    }
    let overlay = match link {
        Some(link) => overlay
            .focusable(true)
            .focus_visible(kit::ring)
            .on_focus(move || focused.set(true))
            .on_blur(move || focused.set(false))
            .claim_touches(TouchClaim::on_start())
            .on_touch(move |e: &TouchEvent| {
                let (w, h) = size.get_untracked();
                let to_t =
                    |hx: f32| plot.with_untracked(|p| p.as_ref().and_then(|p| x_at(p, w, h, hx)));
                on_brush(e, link, gesture, &to_t, &|_, _| {});
            }),
        None => overlay,
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
                .child(overlay),
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
