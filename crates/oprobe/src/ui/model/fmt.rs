//! Number formatting for the interactive surfaces: compact, unit-carrying, fixed width where it
//! matters (KPIs never wrap; table cells keep their column).

/// `812`, `1.23k`, `12.3k`, `123k`, `1.23M`, … (3 significant digits).
pub fn compact(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    let a = v.abs();
    let (x, suffix) = if a >= 1e12 {
        (v / 1e12, "T")
    } else if a >= 1e9 {
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

/// Three significant digits, no trailing zeros beyond what the magnitude needs.
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

/// Bytes, binary units: `512 B`, `1.5 KiB`, `12.3 MiB`.
pub fn bytes(b: f64) -> String {
    if !b.is_finite() {
        return "—".into();
    }
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b;
    let mut i = 0;
    while v.abs() >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} B")
    } else {
        format!("{} {}", sig3(v), units[i])
    }
}

/// A percentage given as 0–100: `12.3 %`, `0.42 %`, `100 %`.
pub fn pct(p: f64) -> String {
    if !p.is_finite() {
        return "—".into();
    }
    if p == 0.0 {
        "0 %".into()
    } else if p.abs() < 0.01 {
        "<0.01 %".into()
    } else {
        format!("{} %", sig3(p))
    }
}

/// A ratio given as 0–1, as a percentage.
pub fn ratio(r: f64) -> String {
    pct(r * 100.0)
}

/// US dollars: `$0.0042`, `$1.23`, `$1.2k`.
pub fn usd(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    let a = v.abs();
    if a >= 1000.0 {
        format!("${}", compact(v))
    } else if a >= 1.0 {
        format!("${v:.2}")
    } else if a == 0.0 {
        "$0".into()
    } else {
        format!("${}", sig3(v))
    }
}

/// A signed relative change: `+12.3 %`, `−4.0 %`.
pub fn delta_pct(p: f64) -> String {
    if !p.is_finite() {
        return "—".into();
    }
    if p.abs() < 0.01 {
        return "±0 %".into();
    }
    let s = sig3(p.abs());
    if p > 0.0 {
        format!("+{s} %")
    } else if p < 0.0 {
        format!("−{s} %")
    } else {
        "±0 %".into()
    }
}

/// Seconds as a timeline position: `0.00 s`, `12.3 s`, `2:05`.
pub fn clock_s(s: f64) -> String {
    if s.abs() < 100.0 {
        format!("{} s", sig3(s))
    } else {
        let t = s.round() as i64;
        format!("{}:{:02}", t / 60, t.rem_euclid(60))
    }
}

/// `2026-11-20T14:30:05Z` → `14:30` (today's runs) or `Nov 20 14:30`.
pub fn short_time(rfc3339: &str, today: &str) -> String {
    let (date, time) = rfc3339.split_once('T').unwrap_or((rfc3339, ""));
    let hm = time.get(..5).unwrap_or(time);
    if date == today {
        return hm.to_owned();
    }
    let month = date
        .get(5..7)
        .and_then(|m| m.parse::<usize>().ok())
        .and_then(|m| {
            [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ]
            .get(m.wrapping_sub(1))
            .copied()
        })
        .unwrap_or("");
    let day = date.get(8..10).unwrap_or("").trim_start_matches('0');
    format!("{month} {day} {hm}")
}

/// The calendar day of an RFC 3339 time (`2026-11-20`).
pub fn day_of(rfc3339: &str) -> &str {
    rfc3339.split('T').next().unwrap_or(rfc3339)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_numbers_never_grow_past_five_characters() {
        assert_eq!(compact(812.0), "812");
        assert_eq!(compact(1234.0), "1.23k");
        assert_eq!(compact(12_345.0), "12.3k");
        assert_eq!(compact(123_456.0), "123k");
        assert_eq!(compact(1_234_567.0), "1.23M");
        assert_eq!(compact(0.5), "0.500");
        for v in [1.0, 99.9, 999.0, 1e4, 9.99e5, 1e7, 5e9] {
            assert!(compact(v).chars().count() <= 5, "{v} → {}", compact(v));
        }
    }

    #[test]
    fn durations_pick_their_unit() {
        assert_eq!(dur_us(812.0), "812 µs");
        assert_eq!(dur_us(1234.0), "1.23 ms");
        assert_eq!(dur_us(123_400.0), "123 ms");
        assert_eq!(dur_us(1_234_000.0), "1.23 s");
        assert_eq!(dur_ms(0.5), "500 µs");
    }

    #[test]
    fn bytes_and_money() {
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1536.0), "1.50 KiB");
        assert_eq!(usd(0.0042), "$0.00420");
        assert_eq!(usd(12.5), "$12.50");
        assert_eq!(delta_pct(-4.0), "−4.00 %");
        assert_eq!(pct(0.001), "<0.01 %");
    }

    #[test]
    fn short_times() {
        assert_eq!(short_time("2026-11-20T14:30:05Z", "2026-11-20"), "14:30");
        assert_eq!(
            short_time("2026-11-19T09:01:00Z", "2026-11-20"),
            "Nov 19 09:01"
        );
    }
}

