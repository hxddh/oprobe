//! Small shared helpers: human-friendly byte sizes and durations, formatting, redaction.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use oprobe_trace::util::{
    civil_from_days, days_from_civil, fmt_rfc3339, now_rfc3339, pct, ratio, strip_url_credentials,
    utc_time,
};

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
    /// Typed: see [`crate::input::size`].
    fn from_str(raw: &str) -> Result<Self> {
        Ok(crate::input::size(raw)?)
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

impl schemars::JsonSchema for ByteSize {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ByteSize".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A byte size: `4MiB`, `512KiB`, `1GB` (a plain number of bytes is accepted on input)."
        })
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

impl schemars::JsonSchema for HumanDuration {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "HumanDuration".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A duration: `10s`, `500ms`, `2m` (plain seconds are accepted on input)."
        })
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

/// A page of the documentation as this version shipped it (`docs/concepts.md#glossary` →
/// `https://github.com/hxddh/oprobe/blob/v6.3.0/docs/concepts.md#glossary`).
pub fn docs_url(path: &str) -> String {
    format!(
        "https://github.com/hxddh/oprobe/blob/v{}/{path}",
        env!("CARGO_PKG_VERSION")
    )
}

/// The glossary of the terms a profile prints (goodput, TTFB, 95 % interval, …).
pub fn glossary_url() -> String {
    docs_url("docs/concepts.md#glossary")
}

/// Parse `"10s"`, `"500ms"`, `"1.5m"`, `"1h"`, `"250us"` or plain seconds (`"5"`); typed errors
/// from [`crate::input::duration`].
pub fn parse_duration(raw: &str) -> Result<Duration> {
    Ok(crate::input::duration(raw)?)
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
/// `1 request`, `2 requests` (review C14: not `1 requests`). `noun` is the singular; the
/// plural adds `s`.
pub fn plural(n: impl Into<u64>, noun: &str) -> String {
    let n = n.into();
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

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
///
/// Covers OpenDAL's credential options across services (`secret_access_key`, `account_key`,
/// `sas_token`, `session_token`, `password`, `credential`, `private_key`, ...), customer-supplied
/// encryption keys (`server_side_encryption_customer_key`, `encryption_key`, ...) and generally
/// any `*_key`. `access_key_id` stays redacted as before; non-secret encryption settings such as
/// `server_side_encryption = "aws:kms"` remain visible.
pub fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    let k = k.as_str();
    const NEEDLES: [&str; 13] = [
        "secret",
        "password",
        "passwd",
        "token",
        "credential",
        "access_key",
        "account_key",
        "sas",
        "private_key",
        "application_key",
        "api_key",
        "apikey",
        "signature",
    ];
    NEEDLES.iter().any(|n| k.contains(n))
        || k == "key"
        || k.ends_with("_key")
        || k.ends_with("-key")
        || (k.contains("encryption") && k.contains("key"))
        || k.contains("authorization")
}

/// Query parameter names whose values must never be printed or persisted: the option names
/// of [`is_secret_key`] (an operator URL carries options as its query) and the parameters of
/// presigned and SAS URLs ([`oprobe_trace::util::is_secret_query_key`]: signatures, tokens,
/// credentials).
pub fn is_secret_query_key(key: &str) -> bool {
    is_secret_key(key) || oprobe_trace::util::is_secret_query_key(key)
}

/// A URL safe to store and to replay: its credentials removed ([`strip_url_credentials`]:
/// `user:password@`, signatures and tokens) and the query parameters named like a secret option
/// ([`is_secret_query_key`]) dropped; the other parameters stay, in order
/// (`s3://b/p?region=x&secret_access_key=y` → `s3://b/p?region=x`). Credentials are resolved
/// again (environment, profile) when the URL is used. Text without `://` is returned unchanged.
pub fn redact_url(raw: &str) -> String {
    let s = strip_url_credentials(raw);
    let Some(q) = s.find('?') else {
        return s;
    };
    if !s[..q].contains("://") {
        return s;
    }
    let (base, rest) = s.split_at(q);
    let rest = &rest[1..];
    let (query, fragment) = match rest.find('#') {
        Some(h) => rest.split_at(h),
        None => (rest, ""),
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let name = url::form_urlencoded::parse(p.as_bytes())
                .next()
                .map(|(k, _)| k.into_owned())
                .unwrap_or_default();
            !is_secret_query_key(&name)
        })
        .collect();
    let mut out = base.to_owned();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    out.push_str(fragment);
    out
}

