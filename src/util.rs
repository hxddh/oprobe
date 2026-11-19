//! Small shared helpers: human-friendly byte sizes and durations, formatting, redaction.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A byte quantity that (de)serializes as a human string (`"4MiB"`, `"512KiB"`, `"1GB"`) or a number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub const fn b(n: u64) -> Self {
        Self(n)
    }
    pub const fn kib(n: u64) -> Self {
        Self(n * 1024)
    }
    pub const fn mib(n: u64) -> Self {
        Self(n * 1024 * 1024)
    }
    pub fn as_u64(self) -> u64 {
        self.0
    }
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

impl FromStr for ByteSize {
    type Err = anyhow::Error;
    fn from_str(raw: &str) -> Result<Self> {
        let s = raw.trim();
        if s.is_empty() {
            bail!("empty size");
        }
        let split = s
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_'))
            .unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let num: f64 = num.replace('_', "").parse().with_context(|| {
            format!("invalid size `{raw}` (examples: 4MiB, 512KiB, 1GB, 65536)")
        })?;
        let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1.0,
            "k" | "kb" => 1e3,
            "ki" | "kib" => 1024.0,
            "m" | "mb" => 1e6,
            "mi" | "mib" => 1024.0 * 1024.0,
            "g" | "gb" => 1e9,
            "gi" | "gib" => 1024.0 * 1024.0 * 1024.0,
            "t" | "tb" => 1e12,
            "ti" | "tib" => 1024.0_f64.powi(4),
            other => bail!("unknown size unit `{other}` in `{raw}`"),
        };
        if num < 0.0 {
            bail!("size must be non-negative");
        }
        Ok(Self((num * mult).round() as u64))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        const UNITS: [(&str, u64); 4] = [
            ("TiB", 1 << 40),
            ("GiB", 1 << 30),
            ("MiB", 1 << 20),
            ("KiB", 1 << 10),
        ];
        for (name, unit) in UNITS {
            if v >= unit && v.is_multiple_of(unit) {
                return write!(f, "{}{name}", v / unit);
            }
        }
        write!(f, "{v}B")
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            N(u64),
            S(String),
        }
        match Raw::deserialize(d)? {
            Raw::N(n) => Ok(ByteSize(n)),
            Raw::S(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

/// A duration that (de)serializes as a human string (`"10s"`, `"500ms"`, `"2m"`) or seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    pub fn secs(s: u64) -> Self {
        Self(Duration::from_secs(s))
    }
    pub fn get(self) -> Duration {
        self.0
    }
}

impl FromStr for HumanDuration {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        parse_duration(s).map(Self)
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0.as_millis();
        if ms == 0 {
            write!(f, "0s")
        } else if ms.is_multiple_of(60_000) {
            write!(f, "{}m", ms / 60_000)
        } else if ms.is_multiple_of(1000) {
            write!(f, "{}s", ms / 1000)
        } else {
            write!(f, "{ms}ms")
        }
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            N(f64),
            S(String),
        }
        match Raw::deserialize(d)? {
            Raw::N(n) if n >= 0.0 => Ok(Self(Duration::from_secs_f64(n))),
            Raw::N(_) => Err(serde::de::Error::custom("duration must be >= 0")),
            Raw::S(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

/// Parse `"10s"`, `"500ms"`, `"1.5m"`, `"1h"`, `"250us"` or plain seconds (`"5"`).
pub fn parse_duration(raw: &str) -> Result<Duration> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        bail!("duration cannot be empty");
    }
    let (num, mult) = if let Some(v) = s.strip_suffix("ms") {
        (v, 1e-3)
    } else if let Some(v) = s.strip_suffix("us").or_else(|| s.strip_suffix("µs")) {
        (v, 1e-6)
    } else if let Some(v) = s.strip_suffix('h') {
        (v, 3600.0)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 60.0)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1.0)
    } else {
        (s.as_str(), 1.0)
    };
    let val: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid duration `{raw}` (examples: 10s, 500ms, 2m)"))?;
    if val.is_nan() || val < 0.0 || !val.is_finite() {
        bail!("duration must be a non-negative number");
    }
    Ok(Duration::from_secs_f64(val * mult))
}

/// Format bytes in binary units with one decimal (`"12.5 MiB"`).
pub fn fmt_bytes(v: u64) -> String {
    let f = v as f64;
    if f >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GiB", f / (1024.0 * 1024.0 * 1024.0))
    } else if f >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", f / (1024.0 * 1024.0))
    } else if f >= 1024.0 {
        format!("{:.1} KiB", f / 1024.0)
    } else {
        format!("{v} B")
    }
}

/// Format a millisecond latency with adaptive precision.
pub fn fmt_ms(ms: f64) -> String {
    if !ms.is_finite() {
        "-".to_owned()
    } else if ms >= 1000.0 {
        format!("{:.2}s", ms / 1000.0)
    } else if ms >= 100.0 {
        format!("{ms:.0}ms")
    } else if ms >= 10.0 {
        format!("{ms:.1}ms")
    } else {
        format!("{ms:.2}ms")
    }
}

