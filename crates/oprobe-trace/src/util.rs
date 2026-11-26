//! Small helpers shared with `oprobe-core` (which re-exports them from its `util`).

/// Whether a URL query parameter named `name` carries a credential: a request signature, a
/// session or access token, or the identity of the key that signed (presigned S3, GCS, OSS and
/// COS URLs, an Azure SAS, STS tokens, API keys). Case-insensitive.
///
/// The rule is by what the parameter is, not by provider: any `*signature*` (`X-Amz-Signature`,
/// `X-Goog-Signature`, `x-oss-signature`, `q-signature`, `Signature`) and the SAS `sig`; any
/// `*token*` (`X-Amz-Security-Token`, `x-oss-security-token`, `access_token`, `token`); any
/// `*credential*` (`X-Amz-Credential`, `X-Goog-Credential`); access key ids (`AWSAccessKeyId`,
/// `OSSAccessKeyId`, `GoogleAccessId`, `q-ak`); and the generic `*secret*`, `*password*`,
/// `*session*`, `*authorization*`, `key`, `*_key`, `apikey`. What only describes the grant
/// stays readable: an SAS's `se` (expiry), `sp` (permissions) and `sv`, `X-Amz-Date`,
/// `X-Amz-Expires`, `X-Amz-SignedHeaders`.
pub fn is_secret_query_key(name: &str) -> bool {
    let k = name.to_ascii_lowercase();
    let k = k.as_str();
    const NEEDLES: [&str; 12] = [
        "signature",
        "token",
        "credential",
        "secret",
        "password",
        "passwd",
        "session",
        "authorization",
        "accesskey",
        "access_key",
        "accessid",
        "apikey",
    ];
    NEEDLES.iter().any(|n| k.contains(n))
        || matches!(k, "sig" | "key" | "q-ak")
        || k.ends_with("_key")
        || k.ends_with("-key")
}

/// Where a URL embedded in prose ends: URLs are quoted, parenthesised (`for url (…)`) or
/// followed by a space.
fn ends_url(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"' | ')' | '<' | '>')
}

/// Remove the credentials of every URL embedded in `s`: the `user:password@` of its authority
/// and the query parameters that carry a signature or a token ([`is_secret_query_key`]). The
/// other parameters stay, in order (`https://u:p@h/k?partNumber=2&X-Amz-Signature=ab` →
/// `https://h/k?partNumber=2`). Text without such credentials is returned unchanged, byte for
/// byte.
///
/// This is the one redaction every string passes before it is recorded, stored or printed:
/// request errors and transport errors (HTTP clients quote the full URL, and a presigned URL's
/// query is a bearer credential), operator URLs, proxies and endpoints.
pub fn strip_url_credentials(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let auth_end = tail
            .find(|c: char| matches!(c, '/' | '?' | '#') || ends_url(c))
            .unwrap_or(tail.len());
        let authority = &tail[..auth_end];
        match authority.rfind('@') {
            Some(at) => out.push_str(&authority[at + 1..]),
            None => out.push_str(authority),
        }
        rest = &tail[auth_end..];
        // The path, up to the query (a fragment or the end of the URL: nothing to remove).
        let path_end = rest
            .find(|c: char| matches!(c, '?' | '#') || ends_url(c))
            .unwrap_or(rest.len());
        out.push_str(&rest[..path_end]);
        rest = &rest[path_end..];
        if let Some(query) = rest.strip_prefix('?') {
            let query_end = query
                .find(|c: char| c == '#' || ends_url(c))
                .unwrap_or(query.len());
            push_public_query(&mut out, &query[..query_end]);
            rest = &query[query_end..];
        }
    }
    out.push_str(rest);
    out
}

