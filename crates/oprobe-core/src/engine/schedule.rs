//! Open-loop arrival schedules (design v5 §3, fixes M1): when operations are *meant* to start.
//!
//! An open-loop run issues operations at intended times that do not depend on how fast earlier
//! operations finished, so a slow target shows up as latency (measured from the intended start)
//! and as arrivals that never started (censored latencies), never as a quietly lower rate
//! (coordinated omission).
//!
//! - [`Arrivals`]: evenly spaced (`constant`) or exponential gaps (`poisson`).
//! - [`RateShape`]: a rate that changes over the window: `ramp:FROM..TO/D`,
//!   `step:R1,R2,…/D` (each rate held for D), `diurnal[:MIN..MAX/PERIOD]`.
//! - [`ArrivalClock`]: the generator the runner pulls intended start offsets from.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use rand::Rng;
use rand::rngs::SmallRng;
use serde::{Deserialize, Serialize};

use crate::errors::OprobeError;

/// How open-loop arrivals are spaced.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Arrivals {
    /// Evenly spaced: one arrival every `1 / rate` seconds.
    #[default]
    Constant,
    /// A Poisson process: exponentially distributed gaps with mean `1 / rate` (bursty, like
    /// independent clients).
    Poisson,
}

impl Arrivals {
    pub fn is_constant(&self) -> bool {
        *self == Arrivals::Constant
    }

    pub fn label(self) -> &'static str {
        match self {
            Arrivals::Constant => "constant",
            Arrivals::Poisson => "poisson",
        }
    }
}

impl FromStr for Arrivals {
    type Err = OprobeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "constant" | "uniform" | "fixed" => Ok(Arrivals::Constant),
            "poisson" | "exponential" => Ok(Arrivals::Poisson),
            other => Err(
                crate::input::invalid(format!("unknown arrival process `{other}`"))
                    .with_hint("use constant or poisson"),
            ),
        }
    }
}

impl fmt::Display for Arrivals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A rate that changes during the window (ops/s as a function of the time since the start).
#[derive(Clone, Debug, PartialEq)]
pub enum RateShape {
    /// Linear from `from` to `to` over `over`, then held at `to`.
    Ramp { from: f64, to: f64, over: Duration },
    /// Each rate held for `every`, the last one until the end.
    Step { rates: Vec<f64>, every: Duration },
    /// A sine between `min` and `max` (starting at `min`); one period per `period`, or per
    /// measurement window when unset.
    Diurnal {
        min: Option<f64>,
        max: Option<f64>,
        period: Option<Duration>,
    },
}

fn bad_shape(s: &str) -> OprobeError {
    crate::input::invalid(format!("invalid rate shape `{s}`")).with_hint(
        "use ramp:FROM..TO/DURATION (ramp:10..500/30s), step:R1,R2,…/DURATION (step:100,200,400/10s) or diurnal[:MIN..MAX/PERIOD]",
    )
}

fn rate_of(s: &str, whole: &str) -> Result<f64, OprobeError> {
    let v: f64 = s.trim().parse().map_err(|_| bad_shape(whole))?;
    if !v.is_finite() || v < 0.0 {
        return Err(bad_shape(whole));
    }
    Ok(v)
}

fn dur_of(s: &str, whole: &str) -> Result<Duration, OprobeError> {
    let d = crate::util::parse_duration(s.trim()).map_err(|_| bad_shape(whole))?;
    if d.is_zero() {
        return Err(bad_shape(whole));
    }
    Ok(d)
}