/// Format USD amounts.
pub fn fmt_usd(v: f64) -> String {
    if v.abs() >= 1000.0 {
        format!("${v:.0}")
    } else if v.abs() >= 1.0 {
        format!("${v:.2}")
    } else {
        format!("${v:.4}")
    }
}

/// Keys whose values must never be printed or persisted.
pub fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "secret",
        "password",
        "token",
        "credential",
        "access_key",
        "account_key",
        "sas",
        "private_key",
        "application_key",
    ]
    .iter()
    .any(|needle| k.contains(needle))
}

/// Replace secret values with `"<redacted>"`.
pub fn redact_options(
    opts: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    opts.iter()
        .map(|(k, v)| {
            if is_secret_key(k) {
                (k.clone(), "<redacted>".to_owned())
            } else {
                (k.clone(), v.clone())
            }
        })
        .collect()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month, day)` of a day number counted from 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

/// Today's day number (days since 1970-01-01, UTC).
pub fn utc_today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        .div_euclid(86_400)
}

/// `SystemTime` from UTC calendar fields; `None` if out of range.
pub fn utc_time(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<std::time::SystemTime> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + (h * 3600 + mi * 60 + s) as i64;
    let secs = u64::try_from(secs).ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

/// RFC 3339 UTC timestamp with seconds precision: `2026-11-19T09:30:00Z`.
pub fn fmt_rfc3339(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let r = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        r / 3600,
        r % 3600 / 60,
        r % 60
    )
}

/// UTC timestamp formatted as RFC 3339 (seconds precision).
pub fn now_rfc3339() -> String {
    fmt_rfc3339(std::time::SystemTime::now())
}

/// Compact UTC timestamp for object prefixes: `20261119T093000Z`.
pub fn compact_utc_now() -> String {
    // 2026-11-19T09:30:00Z -> 20261119T093000Z
    now_rfc3339().replace(['-', ':'], "")
}

/// Parse a compact UTC timestamp produced by [`compact_utc_now`].
pub fn parse_compact_utc(s: &str) -> Option<std::time::SystemTime> {
    if s.len() != 16 || !s.is_ascii() || &s[8..9] != "T" || &s[15..] != "Z" {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s[r].parse::<u32>().ok();
    utc_time(
        n(0..4)? as i64,
        n(4..6)?,
        n(6..8)?,
        n(9..11)?,
        n(11..13)?,
        n(13..15)?,
    )
}

/// CRC-32C (Castagnoli), hardware-accelerated where available (SSE 4.2 / ARMv8 CRC).
pub fn crc32c(data: &[u8]) -> u32 {
    ::crc32c::crc32c(data)
}

/// Continue a CRC-32C over more data (`crc` is a previous result, `0` to start).
pub fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    ::crc32c::crc32c_append(crc, data)
}

pub fn pct(num: f64, den: f64) -> f64 {
    if den > 0.0 { num / den * 100.0 } else { 0.0 }
}

pub fn ratio(num: f64, den: f64) -> f64 {
    if den > 0.0 { num / den } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes_parse_and_print() {
        assert_eq!("4MiB".parse::<ByteSize>().unwrap(), ByteSize::mib(4));
        assert_eq!("512 KiB".parse::<ByteSize>().unwrap(), ByteSize::kib(512));
        assert_eq!("1GB".parse::<ByteSize>().unwrap().0, 1_000_000_000);
        assert_eq!("65536".parse::<ByteSize>().unwrap().0, 65536);
        assert_eq!(ByteSize::mib(8).to_string(), "8MiB");
        assert_eq!(ByteSize(1000).to_string(), "1000B");
        assert!("4XB".parse::<ByteSize>().is_err());
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1.5m").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("2").unwrap(), Duration::from_secs(2));
        assert!(parse_duration("abc").is_err());
        assert_eq!(HumanDuration::secs(120).to_string(), "2m");
    }

    #[test]
    fn crc32c_matches_reference_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        assert_eq!(
            crc32c_append(crc32c(&data[..333]), &data[333..]),
            crc32c(&data)
        );
    }

    #[test]
    fn rfc3339_formatting_and_calendar_math() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(fmt_rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            fmt_rfc3339(UNIX_EPOCH + Duration::from_secs(784_111_777)),
            "1994-11-06T08:49:37Z"
        );
        assert_eq!(
            fmt_rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
        assert_eq!(
            utc_time(2000, 2, 29, 0, 0, 0),
            Some(UNIX_EPOCH + Duration::from_secs(951_782_400))
        );
        assert_eq!(utc_time(2026, 13, 1, 0, 0, 0), None);
        assert!(parse_compact_utc("2026111xT093000Z").is_none());
    }

    #[test]
    fn compact_timestamps_roundtrip() {
        let s = compact_utc_now();
        assert!(parse_compact_utc(&s).is_some(), "{s}");
    }

    #[test]
    fn secrets_are_redacted() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("secret_access_key".to_owned(), "x".to_owned());
        m.insert("bucket".to_owned(), "b".to_owned());
        let r = redact_options(&m);
        assert_eq!(r["secret_access_key"], "<redacted>");
        assert_eq!(r["bucket"], "b");
    }
}