/// Append `?query` without its secret parameters (nothing when none is left).
fn push_public_query(out: &mut String, query: &str) {
    // Prose punctuation right after the URL (`url: http://h/k?sig=x, called: …`,
    // `GET http://h/k?sig=x: refused`) is not part of the last value.
    let body = query.trim_end_matches([',', ';', '.', ':']);
    let (body, punctuation) = query.split_at(body.len());
    let secret = |param: &str| {
        let name = param.split_once('=').map_or(param, |(name, _)| name);
        url::form_urlencoded::parse(name.as_bytes())
            .next()
            .is_some_and(|(name, _)| is_secret_query_key(&name))
    };
    let mut sep = '?';
    for param in body.split('&') {
        if secret(param) {
            continue;
        }
        out.push(sep);
        sep = '&';
        // A value may itself be a URL (`endpoint=http://user:pass@host`).
        out.push_str(&strip_url_credentials(param));
    }
    out.push_str(punctuation);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The presigning and token parameters of every provider OpenDAL signs for are secret;
    /// the parameters that only describe a request or a grant are not.
    #[test]
    fn signature_and_token_parameters_are_secret_whatever_their_case() {
        for k in [
            "X-Amz-Signature",
            "x-amz-signature",
            "X-Amz-Credential",
            "X-Amz-Security-Token",
            "Signature",
            "AWSAccessKeyId",
            "sig",
            "SIG",
            "X-Goog-Signature",
            "X-Goog-Credential",
            "GoogleAccessId",
            "token",
            "access_token",
            "x-oss-signature",
            "x-oss-security-token",
            "x-oss-credential",
            "OSSAccessKeyId",
            "security-token",
            "q-signature",
            "q-ak",
            "x-cos-security-token",
            "sessionToken",
            "secret_access_key",
            "password",
            "api_key",
            "key",
        ] {
            assert!(is_secret_query_key(k), "{k} must be secret");
        }
        for k in [
            "se",
            "sp",
            "sv",
            "sr",
            "st",
            "spr",
            "X-Amz-Algorithm",
            "X-Amz-Date",
            "X-Amz-Expires",
            "X-Amz-SignedHeaders",
            "Expires",
            "partNumber",
            "uploadId",
            "prefix",
            "list-type",
            "comp",
            "restype",
            "versionId",
            "region",
            "q-sign-algorithm",
            "q-key-time",
        ] {
            assert!(!is_secret_query_key(k), "{k} is not secret");
        }
    }

    #[test]
    fn credentials_are_removed_from_every_url_in_a_text() {
        let same = |s: &str| assert_eq!(strip_url_credentials(s), s);
        same("no url here");
        same("https://host.example");
        same("http://127.0.0.1:8080");
        same("http://h/k?partNumber=2&uploadId=abc#frag");
        same("lab://demo?x=1&&y");
        same("a ? b :// c");
        assert_eq!(
            strip_url_credentials("http://user:pa:ss@minio:9000/x?y=1"),
            "http://minio:9000/x?y=1"
        );
        assert_eq!(
            strip_url_credentials("a http://u@h b https://u:p@k/"),
            "a http://h b https://k/"
        );
        // A presigned S3 URL as reqwest quotes it in an error.
        assert_eq!(
            strip_url_credentials(
                "error sending request for url (https://b.s3.amazonaws.com/k?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIA%2F20261126&X-Amz-Date=20261126T000000Z&X-Amz-Expires=900&X-Amz-Security-Token=TOK&X-Amz-Signature=deadbeef&X-Amz-SignedHeaders=host): connection refused"
            ),
            "error sending request for url (https://b.s3.amazonaws.com/k?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Date=20261126T000000Z&X-Amz-Expires=900&X-Amz-SignedHeaders=host): connection refused"
        );
        // An Azure SAS in an OpenDAL error context: the signature goes, the grant stays.
        assert_eq!(
            strip_url_credentials(
                "context: { url: https://a.blob.core.windows.net/c/k?sv=2024-11-04&se=2026-12-01&sp=r&sig=c2VjcmV0, called: http_util::Client::send }"
            ),
            "context: { url: https://a.blob.core.windows.net/c/k?sv=2024-11-04&se=2026-12-01&sp=r, called: http_util::Client::send }"
        );
        // Nothing public is left: the query goes with its `?`.
        assert_eq!(
            strip_url_credentials("GET 'https://h/k?Signature=x&AWSAccessKeyId=y' failed"),
            "GET 'https://h/k' failed"
        );
        // Percent-encoded names, a fragment, and a URL inside a value.
        assert_eq!(
            strip_url_credentials("s3://b/p?X%2DAmz%2DSignature=x&a=1#f"),
            "s3://b/p?a=1#f"
        );
        assert_eq!(
            strip_url_credentials("s3://b?endpoint=http://ak:sk@minio:9000&token=t"),
            "s3://b?endpoint=http://minio:9000"
        );
        assert_eq!(
            strip_url_credentials("read https://h/k?sv=2024&sig=x: connection reset."),
            "read https://h/k?sv=2024: connection reset."
        );
        // Several URLs in one message.
        assert_eq!(
            strip_url_credentials("from http://h/a?sig=1 to http://h/b?sig=2&x=3"),
            "from http://h/a to http://h/b?x=3"
        );
    }
}
