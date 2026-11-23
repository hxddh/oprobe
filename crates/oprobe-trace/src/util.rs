//! Small helpers shared with `oprobe-core` (which re-exports them from its `util`).

/// Remove `user:password@` from every URL embedded in `s` (`http://u:p@host:9000/x` →
/// `http://host:9000/x`). Other text is returned unchanged, byte for byte.
pub fn strip_url_credentials(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let auth_end = tail
            .find(['/', '?', '#', ' ', '\'', '"'])
            .unwrap_or(tail.len());
        let authority = &tail[..auth_end];
        match authority.rfind('@') {
            Some(at) => out.push_str(&authority[at + 1..]),
            None => out.push_str(authority),
        }
        rest = &tail[auth_end..];
    }
    out.push_str(rest);
    out
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
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

pub fn pct(num: f64, den: f64) -> f64 {
    if den > 0.0 { num / den * 100.0 } else { 0.0 }
}

pub fn ratio(num: f64, den: f64) -> f64 {
    if den > 0.0 { num / den } else { 0.0 }
}


/// A fast, non-cryptographic hasher for the recorder's maps (operation ids, keys, prefixes):
/// the `FxHash` of rustc. The keys are the application's own (or hashed) names and ids, and
/// the maps are bounded.
#[derive(Clone, Copy, Default)]
pub struct FxHasher(u64);

const FX_SEED: u64 = 0x517c_c1b7_2722_0a95;

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(FX_SEED);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        if !rest.is_empty() {
            let mut b = [0u8; 8];
            b[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(b));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// A `HashMap` with [`FxHasher`].
pub type FxHashMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<FxHasher>>;

/// A `HashSet` with [`FxHasher`].
pub type FxHashSet<K> = std::collections::HashSet<K, std::hash::BuildHasherDefault<FxHasher>>;