/// A report section as `(name, value)` rows for a property list, whatever its shape: nested
/// objects flatten to `a · b`, arrays of objects show one row per element (the first `max`),
/// zeros, empties and `false` are left out, names lose their unit suffix into the value
/// (`missed_arrivals_ratio: 0.02` → `missed arrivals` `2.00 %`). Sections other streams fill
/// later (schedule, self, dimensions, client, workers) light up without a view change.
pub fn section_rows(v: &serde_json::Value, max: usize) -> Vec<(String, String)> {
    fn name(k: &str) -> String {
        let base = [
            "_count", "_ratio", "_ms", "_us", "_bytes", "_usd", "_per_s", "_pct",
        ]
        .iter()
        .find_map(|s| k.strip_suffix(s))
        .unwrap_or(k);
        base.replace('_', " ")
    }
    fn value(k: &str, v: &serde_json::Value) -> Option<String> {
        use serde_json::Value as J;
        match v {
            J::Null => None,
            J::Bool(false) => None,
            J::Bool(true) => Some("yes".into()),
            J::Number(n) => {
                let x = n.as_f64()?;
                if x == 0.0 {
                    return None;
                }
                Some(if k.ends_with("_ratio") {
                    ratio(x)
                } else if k.ends_with("_ms") {
                    dur_ms(x)
                } else if k.ends_with("_us") {
                    dur_us(x)
                } else if k.ends_with("_bytes") {
                    bytes(x)
                } else if k.ends_with("_usd") {
                    usd(x)
                } else if k.ends_with("_pct") {
                    format!("{} %", sig3(x))
                } else {
                    compact(x)
                })
            }
            J::String(s) if s.is_empty() => None,
            J::String(s) => Some(s.clone()),
            J::Array(a) if a.is_empty() => None,
            J::Array(a) if a.iter().all(|x| !x.is_object()) => Some(
                a.iter()
                    .filter_map(|x| value("", x))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            _ => None,
        }
    }
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, String)>, max: usize) {
        use serde_json::Value as J;
        match v {
            J::Object(m) => {
                for (k, x) in m {
                    let n = if prefix.is_empty() {
                        name(k)
                    } else {
                        format!("{prefix} · {}", name(k))
                    };
                    match x {
                        J::Object(_) => walk(&n, x, out, max),
                        J::Array(a) if a.iter().any(|e| e.is_object()) => {
                            for (i, e) in a.iter().take(max).enumerate() {
                                walk(&format!("{n} {}", i + 1), e, out, max);
                            }
                        }
                        _ => {
                            if let Some(s) = value(k, x) {
                                out.push((n, s));
                            }
                        }
                    }
                }
            }
            other => {
                if let Some(s) = value("", other) {
                    out.push((prefix.to_owned(), s));
                }
            }
        }
    }
    let mut out = Vec::new();
    match v {
        // A list of records (workers): one group of rows each.
        serde_json::Value::Array(a) if a.iter().any(|e| e.is_object()) => {
            for (i, e) in a.iter().take(max).enumerate() {
                walk(&format!("{}", i + 1), e, &mut out, max);
            }
        }
        _ => walk("", v, &mut out, max),
    }
    out.truncate(max * 4);
    out
}

#[cfg(test)]
mod section_tests {
    #[test]
    fn sections_become_property_rows() {
        let v = serde_json::json!({
            "open_loop": true,
            "missed_arrivals_ratio": 0.02,
            "intended_ops_count": 0,
            "lag": {"p99_ms": 12.5},
            "workers": [{"id": "w1", "ops_per_s": 1200.0}],
            "phases_available": ["queue", "server"],
            "empty": "",
        });
        let rows = super::section_rows(&v, 5);
        let get = |k: &str| rows.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("open loop"), Some("yes"));
        assert_eq!(get("missed arrivals"), Some("2.00 %"));
        assert_eq!(get("intended ops"), None);
        assert_eq!(get("lag · p99"), Some("12.5 ms"));
        assert_eq!(get("workers 1 · id"), Some("w1"));
        assert_eq!(get("phases available"), Some("queue, server"));
        assert_eq!(get("empty"), None);
        let w = super::section_rows(&serde_json::json!([{"id": "w1"}, {"id": "w2"}]), 5);
        assert_eq!(
            w,
            vec![
                ("1 · id".into(), "w1".into()),
                ("2 · id".into(), "w2".into())
            ]
        );
        assert!(super::section_rows(&serde_json::json!([]), 5).is_empty());
    }
}
