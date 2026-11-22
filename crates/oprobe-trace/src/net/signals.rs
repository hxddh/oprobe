//! Server signals of one response (design v5 §3, fixes M10): the provider's error code (from
//! the error body, capped at 4 KiB, or an error-code header), S3's `x-amz-id-2`, the bucket
//! region, `Server-Timing` and the CDN cache status. Shared by both transports.

use crate::rows::ReqDetail;

/// Error bodies are sniffed up to this many bytes.
pub const ERROR_BODY_CAP: usize = 4096;
const SERVER_TIMING_CAP: usize = 256;

fn header<'a>(h: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::trim)
}

fn capped(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// What the response headers say (`status` decides whether error-code headers count).
pub fn from_headers(h: &http::HeaderMap, status: u16) -> ReqDetail {
    let mut d = ReqDetail {
        host_id: header(h, "x-amz-id-2").map(|v| capped(v, 128)),
        region: header(h, "x-amz-bucket-region").map(|v| capped(v, 64)),
        server_timing: header(h, "server-timing").map(|v| capped(v, SERVER_TIMING_CAP)),
        cache_status: cache_status(h),
        ..Default::default()
    };
    if status >= 400 {
        d.error_code = [
            "x-ms-error-code",
            "x-amz-error-code",
            "x-oss-ec",
            "x-goog-error-code",
        ]
        .iter()
        .find_map(|n| header(h, n))
        .filter(|v| !v.is_empty())
        .map(|v| capped(v, 64));
    }
    d
}

/// `hit`, `miss`, `expired`, … from the usual CDN headers.
fn cache_status(h: &http::HeaderMap) -> Option<String> {
    for name in [
        "cf-cache-status",
        "x-cache-status",
        "x-cache",
        "cdn-cache-status",
    ] {
        if let Some(v) = header(h, name).filter(|v| !v.is_empty()) {
            let l = v.to_ascii_lowercase();
            // `x-cache: Hit from cloudfront`, `TCP_HIT`, `HIT, MISS` (shield, edge): the edge
            // (first) verdict.
            let first = l.split(',').next().unwrap_or(&l).trim();
            let word = if first.contains("hit") {
                "hit"
            } else if first.contains("miss") {
                "miss"
            } else if first.contains("expired") {
                "expired"
            } else if first.contains("bypass") || first.contains("pass") {
                "bypass"
            } else if first.contains("dynamic") {
                "dynamic"
            } else {
                return Some(capped(first, 32));
            };
            return Some(word.to_owned());
        }
    }
    None
}

/// The provider error code of an error body: S3 / GCS XML / Azure / OSS / COS
/// `<Code>…</Code>`, GCS JSON `error.errors[0].reason` (or `error.status`), and generic
/// `"code"` JSON.
pub fn error_code_of(body: &[u8]) -> Option<String> {
    let body = &body[..body.len().min(ERROR_BODY_CAP)];
    let text = String::from_utf8_lossy(body);
    let t = text.trim_start();
    if t.starts_with('<') {
        let start = t.find("<Code>")? + "<Code>".len();
        let end = start + t[start..].find("</Code>")?;
        let code = t[start..end].trim();
        return (!code.is_empty() && code.len() <= 64).then(|| code.to_owned());
    }
    if t.starts_with('{') {
        // Truncated bodies do not parse: look for the fields textually.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            let pick = [
                "/error/errors/0/reason",
                "/error/status",
                "/error/code",
                "/code",
                "/Code",
            ]
            .iter()
            .find_map(|p| match v.pointer(p)? {
                serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            });
            return pick.filter(|c| c.len() <= 64);
        }
        for key in ["\"reason\"", "\"code\""] {
            if let Some(i) = t.find(key) {
                let rest = t[i + key.len()..]
                    .trim_start()
                    .strip_prefix(':')?
                    .trim_start();
                let rest = rest.strip_prefix('"')?;
                let end = rest.find('"')?;
                let code = &rest[..end];
                return (!code.is_empty() && code.len() <= 64).then(|| code.to_owned());
            }
        }
    }
    None
}