/// A storage URL safe to print: credentials and the query string (SAS tokens, signatures)
/// removed.
pub fn display_url(raw: &str) -> String {
    let stripped = strip_url_credentials(raw);
    match stripped.find('?') {
        Some(q) => stripped[..q].to_owned(),
        None => stripped,
    }
}

/// Replace secret values with `"<redacted>"` and strip credentials embedded in URL values
/// (e.g. an `endpoint` of `http://user:pass@host`).
pub fn redact_options(
    opts: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    opts.iter()
        .map(|(k, v)| {
            if is_secret_key(k) {
                (k.clone(), "<redacted>".to_owned())
            } else {
                (k.clone(), strip_url_credentials(v))
            }
        })
        .collect()
}

/// Today's day number (days since 1970-01-01, UTC).
pub fn utc_today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        .div_euclid(86_400)
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

    #[test]
    fn secret_keys_cover_opendal_credentials_and_encryption_keys() {
        for k in [
            "secret_access_key",
            "access_key_id",
            "session_token",
            "security_token",
            "account_key",
            "sas_token",
            "password",
            "credential",
            "credential_path",
            "private_key",
            "application_key",
            "server_side_encryption_customer_key",
            "server_side_encryption_customer_key_md5",
            "server_side_encryption_aws_kms_key_id",
            "encryption_key",
            "master_key",
            "SSE_KEY",
            "key",
            "api_key",
        ] {
            assert!(is_secret_key(k), "{k} must be secret");
        }
        for k in [
            "bucket",
            "region",
            "endpoint",
            "root",
            "server_side_encryption",
            "server_side_encryption_customer_algorithm",
            "account_name",
            "container",
            "allow_anonymous",
            "enable_virtual_host_style",
        ] {
            assert!(!is_secret_key(k), "{k} is not secret");
        }
    }

    #[test]
    fn redacted_urls_keep_only_what_is_not_secret() {
        assert_eq!(
            redact_url("webdav://alice:hunter2@host/files"),
            "webdav://host/files"
        );
        assert_eq!(
            redact_url("s3://bucket/p?region=eu-west-1&secret_access_key=hunter2"),
            "s3://bucket/p?region=eu-west-1"
        );
        assert_eq!(
            redact_url("s3://bucket?secret_access_key=hunter2"),
            "s3://bucket"
        );
        assert_eq!(
            redact_url(
                "azblob://c/p?sv=2024&sig=hunter2&SessionToken=hunter2&X-Amz-Signature=hunter2&sas_token=hunter2&password=hunter2&endpoint=http://h#f"
            ),
            "azblob://c/p?sv=2024&endpoint=http://h#f"
        );
        // A presigned URL given as the operator: every provider's signature and token go.
        assert_eq!(
            redact_url(
                "https://h/k?X-Amz-Credential=AK&X-Amz-Security-Token=T&AWSAccessKeyId=AK&X-Goog-Signature=S&x-oss-security-token=T&q-signature=S&access_token=T&X-Amz-Expires=60"
            ),
            "https://h/k?X-Amz-Expires=60"
        );
        // Percent-encoded names are recognised.
        assert_eq!(
            redact_url("s3://b?secret%5Faccess%5Fkey=x&a=1"),
            "s3://b?a=1"
        );
        assert_eq!(redact_url("memory://"), "memory://");
        assert_eq!(redact_url("prod-bucket"), "prod-bucket");
        assert_eq!(redact_url("lab://demo?x=1"), "lab://demo?x=1");
    }

    #[test]
    fn url_credentials_are_stripped() {
        assert_eq!(
            strip_url_credentials("http://user:pa:ss@minio:9000/x?y=1"),
            "http://minio:9000/x?y=1"
        );
        assert_eq!(
            strip_url_credentials("https://host.example"),
            "https://host.example"
        );
        assert_eq!(
            strip_url_credentials("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(strip_url_credentials("no url here"), "no url here");
        assert_eq!(
            strip_url_credentials("a http://u@h b https://u:p@k/"),
            "a http://h b https://k/"
        );
        assert_eq!(
            display_url("webdav://u:p@host/dir?sig=abc"),
            "webdav://host/dir"
        );
        let mut m = std::collections::BTreeMap::new();
        m.insert("endpoint".to_owned(), "http://ak:sk@minio:9000".to_owned());
        assert_eq!(redact_options(&m)["endpoint"], "http://minio:9000");
    }
}

#[cfg(test)]
mod plural_tests {
    #[test]
    fn plural_counts() {
        assert_eq!(super::plural(1u64, "request"), "1 request");
        assert_eq!(super::plural(0u64, "request"), "0 requests");
        assert_eq!(super::plural(1105u64, "operation"), "1105 operations");
    }
}