impl FromStr for RateShape {
    type Err = OprobeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let whole = s.trim();
        let lower = whole.to_ascii_lowercase();
        let (kind, rest) = lower.split_once(':').unwrap_or((lower.as_str(), ""));
        match kind {
            "ramp" => {
                let (rates, over) = rest.split_once('/').ok_or_else(|| bad_shape(whole))?;
                let (from, to) = rates.split_once("..").ok_or_else(|| bad_shape(whole))?;
                let (from, to) = (rate_of(from, whole)?, rate_of(to, whole)?);
                if from == 0.0 && to == 0.0 {
                    return Err(bad_shape(whole));
                }
                Ok(RateShape::Ramp {
                    from,
                    to,
                    over: dur_of(over, whole)?,
                })
            }
            "step" => {
                let (rates, every) = rest.split_once('/').ok_or_else(|| bad_shape(whole))?;
                let rates = rates
                    .split(',')
                    .map(|r| rate_of(r, whole))
                    .collect::<Result<Vec<_>, _>>()?;
                if rates.is_empty() || rates.iter().all(|r| *r == 0.0) {
                    return Err(bad_shape(whole));
                }
                Ok(RateShape::Step {
                    rates,
                    every: dur_of(every, whole)?,
                })
            }
            "diurnal" => {
                if rest.is_empty() {
                    return Ok(RateShape::Diurnal {
                        min: None,
                        max: None,
                        period: None,
                    });
                }
                let (rates, period) = match rest.split_once('/') {
                    Some((r, p)) => (r, Some(dur_of(p, whole)?)),
                    None => (rest, None),
                };
                let (min, max) = rates.split_once("..").ok_or_else(|| bad_shape(whole))?;
                let (min, max) = (rate_of(min, whole)?, rate_of(max, whole)?);
                if max <= 0.0 || min > max {
                    return Err(bad_shape(whole));
                }
                Ok(RateShape::Diurnal {
                    min: Some(min),
                    max: Some(max),
                    period,
                })
            }
            _ => Err(bad_shape(whole)),
        }
    }
}

fn fmt_rate(r: f64) -> String {
    if r.fract() == 0.0 {
        format!("{r:.0}")
    } else {
        format!("{r}")
    }
}