/// Collects the first [`ERROR_BODY_CAP`] bytes of an error response body.
#[derive(Debug, Default)]
pub struct ErrorSniffer {
    buf: Vec<u8>,
}

impl ErrorSniffer {
    pub fn push(&mut self, chunk: &[u8]) {
        let room = ERROR_BODY_CAP.saturating_sub(self.buf.len());
        self.buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    pub fn code(&self) -> Option<String> {
        error_code_of(&self.buf)
    }
}

/// Merge the body's error code into `detail` (the header's code wins) and drop an empty one.
pub fn finish(
    detail: Option<Box<ReqDetail>>,
    sniff: Option<&ErrorSniffer>,
) -> Option<Box<ReqDetail>> {
    let mut d = detail.unwrap_or_default();
    if d.error_code.is_none()
        && let Some(s) = sniff
    {
        d.error_code = s.code();
    }
    (!d.is_empty()).then_some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_of_every_provider() {
        let s3 = br#"<?xml version="1.0" encoding="UTF-8"?>
<Error><Code>SlowDown</Code><Message>Please reduce your request rate.</Message><RequestId>X</RequestId></Error>"#;
        assert_eq!(error_code_of(s3).as_deref(), Some("SlowDown"));
        let azure = "\u{feff}<?xml version=\"1.0\"?><Error><Code>ServerBusy</Code></Error>";
        assert_eq!(
            error_code_of(azure.as_bytes()).as_deref(),
            Some("ServerBusy")
        );
        let gcs =
            br#"{"error":{"code":429,"message":"slow","errors":[{"reason":"rateLimitExceeded"}]}}"#;
        assert_eq!(error_code_of(gcs).as_deref(), Some("rateLimitExceeded"));
        let gcs2 = br#"{"error":{"code":503,"status":"UNAVAILABLE"}}"#;
        assert_eq!(error_code_of(gcs2).as_deref(), Some("UNAVAILABLE"));
        let truncated = br#"{"error":{"errors":[{"domain":"usageLimits","reason":"userRateLimitExceeded","mess"#;
        assert_eq!(
            error_code_of(truncated).as_deref(),
            Some("userRateLimitExceeded")
        );
        assert_eq!(error_code_of(b"oops"), None);
        assert_eq!(error_code_of(b"<html>500</html>"), None);
        let mut s = ErrorSniffer::default();
        s.push(&s3[..30]);
        s.push(&s3[30..]);
        assert_eq!(s.code().as_deref(), Some("SlowDown"));
        s.push(&vec![b' '; 10_000]);
        assert_eq!(s.buf.len(), ERROR_BODY_CAP);
    }

    #[test]
    fn headers_carry_host_id_region_timing_and_cache() {
        let mut h = http::HeaderMap::new();
        h.insert("x-amz-id-2", "abc+def=".parse().unwrap());
        h.insert("x-amz-bucket-region", "eu-west-1".parse().unwrap());
        h.insert("server-timing", "db;dur=53, app;dur=47.2".parse().unwrap());
        h.insert("x-cache", "Hit from cloudfront".parse().unwrap());
        h.insert("x-ms-error-code", "ServerBusy".parse().unwrap());
        let d = from_headers(&h, 200);
        assert_eq!(d.host_id.as_deref(), Some("abc+def="));
        assert_eq!(d.region.as_deref(), Some("eu-west-1"));
        assert_eq!(d.server_timing.as_deref(), Some("db;dur=53, app;dur=47.2"));
        assert_eq!(d.cache_status.as_deref(), Some("hit"));
        assert_eq!(d.error_code, None);
        assert_eq!(
            from_headers(&h, 503).error_code.as_deref(),
            Some("ServerBusy")
        );
        let mut h = http::HeaderMap::new();
        h.insert("cf-cache-status", "MISS".parse().unwrap());
        assert_eq!(from_headers(&h, 200).cache_status.as_deref(), Some("miss"));
        assert!(finish(Some(Box::default()), None).is_none());
    }
}
