//! Numbers for the app: compact, unit-carrying, fixed width where it matters (tiles never
//! wrap), and the axis scales of the charts (nice 1/2/5 ticks, log latency axes). Numbers,
//! units and codes are never translated.

/// `812`, `1.23k`, `12.3k`, `123k`, `1.23M`, … (3 significant digits).
pub fn compact(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    let a = v.abs();
    let (x, suffix) = if a >= 1e9 {
        (v / 1e9, "G")
    } else if a >= 1e6 {
        (v / 1e6, "M")
    } else if a >= 1e3 {
        (v / 1e3, "k")
    } else {
        (v, "")
    };
    format!("{}{suffix}", sig3(x))
}

/// Three significant digits.
pub fn sig3(x: f64) -> String {
    let a = x.abs();
    if a == 0.0 {
        "0".into()
    } else if a >= 100.0 {
        format!("{x:.0}")
    } else if a >= 10.0 {
        format!("{x:.1}")
    } else if a >= 1.0 {
        format!("{x:.2}")
    } else {
        let decimals = (2 - a.log10().floor() as i32).clamp(3, 9) as usize;
        format!("{x:.decimals$}")
    }
}

/// A duration given in µs: `812 µs`, `1.23 ms`, `123 ms`, `1.23 s`, `2.0 min`.
pub fn dur_us(us: f64) -> String {
    if !us.is_finite() {
        return "—".into();
    }
    let a = us.abs();
    if a < 1.0 {
        "0 µs".into()
    } else if a < 1_000.0 {
        format!("{us:.0} µs")
    } else if a < 1_000_000.0 {
        format!("{} ms", sig3(us / 1e3))
    } else if a < 120_000_000.0 {
        format!("{} s", sig3(us / 1e6))
    } else {
        format!("{:.1} min", us / 6e7)
    }
}

/// A duration given in ms.
pub fn dur_ms(ms: f64) -> String {
    dur_us(ms * 1000.0)
}

/// A ratio given as 0–1, as a percentage: `12.3 %`, `<0.01 %`.
pub fn ratio(r: f64) -> String {
    let p = r * 100.0;
    if !p.is_finite() {
        "—".into()
    } else if p == 0.0 {
        "0 %".into()
    } else if p.abs() < 0.01 {
        "<0.01 %".into()
    } else {
        format!("{} %", sig3(p))
    }
}

/// A signed relative change given as a ratio (0.31 → `+31.0 %`, −0.042 → `−4.20 %`).
pub fn delta(r: f64) -> String {
    let p = r * 100.0;
    if !p.is_finite() {
        return "—".into();
    }
    if p.abs() < 0.01 {
        return "±0 %".into();
    }
    let s = sig3(p.abs());
    if p > 0.0 {
        format!("+{s} %")
    } else {
        format!("−{s} %")
    }
}

/// Seconds as a timeline position: `0.50 s`, `12.3 s`, `2:05`.
pub fn clock_s(s: f64) -> String {
    if s.abs() < 100.0 {
        format!("{} s", sig3(s))
    } else {
        let t = s.round() as i64;
        format!("{}:{:02}", t / 60, t.rem_euclid(60))
    }
}

/// The month, day and minute of an RFC 3339 time (`2026-11-20T14:30:05Z` → `11-20 14:30`).
pub fn short_time(rfc3339: &str) -> String {
    let (date, time) = rfc3339.split_once('T').unwrap_or((rfc3339, ""));
    format!(
        "{} {}",
        date.get(5..10).unwrap_or(date),
        time.get(..5).unwrap_or(time)
    )
}

/// A mapping from data values to 0–1 along an axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    pub min: f64,
    pub max: f64,
    pub log: bool,
}

impl Scale {
    /// A linear scale from zero to the data's maximum, widened to a nice bound.
    pub fn zero_based(max: f64) -> Self {
        let max = if max > 0.0 && max.is_finite() {
            max
        } else {
            1.0
        };
        let step = nice_step(max / 5.0);
        Self {
            min: 0.0,
            max: (max / step).ceil() * step,
            log: false,
        }
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

    /// Tick values (about `target` of them on a linear axis; decades on a log axis).
    pub fn ticks(&self, target: usize) -> Vec<f64> {
        if self.log {
            let (a, b) = (
                self.min.log10().floor() as i32,
                self.max.log10().ceil() as i32,
            );
            let few = b - a <= 3;
            let mut out = Vec::new();
            for e in a..=b {
                for m in [1.0, 2.0, 5.0] {
                    let v = 10f64.powi(e) * m;
                    if (m == 1.0 || few) && v >= self.min * 0.999 && v <= self.max * 1.001 {
                        out.push(v);
                    }
                }
            }
            out
        } else {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_keep_their_units() {
        assert_eq!(compact(1234.0), "1.23k");
        assert_eq!(dur_ms(0.5), "500 µs");
        assert_eq!(dur_ms(12.34), "12.3 ms");
        assert_eq!(delta(0.31), "+31.0 %");
        assert_eq!(delta(-0.042), "−4.20 %");
        assert_eq!(ratio(0.0), "0 %");
        assert_eq!(short_time("2026-11-20T14:30:05Z"), "11-20 14:30");
    }

    #[test]
    fn scales_tick_nicely() {
        let s = Scale::zero_based(87.0);
        assert_eq!((s.min, s.max), (0.0, 100.0));
        assert_eq!(s.ticks(5), vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
        let l = Scale::log(0.7, 340.0);
        assert_eq!((l.min, l.max), (0.1, 1000.0));
        assert_eq!(l.ticks(5), vec![0.1, 1.0, 10.0, 100.0, 1000.0]);
        assert!((s.value(s.norm(25.0)) - 25.0).abs() < 1e-9);
    }
}
