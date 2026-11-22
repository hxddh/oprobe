//! Axis scales and "nice" ticks for the honest charts: linear axes start at zero for
//! magnitudes, ticks fall on 1/2/5 × 10ⁿ, log axes tick on decades (and 2/5 between when few).

/// A mapping from data values to 0–1 along an axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    pub min: f64,
    pub max: f64,
    pub log: bool,
}

impl Scale {
    /// A linear scale over `[min, max]`, widened to nice tick bounds.
    pub fn linear(min: f64, max: f64) -> Self {
        let (lo, hi, _) = nice_range(min, max, 5);
        Self {
            min: lo,
            max: hi,
            log: false,
        }
    }

    /// A linear scale from zero to the data's maximum (magnitudes: rates, counts).
    pub fn zero_based(max: f64) -> Self {
        Self::linear(0.0, if max > 0.0 { max } else { 1.0 })
    }

    /// A log scale covering `[min, max]` (positive), widened to whole decades.
    pub fn log(min: f64, max: f64) -> Self {
        let min = if min > 0.0 { min } else { max.max(1e-3) / 1e3 };
        let max = max.max(min * 10.0);
        Self {
            min: 10f64.powf(min.log10().floor()),
            max: 10f64.powf(max.log10().ceil()),
            log: true,
        }
    }

    /// Where `v` falls, 0 at `min` and 1 at `max` (clamped).
    pub fn norm(&self, v: f64) -> f64 {
        let t = if self.log {
            let v = v.max(self.min);
            (v.log10() - self.min.log10()) / (self.max.log10() - self.min.log10())
        } else {
            (v - self.min) / (self.max - self.min)
        };
        if t.is_finite() {
            t.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// The value at `t` (0–1) along the axis.
    pub fn value(&self, t: f64) -> f64 {
        if self.log {
            10f64.powf(self.min.log10() + t * (self.max.log10() - self.min.log10()))
        } else {
            self.min + t * (self.max - self.min)
        }
    }

    /// Tick values (about `target` of them on a linear axis).
    pub fn ticks(&self, target: usize) -> Vec<f64> {
        if self.log {
            log_ticks(self.min, self.max)
        } else {
            // Ticks inside the axis only: an axis that ends at 8 s has no "10 s" label.
            let step = nice_step((self.max - self.min) / target.max(1) as f64);
            let first = (self.min / step - 1e-9).ceil();
            let last = (self.max / step + 1e-9).floor();
            (first as i64..=last as i64)
                .map(|i| i as f64 * step)
                .collect()
        }
    }
}

/// 1, 2 or 5 × 10ⁿ at or above `raw`.
pub fn nice_step(raw: f64) -> f64 {
    if !(raw.is_finite() && raw > 0.0) {
        return 1.0;
    }
    let mag = 10f64.powf(raw.log10().floor());
    let f = raw / mag;
    let nice = if f <= 1.0 {
        1.0
    } else if f <= 2.0 {
        2.0
    } else if f <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * mag
}

/// `[lo, hi]` widened to multiples of a nice step giving about `target` intervals.
pub fn nice_range(min: f64, max: f64, target: usize) -> (f64, f64, f64) {
    let (min, max) = if max > min {
        (min, max)
    } else {
        (min, min + 1.0)
    };
    let step = nice_step((max - min) / target.max(1) as f64);
    let lo = (min / step).floor() * step;
    let hi = (max / step).ceil() * step;
    (lo, hi, step)
}

/// Decades between `min` and `max`; with three decades or fewer, 2 and 5 between them too.
pub fn log_ticks(min: f64, max: f64) -> Vec<f64> {
    let (a, b) = (min.log10().floor() as i32, max.log10().ceil() as i32);
    let few = b - a <= 3;
    let mut out = Vec::new();
    for e in a..=b {
        let d = 10f64.powi(e);
        for m in [1.0, 2.0, 5.0] {
            if m != 1.0 && !few {
                continue;
            }
            let v = d * m;
            if v >= min * 0.999 && v <= max * 1.001 {
                out.push(v);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_linear_ticks() {
        let s = Scale::zero_based(87.0);
        assert_eq!(s.min, 0.0);
        assert_eq!(s.max, 100.0);
        assert_eq!(s.ticks(5), vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
        assert_eq!(nice_step(0.3), 0.5);
        let t = Scale {
            min: 0.0,
            max: 8.0,
            log: false,
        }
        .ticks(2);
        assert_eq!(t, vec![0.0, 5.0]);
        assert_eq!(nice_step(7.0), 10.0);
    }

    #[test]
    fn log_axis_covers_decades() {
        let s = Scale::log(0.7, 340.0);
        assert_eq!((s.min, s.max), (0.1, 1000.0));
        assert!((s.norm(1.0) - 1.0 / 4.0).abs() < 1e-9);
        assert_eq!(s.ticks(5), vec![0.1, 1.0, 10.0, 100.0, 1000.0]);
        assert_eq!(
            Scale::log(0.7, 34.0).ticks(5),
            vec![0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0]
        );
        let wide = Scale::log(0.01, 1e5);
        assert_eq!(wide.ticks(5).len(), 8);
    }

    #[test]
    fn norm_and_value_round_trip() {
        let s = Scale::linear(0.0, 50.0);
        assert!((s.value(s.norm(25.0)) - 25.0).abs() < 1e-9);
        assert_eq!(s.norm(-5.0), 0.0);
    }
}