impl fmt::Display for RateShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = |d: &Duration| crate::util::HumanDuration(*d).to_string();
        match self {
            RateShape::Ramp { from, to, over } => {
                write!(f, "ramp:{}..{}/{}", fmt_rate(*from), fmt_rate(*to), d(over))
            }
            RateShape::Step { rates, every } => {
                let r: Vec<String> = rates.iter().map(|r| fmt_rate(*r)).collect();
                write!(f, "step:{}/{}", r.join(","), d(every))
            }
            RateShape::Diurnal { min, max, period } => {
                f.write_str("diurnal")?;
                if let (Some(lo), Some(hi)) = (min, max) {
                    write!(f, ":{}..{}", fmt_rate(*lo), fmt_rate(*hi))?;
                    if let Some(p) = period {
                        write!(f, "/{}", d(p))?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl Serialize for RateShape {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for RateShape {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for RateShape {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RateShape".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Time-varying open-loop rate: ramp:FROM..TO/DURATION, step:R1,R2,…/DURATION or diurnal[:MIN..MAX/PERIOD]"
        })
    }
}

impl RateShape {
    /// The kind (`ramp`, `step`, `diurnal`): `report.schedule.arrivals`.
    pub fn kind(&self) -> &'static str {
        match self {
            RateShape::Ramp { .. } => "ramp",
            RateShape::Step { .. } => "step",
            RateShape::Diurnal { .. } => "diurnal",
        }
    }

    /// The rate `t` into the window. `base` is `--rate` (the diurnal default peak) and
    /// `window` the measurement window (the diurnal default period).
    pub fn rate_at(&self, t: Duration, base: f64, window: Duration) -> f64 {
        let ts = t.as_secs_f64();
        match self {
            RateShape::Ramp { from, to, over } => {
                let x = (ts / over.as_secs_f64()).min(1.0);
                from + (to - from) * x
            }
            RateShape::Step { rates, every } => {
                let i = (ts / every.as_secs_f64()) as usize;
                rates[i.min(rates.len() - 1)]
            }
            RateShape::Diurnal { min, max, period } => {
                let hi = max.unwrap_or(base);
                let lo = min.unwrap_or(hi * 0.2);
                let p = period.unwrap_or(window).as_secs_f64().max(1e-3);
                // Starts at the trough, peaks half a period in.
                let phase = (1.0 - (2.0 * std::f64::consts::PI * ts / p).cos()) / 2.0;
                lo + (hi - lo) * phase
            }
        }
    }

    /// The same shape at `f` times the rate (a distributed run's share of it).
    pub fn scaled(&self, f: f64) -> RateShape {
        match self {
            RateShape::Ramp { from, to, over } => RateShape::Ramp {
                from: from * f,
                to: to * f,
                over: *over,
            },
            RateShape::Step { rates, every } => RateShape::Step {
                rates: rates.iter().map(|r| r * f).collect(),
                every: *every,
            },
            RateShape::Diurnal { min, max, period } => RateShape::Diurnal {
                min: min.map(|v| v * f),
                max: max.map(|v| v * f),
                period: *period,
            },
        }
    }

    /// The highest rate the shape reaches (the plan's and the slot pool's sizing).
    pub fn peak(&self, base: f64) -> f64 {
        match self {
            RateShape::Ramp { from, to, .. } => from.max(*to),
            RateShape::Step { rates, .. } => rates.iter().copied().fold(0.0, f64::max),
            RateShape::Diurnal { max, .. } => max.unwrap_or(base),
        }
    }

    /// Expected arrivals over `window` (the plan's estimate).
    pub fn expected_arrivals(&self, base: f64, window: Duration) -> f64 {
        let steps = 1000u32;
        let dt = window / steps;
        (0..steps)
            .map(|i| self.rate_at(dt * i + dt / 2, base, window) * dt.as_secs_f64())
            .sum()
    }
}

/// Generates intended start offsets (from the start of the window) one at a time.
#[derive(Debug)]
pub struct ArrivalClock {
    arrivals: Arrivals,
    shape: Option<RateShape>,
    base: f64,
    window: Duration,
    /// Offset of the last arrival handed out.
    t: Duration,
    started: bool,
}

/// Resolution of the numeric integration of a time-varying rate.
const STEP: f64 = 0.001;

impl ArrivalClock {
    /// `base`: `--rate` (ops/s); with a shape it is only the diurnal default peak.
    pub fn new(arrivals: Arrivals, shape: Option<RateShape>, base: f64, window: Duration) -> Self {
        Self {
            arrivals,
            shape,
            base: base.max(0.0),
            window,
            t: Duration::ZERO,
            started: false,
        }
    }

    /// `constant`, `poisson`, or the shape's kind (with `+poisson` when gaps are exponential).
    pub fn label(&self) -> String {
        match (&self.shape, self.arrivals) {
            (None, a) => a.label().to_owned(),
            (Some(s), Arrivals::Constant) => s.kind().to_owned(),
            (Some(s), Arrivals::Poisson) => format!("{}+poisson", s.kind()),
        }
    }

    pub fn rate_at(&self, t: Duration) -> f64 {
        match &self.shape {
            None => self.base,
            Some(s) => s.rate_at(t, self.base, self.window),
        }
    }

    /// The next intended start offset, or `None` once it would be at or past `horizon`.
    /// The first arrival of a constant process is at 0; every gap holds one unit of expected
    /// arrivals (`∫ rate dt = 1`, or an exponential draw for poisson).
    pub fn next(&mut self, rng: &mut SmallRng, horizon: Duration) -> Option<Duration> {
        let need: f64 = match self.arrivals {
            Arrivals::Constant if !self.started => 0.0,
            Arrivals::Constant => 1.0,
            Arrivals::Poisson => {
                let u: f64 = rng.r#gen::<f64>();
                -(1.0 - u).max(f64::MIN_POSITIVE).ln()
            }
        };
        self.started = true;
        let h = horizon.as_secs_f64();
        let mut t = self.t.as_secs_f64();
        if need > 0.0 {
            match &self.shape {
                None => {
                    if self.base <= 0.0 {
                        return None;
                    }
                    t += need / self.base;
                }
                Some(_) => {
                    let mut left = need;
                    loop {
                        if t >= h {
                            return None;
                        }
                        let r = self.rate_at(Duration::from_secs_f64(t)).max(0.0);
                        if r * STEP >= left {
                            t += left / r;
                            break;
                        }
                        left -= r * STEP;
                        t += STEP;
                    }
                }
            }
        }
        if t >= h {
            return None;
        }
        self.t = Duration::from_secs_f64(t);
        Some(self.t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn count(c: &mut ArrivalClock, window: Duration) -> Vec<Duration> {
        let mut rng = SmallRng::seed_from_u64(7);
        let mut v = Vec::new();
        while let Some(t) = c.next(&mut rng, window) {
            v.push(t);
        }
        v
    }

    #[test]
    fn constant_arrivals_are_evenly_spaced() {
        let w = Duration::from_secs(2);
        let mut c = ArrivalClock::new(Arrivals::Constant, None, 100.0, w);
        let v = count(&mut c, w);
        assert_eq!(v.len(), 200);
        assert_eq!(v[0], Duration::ZERO);
        let gap = v[1] - v[0];
        assert!((gap.as_secs_f64() - 0.01).abs() < 1e-9);
    }

    #[test]
    fn poisson_arrivals_have_the_rate_on_average_and_vary() {
        let w = Duration::from_secs(20);
        let mut c = ArrivalClock::new(Arrivals::Poisson, None, 200.0, w);
        let v = count(&mut c, w);
        let n = v.len() as f64;
        assert!((n - 4000.0).abs() < 4000.0 * 0.06, "{n}");
        let gaps: Vec<f64> = v.windows(2).map(|p| (p[1] - p[0]).as_secs_f64()).collect();
        let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        let var = gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / gaps.len() as f64;
        // Exponential: standard deviation ≈ mean.
        assert!((var.sqrt() / mean - 1.0).abs() < 0.1, "{mean} {var}");
    }

    #[test]
    fn shapes_parse_print_and_integrate() {
        for s in [
            "ramp:10..500/30s",
            "step:100,200,400/10s",
            "diurnal",
            "diurnal:5..50/1m",
            "diurnal:5..50",
        ] {
            let p: RateShape = s.parse().unwrap();
            assert_eq!(p.to_string(), s);
        }
        for bad in [
            "ramp:10/3s",
            "ramp:0..0/1s",
            "step:/1s",
            "nope",
            "diurnal:9..1",
            "ramp:1..2/0s",
        ] {
            assert!(bad.parse::<RateShape>().is_err(), "{bad}");
        }
        // A ramp from 0 to 200 over 2 s holds 200 arrivals on average.
        let w = Duration::from_secs(2);
        let shape: RateShape = "ramp:0..200/2s".parse().unwrap();
        assert!((shape.expected_arrivals(0.0, w) - 200.0).abs() < 1.0);
        let mut c = ArrivalClock::new(Arrivals::Constant, Some(shape), 0.0, w);
        assert_eq!(c.label(), "ramp");
        let v = count(&mut c, w);
        assert!((v.len() as i64 - 200).abs() <= 2, "{}", v.len());
        // More arrivals in the second half than in the first (3× for a linear ramp).
        let first = v.iter().filter(|t| t.as_secs_f64() < 1.0).count();
        assert!(v.len() - first > 2 * first, "{first} of {}", v.len());
        // Steps: 100/s then 300/s.
        let shape: RateShape = "step:100,300/1s".parse().unwrap();
        let mut c = ArrivalClock::new(Arrivals::Constant, Some(shape.clone()), 0.0, w);
        let v = count(&mut c, w);
        assert!((v.len() as i64 - 400).abs() <= 2, "{}", v.len());
        assert_eq!(shape.peak(0.0), 300.0);
        // Diurnal defaults: between 20 % of --rate and --rate, one period per window.
        let shape: RateShape = "diurnal".parse().unwrap();
        assert_eq!(shape.rate_at(Duration::ZERO, 100.0, w), 20.0);
        assert!((shape.rate_at(Duration::from_secs(1), 100.0, w) - 100.0).abs() < 1e-9);
    }
}

/// Object sizes of a generated dataset (design v5 §3, M12): `--object-size
/// dist:lognormal(MEDIAN,SIGMA)` (e.g. `dist:lognormal(256KiB,1.5)`) or `file:PATH` (one
/// size per line, used in turn).
#[derive(Clone, Debug, PartialEq)]
pub enum SizeDist {
    LogNormal {
        median: u64,
        sigma: f64,
    },
    File {
        path: std::path::PathBuf,
        sizes: Vec<u64>,
    },
}

/// Largest generated object of a distribution.
pub const MAX_DIST_OBJECT: u64 = 1 << 30;

fn bad_dist(s: &str) -> OprobeError {
    crate::input::invalid(format!("invalid object size distribution `{s}`"))
        .with_hint("use dist:lognormal(MEDIAN,SIGMA), e.g. dist:lognormal(256KiB,1.5), or file:PATH with one size per line")
}

impl SizeDist {
    /// Whether `s` names a distribution rather than one size.
    pub fn is_dist(s: &str) -> bool {
        let l = s.trim().to_ascii_lowercase();
        l.starts_with("dist:") || l.starts_with("file:")
    }

    /// The median (lognormal) or the first size (file): the dataset's nominal size.
    pub fn nominal(&self) -> u64 {
        match self {
            SizeDist::LogNormal { median, .. } => *median,
            SizeDist::File { sizes, .. } => {
                let mut v = sizes.clone();
                v.sort_unstable();
                v[v.len() / 2]
            }
        }
    }

    /// The size of object `i` (deterministic for a seed).
    pub fn size_of(&self, seed: u64, i: usize) -> u64 {
        match self {
            SizeDist::LogNormal { median, sigma } => {
                use rand::SeedableRng;
                let mut rng = SmallRng::seed_from_u64(seed ^ (i as u64).wrapping_mul(0x9E37_79B9));
                // Box–Muller.
                let u1: f64 = rng.r#gen::<f64>().max(f64::MIN_POSITIVE);
                let u2: f64 = rng.r#gen::<f64>();
                let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
                ((*median as f64) * (sigma * z).exp())
                    .round()
                    .clamp(1.0, MAX_DIST_OBJECT as f64) as u64
            }
            SizeDist::File { sizes, .. } => sizes[i % sizes.len()],
        }
    }
}

impl FromStr for SizeDist {
    type Err = OprobeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        let l = t.to_ascii_lowercase();
        if let Some(rest) = l.strip_prefix("dist:lognormal(") {
            let inner = rest.strip_suffix(')').ok_or_else(|| bad_dist(t))?;
            let (m, sg) = inner.split_once(',').ok_or_else(|| bad_dist(t))?;
            let median = crate::input::nonzero_size(m.trim())
                .map_err(|_| bad_dist(t))?
                .0;
            let sigma: f64 = sg.trim().parse().map_err(|_| bad_dist(t))?;
            if !(sigma.is_finite() && (0.0..=5.0).contains(&sigma)) {
                return Err(bad_dist(t));
            }
            return Ok(SizeDist::LogNormal { median, sigma });
        }
        if l.starts_with("file:") {
            let path = std::path::PathBuf::from(&t[5..]);
            let text = std::fs::read_to_string(&path).map_err(|e| {
                crate::input::invalid(format!("cannot read sizes from `{}`: {e}", path.display()))
            })?;
            let sizes = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| crate::input::nonzero_size(l).map(|b| b.0.min(MAX_DIST_OBJECT)))
                .collect::<Result<Vec<_>, _>>()?;
            if sizes.is_empty() {
                return Err(bad_dist(t));
            }
            return Ok(SizeDist::File { path, sizes });
        }
        Err(bad_dist(t))
    }
}

impl fmt::Display for SizeDist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SizeDist::LogNormal { median, sigma } => write!(
                f,
                "dist:lognormal({},{sigma})",
                crate::util::ByteSize(*median)
            ),
            SizeDist::File { path, .. } => write!(f, "file:{}", path.display()),
        }
    }
}

