//! Exporters of a stored run (`oprobe runs export`): OTLP/JSON traces and metrics, HAR 1.2,
//! CSV and NDJSON rows; the Prometheus text format used by `oprobe watch`.
//!
//! Exports never contain credentials: URLs are built from the redacted target display and the
//! object key, without query strings.

pub mod csv;
pub mod har;
pub mod ndjson;
pub mod otlp;
pub mod prometheus;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::model::{Report, Run};

/// Parse an RFC 3339 UTC timestamp (`2026-11-20T14:30:05Z`, optional fraction).
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<u32>().ok();
    let base = crate::util::utc_time(
        n(0..4)? as i64,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    )?;
    let rest = s.get(19..).unwrap_or("");
    let frac = rest
        .strip_prefix('.')
        .map(|f| {
            let digits: String = f.chars().take_while(char::is_ascii_digit).collect();
            let nanos = format!("{digits:0<9}");
            nanos
                .get(..9)
                .and_then(|d| d.parse::<u32>().ok())
                .unwrap_or(0)
        })
        .unwrap_or(0);
    Some(base + Duration::from_nanos(frac as u64))
}

/// Unix nanoseconds of the measurement window start (`t_us == 0` of the trace rows).
pub fn window_start_unix_ns(r: &Report) -> u128 {
    let end = parse_rfc3339(&r.run.ended_at)
        .or_else(|| parse_rfc3339(&r.run.started_at))
        .unwrap_or(UNIX_EPOCH);
    let ns = end
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    ns.saturating_sub((r.run.window_s.max(0.0) * 1e9) as u128)
}

/// Unix nanoseconds of a row time (µs from the window start).
pub fn abs_ns(start_ns: u128, t_us: i64) -> u128 {
    if t_us >= 0 {
        start_ns + t_us as u128 * 1000
    } else {
        start_ns.saturating_sub(t_us.unsigned_abs() as u128 * 1000)
    }
}

/// RFC 3339 with milliseconds for a unix-nanosecond time.
pub fn rfc3339_ms(ns: u128) -> String {
    let t = UNIX_EPOCH + Duration::from_nanos(ns as u64);
    let base = crate::util::fmt_rfc3339(t);
    let ms = (ns / 1_000_000) % 1000;
    match base.strip_suffix('Z') {
        Some(b) => format!("{b}.{ms:03}Z"),
        None => base,
    }
}

/// The redacted URL of an object key of the run's target (no credentials, no query).
pub fn object_url(r: &Report, key: &str) -> String {
    let base = crate::util::display_url(&r.target.display);
    let base = base.trim_end_matches('/');
    if key.is_empty() {
        base.to_owned()
    } else {
        format!("{base}/{}", key.trim_start_matches('/'))
    }
}

/// The bucket of an S3-style target (`s3://bucket/…` or the `bucket` option).
pub fn bucket(r: &Report) -> Option<String> {
    if let Some(b) = r.target.options.get("bucket") {
        return Some(b.clone());
    }
    let d = &r.target.display;
    let rest = d.split_once("://")?.1;
    let b = rest.split('/').next()?;
    (!b.is_empty()).then(|| b.to_owned())
}

/// Index of op rows by id.
pub(crate) fn ops_by_id(run: &Run) -> std::collections::HashMap<u64, &crate::model::OpRow> {
    run.ops.iter().map(|o| (o.op_id, o)).collect()
}

/// Export formats of `oprobe runs export`.
pub const FORMATS: [&str; 7] = ["json", "ndjson", "csv", "har", "otlp", "md", "html"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_round_trip() {
        let t = parse_rfc3339("2026-11-20T14:30:05.250Z").unwrap();
        let ns = t.duration_since(UNIX_EPOCH).unwrap().as_nanos();
        assert_eq!(rfc3339_ms(ns), "2026-11-20T14:30:05.250Z");
        assert!(parse_rfc3339("nope").is_none());
    }
}