impl Serialize for SizeDist {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SizeDist {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for SizeDist {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SizeDist".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Object size distribution: dist:lognormal(MEDIAN,SIGMA) or file:PATH"
        })
    }
}

#[cfg(test)]
mod size_tests {
    use super::*;

    #[test]
    fn lognormal_sizes_spread_around_the_median() {
        let d: SizeDist = "dist:lognormal(64KiB,1.0)".parse().unwrap();
        assert_eq!(d.to_string(), "dist:lognormal(64KiB,1)");
        assert_eq!(d.nominal(), 65536);
        let mut v: Vec<u64> = (0..2001).map(|i| d.size_of(7, i)).collect();
        v.sort_unstable();
        let med = v[1000] as f64;
        assert!((med / 65536.0 - 1.0).abs() < 0.15, "{med}");
        assert!(v[1990] > 4 * v[1000] && v[10] < v[1000] / 4);
        assert_eq!(d.size_of(7, 3), d.size_of(7, 3));
        for bad in [
            "dist:lognormal(1MiB)",
            "dist:pareto(1,2)",
            "dist:lognormal(0,1)",
            "file:/nope",
        ] {
            assert!(bad.parse::<SizeDist>().is_err(), "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sizes.txt");
        std::fs::write(&f, "# sizes\n4KiB\n1MiB\n\n100\n").unwrap();
        let d: SizeDist = format!("file:{}", f.display()).parse().unwrap();
        assert_eq!(d.size_of(0, 1), 1 << 20);
        assert_eq!(d.size_of(0, 4), 1 << 20);
        assert_eq!(d.nominal(), 4096);
    }
}
