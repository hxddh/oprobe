//! Small helpers shared with `oprobe-core` (which re-exports them from its `util`).

/// Whether a URL query parameter named `name` carries a credential: a request signature, a
/// session or access token, or the identity of the key that signed (presigned S3, GCS, OSS and
/// COS URLs, an Azure SAS, a Swift temp URL, STS tokens, API keys). Case-insensitive.
///
/// The rule is by what the parameter is, not by provider: any `*signature*` (`X-Amz-Signature`,
/// `X-Goog-Signature`, `x-oss-signature`, `q-signature`, `Signature`), the SAS `sig` and any
/// `*_sig` (Swift's `temp_url_sig`); any `*token*` (`X-Amz-Security-Token`,
/// `x-oss-security-token`, `access_token`, `token`); any `*credential*` (`X-Amz-Credential`,
/// `X-Goog-Credential`); access key ids (`AWSAccessKeyId`, `OSSAccessKeyId`, `GoogleAccessId`,
/// `q-ak`); `upload_id` (the session URI of a GCS resumable upload is a bearer credential;
/// S3's `uploadId` is not, and stays); and the generic `*secret*`, `*password*`, `*session*`,
/// `*authorization*`, `*jwt*`, `*hmac*`, `key`, `*_key`, `apikey`.
///
/// What only describes the request or the grant stays readable: an SAS's `se` (expiry), `sp`
/// (permissions) and `sv`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders`, and the
/// cursors and key ranges of a listing, which name a position and grant nothing
/// (`continuation-token`, `pageToken`, `start_key`, `end_key`, `partition-key`, whatever their
/// case and their `-` or `_`); and switches ([`is_switch_key`]: `skip_signature=true` turns
/// signing off, it signs nothing). Any other name that looks like a secret's is one.
pub fn is_secret_query_key(name: &str) -> bool {
    if is_switch_key(name) {
        return false;
    }
    const CURSORS: [&str; 5] = [
        "continuationtoken",
        "pagetoken",
        "startkey",
        "endkey",
        "partitionkey",
    ];
    const NEEDLES: [&str; 14] = [
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
        "jwt",
        "hmac",
    ];
    let squashed = name
        .bytes()
        .filter(|b| !matches!(b, b'-' | b'_'))
        .map(|b| b.to_ascii_lowercase());
    if CURSORS.iter().any(|c| squashed.clone().eq(c.bytes())) {
        return false;
    }
    let k = name.as_bytes();
    let contains = |n: &str| {
        k.windows(n.len())
            .any(|w| w.eq_ignore_ascii_case(n.as_bytes()))
    };
    let ends_with = |n: &str| {
        k.len() >= n.len() && k[k.len() - n.len()..].eq_ignore_ascii_case(n.as_bytes())
    };
    NEEDLES.iter().any(|n| contains(n))
        || ["sig", "key", "q-ak", "upload_id"]
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n))
        || ["_key", "-key", "_sig", "-sig"].iter().any(|n| ends_with(n))
}

/// Whether the option or parameter `name` is a switch: `skip_…`, `disable_…`, `enable_…` or
/// `allow_…` (OpenDAL's `skip_signature`, `disable_config_load`, `allow_anonymous`), also
/// behind a path (`options.skip_signature`). A switch says whether something is used and is
/// never a credential, whatever else its name holds. 9.2 took `skip_signature` for a
/// signature: a run of a public bucket lost the option from its stored operator and its
/// replay command, which then signed with whatever credentials were around.
pub fn is_switch_key(name: &str) -> bool {
    const SWITCHES: [&str; 4] = ["skip", "disable", "enable", "allow"];
    let name = name.rsplit('.').next().unwrap_or(name).as_bytes();
    SWITCHES.iter().any(|s| {
        name.len() > s.len()
            && name[..s.len()].eq_ignore_ascii_case(s.as_bytes())
            && matches!(name[s.len()], b'_' | b'-')
    })
}

/// What stands for a removed value where the name around it stays.
const REDACTED: &str = "<redacted>";

/// Percent-encoded values are decoded and searched this many levels deep (a URL in a value
/// of a URL in a value); a value nested deeper is removed whole.
const MAX_NESTING: u8 = 3;

/// Where a URL embedded in prose ends: URLs are quoted, parenthesised (`for url (…)`) or
/// followed by a space.
fn ends_url(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"' | ')' | '<' | '>')
}

/// Remove the credentials `s` carries, wherever they are in it:
///
/// - the `user:password@` of every URL's authority, a password with `/`, `?` or `#` in it
///   included (an AWS secret key typed into a URL unescaped);
/// - the parameters that carry a signature or a token ([`is_secret_query_key`]) of every
///   query (`?a=1&X-Amz-Signature=…`), fragment (`#access_token=…`) and bare parameter list
///   (`sv=2024&sig=…`, an SAS token), with or without a URL around them. The other parameters
///   stay, in order (`https://u:p@h/k?partNumber=2&X-Amz-Signature=ab` →
///   `https://h/k?partNumber=2`);
/// - the same inside a percent-encoded value (`?next=https%3A%2F%2Fh%2Fk%3Fsig%3D…`);
/// - the value of a lone `name=value` with such a name (the `Signature=…` and `Credential=…`
///   of a quoted `Authorization` header, `password=…`), which becomes `name=<redacted>`. A
///   bare `key=…` outside a parameter list stays: there it names an object far more often
///   than it carries an API key;
/// - the value of an `Authorization` header (`Authorization: Bearer …`).
///
/// Text without such credentials is returned unchanged, byte for byte. A value that is an
/// environment reference (`${NAME}`), a documentation ellipsis (`…`) or already `<redacted>`
/// stays.
///
/// What it cannot know: a credential in none of these forms (a bare token in a path or in
/// prose, a JSON field), and a password before whose first `/` stands only a port number
/// (`user:8080/…@host` reads as `host:port/path@…`).
///
/// This is the one redaction every string passes before it is recorded, stored or printed:
/// request errors and transport errors (HTTP clients quote the full URL, and a presigned URL's
/// query is a bearer credential), operator URLs, proxies and endpoints.
pub fn strip_url_credentials(s: &str) -> String {
    redacted(s).unwrap_or_else(|| s.to_owned())
}

/// [`strip_url_credentials`] of `s`, or `None` when there is nothing to remove (nothing is
/// allocated then: the capture boundary calls this for every error text).
pub fn redacted(s: &str) -> Option<String> {
    strip(s, 0)
}

fn strip(s: &str, depth: u8) -> Option<String> {
    let mut out = None;
    let mut pass = |f: &dyn Fn(&str) -> Option<String>| {
        if let Some(changed) = f(out.as_deref().unwrap_or(s)) {
            out = Some(changed);
        }
    };
    pass(&strip_userinfo);
    pass(&strip_authorization);
    pass(&|text| strip_parameters(text, depth));
    out
}

/// Copies the kept parts of `src` into a string that is only allocated once something is
/// removed.
struct Rewrite<'a> {
    src: &'a str,
    out: Option<String>,
    /// `src[..copied]` is in `out` (or was replaced).
    copied: usize,
}

impl<'a> Rewrite<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            out: None,
            copied: 0,
        }
    }

    /// Keep `src` up to `until` and put `with` in place of `src[until..resume]`.
    fn replace(&mut self, until: usize, resume: usize, with: &str) {
        let out = self
            .out
            .get_or_insert_with(|| String::with_capacity(self.src.len()));
        out.push_str(&self.src[self.copied..until]);
        out.push_str(with);
        self.copied = resume;
    }

    fn finish(self) -> Option<String> {
        let mut out = self.out?;
        out.push_str(&self.src[self.copied..]);
        Some(out)
    }
}

/// Whether `authority` (what follows `://` up to the first `/`, `?` or `#`, with no `@` in
/// it) is the start of a `user:password` whose password goes on past that character: what
/// follows its last `:` is not a port.
fn password_runs_on(authority: &str) -> bool {
    if authority.starts_with('[') {
        return false;
    }
    let Some((user, tail)) = authority.rsplit_once(':') else {
        return false;
    };
    let port = !tail.is_empty()
        && tail.len() <= 5
        && tail.bytes().all(|b| b.is_ascii_digit())
        && tail.parse::<u32>().is_ok_and(|p| p <= 65_535);
    !user.is_empty() && !port
}

/// Remove the `user:password@` of every URL in `s`.
fn strip_userinfo(s: &str) -> Option<String> {
    if !s.contains('@') {
        return None;
    }
    let mut w = Rewrite::new(s);
    let mut from = 0;
    while let Some(i) = s[from..].find("://") {
        let start = from + i + 3;
        let url = &s[start..];
        let url = &url[..url.find(ends_url).unwrap_or(url.len())];
        let authority = &url[..url.find(['/', '?', '#']).unwrap_or(url.len())];
        let userinfo = match authority.rfind('@') {
            Some(at) => at + 1,
            // The password itself holds a `/`, `?` or `#`: it ends at the URL's last `@`.
            None if password_runs_on(authority) => url.rfind('@').map_or(0, |at| at + 1),
            None => 0,
        };
        if userinfo > 0 {
            w.replace(start, start + userinfo, "");
        }
        from = start + userinfo;
    }
    w.finish()
}

/// A character of a parameter or header name.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '%' | '~' | '+')
}

/// Whether `name` holds a percent-encoded `/` (`%2F`, `%2f`).
fn has_encoded_slash(name: &str) -> bool {
    name.as_bytes()
        .windows(3)
        .any(|w| w[0] == b'%' && w[1] == b'2' && w[2].eq_ignore_ascii_case(&b'f'))
}

/// Where the name that ends at `end` starts (not before `floor`).
fn name_start(s: &str, floor: usize, end: usize) -> usize {
    s[floor..end]
        .char_indices()
        .rev()
        .find(|(_, c)| !is_name_char(*c))
        .map_or(floor, |(at, c)| floor + at + c.len_utf8())
}

/// A value that is no secret whatever its name: nothing, a reference to the environment, the
/// ellipsis of a hint (`--set options.account_key=…`), or what an earlier redaction left.
fn is_placeholder(value: &str) -> bool {
    value.is_empty() || [REDACTED, "${", "…"].iter().any(|p| value.starts_with(p))
}

/// How many spaces `s` starts with.
fn spaces(s: &str) -> usize {
    s.len() - s.trim_start_matches(' ').len()
}

/// The quote `s` starts with, as written: `"`, `'`, or one escaped inside a JSON or Rust
/// string (`\"`, `\'`).
fn opening_quote(s: &str) -> Option<&str> {
    ["\\\"", "\\'", "\"", "'"].into_iter().find(|q| s.starts_with(q))
}

/// Replace the value of every `Authorization` header quoted in `s` (`Authorization: Bearer …`,
/// `"authorization": "AWS4-HMAC-SHA256 Credential=…"`, and the same escaped inside a JSON
/// string: `{\"Authorization\": \"Bearer …\"}`): up to the closing quote when the value or
/// the whole header is quoted, else to the end of the line.
fn strip_authorization(s: &str) -> Option<String> {
    const NAME: &[u8] = b"authorization";
    let mut w = Rewrite::new(s);
    let mut from = 0;
    while let Some(i) = s.as_bytes()[from..]
        .windows(NAME.len())
        .position(|w| w.eq_ignore_ascii_case(NAME))
    {
        let at = from + i;
        let mut rest = at + NAME.len();
        from = rest;
        let start = name_start(s, w.copied, at);
        let before = s[..start].chars().next_back();
        // A query parameter is the parameters' to remove, with its separator.
        if matches!(before, Some('?' | '&' | '#')) {
            continue;
        }
        // The quote before the name, escaped or not.
        let before_quote = ["\\\"", "\\'", "\"", "'"]
            .into_iter()
            .find(|q| s[..start].ends_with(q));
        let name_quote = opening_quote(&s[rest..]);
        rest += name_quote.map_or(0, str::len);
        rest += spaces(&s[rest..]);
        if !s[rest..].starts_with([':', '=']) {
            continue;
        }
        rest += 1;
        rest += spaces(&s[rest..]);
        let value_quote = opening_quote(&s[rest..]);
        rest += value_quote.map_or(0, str::len);
        let value = &s[rest..];
        // The quote that ends the value: its own, else the one around the whole header.
        let closing = value_quote.or(before_quote.filter(|_| name_quote.is_none()));
        let len = match closing {
            Some(q) => value.find(q),
            None => value.find(['\n', '\r']),
        }
        .unwrap_or(value.len());
        if !is_placeholder(value[..len].trim()) {
            w.replace(rest, rest + len, REDACTED);
        }
        from = rest + len;
    }
    w.finish()
}

/// `s` with its `%XX` escapes decoded; `None` when it has none.
fn percent_decoded(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let hex = |i: usize| b.get(i).and_then(|c| char::from(*c).to_digit(16));
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut decoded = false;
    while i < b.len() {
        match (b[i], hex(i + 1), hex(i + 2)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                decoded = true;
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    decoded.then(|| String::from_utf8_lossy(&out).into_owned())
}

/// Whether the parameter name `name` (as written: possibly percent-encoded) is a secret's.
fn is_secret_name(name: &str) -> bool {
    match percent_decoded(name) {
        Some(decoded) => is_secret_query_key(&decoded),
        None => is_secret_query_key(name),
    }
}

/// The value of a parameter that is not secret by its name, without the credentials of a
/// percent-encoded URL or parameter list in it; `None` when it carries none.
fn clean_value(value: &str, depth: u8) -> Option<String> {
    let decoded = percent_decoded(value)?;
    if depth >= MAX_NESTING {
        return Some(REDACTED.to_owned());
    }
    let clean = strip(&decoded, depth + 1)?;
    Some(url::form_urlencoded::byte_serialize(clean.as_bytes()).collect())
}

/// Remove the secret parameters of every parameter list in `s`, and the value of every lone
/// secret `name=value` (see [`strip_url_credentials`]).
fn strip_parameters(s: &str, depth: u8) -> Option<String> {
    let mut w = Rewrite::new(s);
    let mut from = 0;
    while let Some(i) = s[from..].find('=') {
        let eq = from + i;
        from = eq + 1;
        let start = name_start(s, w.copied, eq);
        if start == eq {
            continue;
        }
        let after = &s[eq + 1..];
        if !is_secret_name(&s[start..eq]) {
            // Its value may be a percent-encoded URL with credentials of its own.
            let len = after
                .find(|c: char| c == '&' || c == '#' || ends_url(c))
                .unwrap_or(after.len());
            if let Some(clean) = clean_value(&after[..len], depth) {
                w.replace(eq + 1, eq + 1 + len, &clean);
                from = eq + 1 + len;
            }
            continue;
        }
        // The parameter list the name is in: from its `?` or `#` (else the start of the
        // word) to the end of the word.
        let boundary = s[w.copied..start]
            .char_indices()
            .rev()
            .find(|(_, c)| matches!(c, '?' | '#') || ends_url(*c));
        let (lead, list_start) = match boundary {
            Some((at, c @ ('?' | '#'))) => (Some(c), w.copied + at + 1),
            Some((at, c)) => (None, w.copied + at + c.len_utf8()),
            None => (None, w.copied),
        };
        // A word may open with a bracket of its own (`(sig=…&se=…)`): not part of a name.
        let list_start = match lead {
            Some(_) => list_start,
            None => s[list_start..start]
                .find(|c: char| is_name_char(c) || c == '&')
                .map_or(start, |at| list_start + at),
        };
        let word_end = eq
            + 1
            + after
                .find(|c: char| c == '#' || ends_url(c))
                .unwrap_or(after.len());
        // Prose punctuation right after the list (`url: http://h/k?sig=x, called: …`,
        // `GET http://h/k?sig=x: refused`) is not part of its last value.
        let list = s[list_start..word_end].trim_end_matches([',', ';', '.', ':']);
        let list_end = (list_start + list.len()).max(eq + 1);
        // A `name=value` segment of a path (`s3://b/dt=2026-11-27/session_id=abc/part-0.parquet`,
        // a Hive partition) names a place, and its value runs on into the rest of the key: it
        // is no parameter. A parameter list starts at `?`, `#`, a space or a quote, never `/`.
        // An object path quoted as one segment encodes its slashes (GCS:
        // `/o/dt=2026-01-01%2Fsession_id=abc`): a "name" with `%2F` in it runs across a slash,
        // so it is a path too.
        if lead.is_none() && (s[..start].ends_with('/') || has_encoded_slash(&s[start..eq])) {
            continue;
        }
        if lead.is_none() && !list.contains('&') {
            // A lone `name=value` in prose: the name stays and the value goes (up to its
            // closing quote when it is quoted). `key=…` there is the key of an object
            // (`expected key=value`, `key=a/b.parquet not found`).
            if s[start..eq].eq_ignore_ascii_case("key") {
                continue;
            }
            let (value_start, value_end) = match after.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let inner = eq + 1 + q.len_utf8();
                    (inner, inner + s[inner..].find(q).unwrap_or(s.len() - inner))
                }
                _ => (eq + 1, list_end),
            };
            if !is_placeholder(&s[value_start..value_end]) {
                w.replace(value_start, value_end, REDACTED);
            }
            from = value_end;
            continue;
        }
        let kept: Vec<std::borrow::Cow<'_, str>> = list
            .split('&')
            .filter(|param| !is_secret_name(param.split_once('=').map_or(param, |(name, _)| name)))
            .map(|param| {
                let clean = param
                    .split_once('=')
                    .and_then(|(name, value)| Some((name, clean_value(value, depth)?)));
                match clean {
                    Some((name, value)) => format!("{name}={value}").into(),
                    None => param.into(),
                }
            })
            .collect();
        // When nothing public is left the list goes with its `?`.
        let lead_len = lead.map_or(0, char::len_utf8);
        let mut public = String::new();
        if !kept.is_empty() {
            public.extend(lead);
            public.push_str(&kept.join("&"));
        }
        w.replace(list_start - lead_len, list_end, &public);
        from = list_end;
    }
    w.finish()
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
    /// A Hive-partitioned key names a place: its `name=value` segments stay whole, whatever
    /// their names, while the query, prose and headers around it are still redacted.
    #[test]
    fn hive_partitions_in_a_path_are_not_parameters() {
        for path in [
            "s3://bucket/dt=2026-11-27/session_id=abc123/part-0.parquet",
            "s3://bucket/t/upload_id=u-17/token=x/part-1.parquet",
            "dt=2026-11-27/session_id=abc123/part-0.parquet not found",
            "read gs://b/warehouse/tbl/session_token=9/f.orc: NotFound",
            // A GCS object name is one path segment, its slashes encoded (9.3.4 redacted
            // `session_id`'s value and the query after it).
            "GET /b/o/dt=2026-01-01%2Fsession_id=abc HTTP/1.1",
            "https://storage.googleapis.com/storage/v1/b/b/o/dt=1%2fsession_id=abc?alt=media",
        ] {
            assert_eq!(strip_url_credentials(path), path);
        }
        assert_eq!(
            strip_url_credentials(
                "https://storage.googleapis.com/storage/v1/b/b/o/dt=1%2Fsession_id=abc?alt=media&access_token=T"
            ),
            "https://storage.googleapis.com/storage/v1/b/b/o/dt=1%2Fsession_id=abc?alt=media"
        );
        assert_eq!(
            strip_url_credentials(
                "https://h/b/session_id=abc/part-0.parquet?partNumber=2&X-Amz-Signature=ab"
            ),
            "https://h/b/session_id=abc/part-0.parquet?partNumber=2"
        );
        assert_eq!(
            strip_url_credentials("retry with session_id=abc123 failed"),
            "retry with session_id=<redacted> failed"
        );
        assert_eq!(
            strip_url_credentials("upload_id=ADPycd/2 expired"),
            "upload_id=<redacted> expired"
        );
    }

    /// An `Authorization` header quoted inside a JSON string (a logged request, an error
    /// that embeds its headers as JSON) loses its value like an unescaped one.
    #[test]
    fn a_json_escaped_authorization_header_is_redacted() {
        assert_eq!(
            strip_url_credentials(r#"{"headers":"{\"Authorization\": \"Bearer eyJhbGc.x.y\", \"Host\": \"h\"}"}"#),
            r#"{"headers":"{\"Authorization\": \"<redacted>\", \"Host\": \"h\"}"}"#
        );
        assert_eq!(
            strip_url_credentials(r#"headers: {\'authorization\': \'AWS4-HMAC-SHA256 Credential=AKIA/x\'}"#),
            r#"headers: {\'authorization\': \'<redacted>\'}"#
        );
        // The plain forms are unchanged.
        assert_eq!(
            strip_url_credentials(r#"{"Authorization": "Bearer abc", "x": 1}"#),
            r#"{"Authorization": "<redacted>", "x": 1}"#
        );
    }

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
            // A Swift temp URL, a GCS resumable session, and tokens by other names.
            "temp_url_sig",
            "st_sig",
            "x-sig",
            "upload_id",
            "UPLOAD_ID",
            "jwt",
            "id_jwt",
            "hmac",
            "X-Hmac",
            // A token or a key that is not one of the known cursors stays secret.
            "next_token",
            "page_access_token",
            "start_key_secret",
            "row_key",
            "skipped_signature",
            "allowed-token",
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
            "temp_url_expires",
            // The cursors and key ranges of a listing name a position: they grant nothing.
            "continuation-token",
            "ContinuationToken",
            "pageToken",
            "page_token",
            "start_key",
            "end_key",
            "partition-key",
            "PartitionKey",
            "marker",
            "key-marker",
            "upload-id-marker",
            "start-after",
            "max-keys",
            // Switches: whether something is used.
            "skip_signature",
            "options.skip_signature",
            "disable-token-refresh",
            "enable_session_cache",
            "Allow_Credentials",
        ] {
            assert!(!is_secret_query_key(k), "{k} is not secret");
        }
    }

    fn strips(from: &str, to: &str) {
        assert_eq!(strip_url_credentials(from), to);
        // What is left carries nothing more to remove.
        assert_eq!(redacted(to), None, "{to}");
    }

    /// Text nothing is removed from is not copied, and comes back byte for byte.
    #[test]
    fn text_without_credentials_is_returned_as_it_is() {
        for s in [
            "",
            "send http request: connection refused",
            "a = b, c=d; e@f",
            "https://h/b/user@example.com/k?list-type=2&continuation-token=1ueGcxLPRx1Tr&max-keys=1000",
            "http://127.0.0.1:9000/bucket/user@example.com",
            "https://storage.googleapis.com/storage/v1/b/b/o?pageToken=CgZh&prefix=a%2Fb",
            "GET /t?start_key=a%2F1&end_key=a%2F9&partition-key=7&marker=k",
            "s3://b/p?uploadId=VXBsb2FkIElE&partNumber=3",
            "the authorization header is malformed; the region 'us-east-1' is wrong",
            "options.secret_access_key=<redacted> options.token=${TOKEN}",
            // Hints and messages of oprobe itself.
            "send unsigned requests to a public bucket with --set options.skip_signature=true",
            "s3://bucket/p?skip_signature=true&region=us-east-1",
            "set them in the operator's options (`--set options.access_key_id=…`, or `[options]`)",
            "invalid condition `x` (expected key=value)",
            "object key=a/b.parquet not found",
            "Authorization: <redacted>",
            "http://[::1]:9000/a@b",
            "x%3Dy=%41%42",
        ] {
            assert_eq!(redacted(s), None, "{s}");
            assert_eq!(strip_url_credentials(s), s);
        }
    }

    /// A presigned query needs no `scheme://` in front of it to be a credential: a request
    /// line, a path, a bare SAS token.
    #[test]
    fn secret_parameters_are_removed_without_a_url_around_them() {
        strips(
            "GET /b/k?X-Amz-Expires=900&X-Amz-Credential=AKIA%2Fs3&X-Amz-Signature=SIG HTTP/1.1",
            "GET /b/k?X-Amz-Expires=900 HTTP/1.1",
        );
        strips("b.s3.amazonaws.com/k?X-Amz-Signature=SIG: refused", "b.s3.amazonaws.com/k: refused");
        strips("path `/c/k?sv=2024&sig=SIG&se=2026`", "path `/c/k?sv=2024&se=2026`");
        // A SAS token on its own, as an option's value or in a message.
        strips("sas sv=2024-11-04&ss=b&sig=SIG rejected", "sas sv=2024-11-04&ss=b rejected");
        strips("sig=SIG&sv=2024", "sv=2024");
        strips("(temp_url_sig=SIG&temp_url_expires=1)", "(temp_url_expires=1)");
    }

    /// An `Authorization` header quoted in a message: its signature and credential, or the
    /// whole value when it is the header line.
    #[test]
    fn a_quoted_authorization_header_loses_its_credentials() {
        strips(
            "sent 'AWS4-HMAC-SHA256 Credential=AKIA/20261126/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=SIG'",
            "sent 'AWS4-HMAC-SHA256 Credential=<redacted>, SignedHeaders=host;x-amz-date, Signature=<redacted>'",
        );
        strips(
            "header \"Authorization: AWS4-HMAC-SHA256 Credential=AKIA/x, SignedHeaders=host, Signature=SIG\" rejected",
            "header \"Authorization: <redacted>\" rejected",
        );
        strips(
            "headers: {\"authorization\": \"Bearer TOKEN\", \"host\": \"h\"}",
            "headers: {\"authorization\": \"<redacted>\", \"host\": \"h\"}",
        );
        strips("Proxy-Authorization: Basic dTpw\nHost: h", "Proxy-Authorization: <redacted>\nHost: h");
        strips("authorization=Bearer TOKEN", "authorization=<redacted>");
        // A lone secret `name=value`, quoted or not; the punctuation after it stays.
        strips("login failed (password=hunter2).", "login failed (password=<redacted>).");
        strips("dsn: user=u password='a b' host=h", "dsn: user=u password='<redacted>' host=h");
        strips("--set options.secret_access_key=SK, then", "--set options.secret_access_key=<redacted>, then");
    }

    /// An AWS secret key holds `/` and `+`; typed into a URL unescaped, the password does not
    /// end at the first `/`, `?` or `#`.
    #[test]
    fn a_password_with_url_delimiters_is_removed_whole() {
        strips("s3://AKIA:wJalr/XUtn+FEMI/K7MDENG@bucket/p", "s3://bucket/p");
        strips("https://u:p?a#ss/w@h:9000/k?x=1", "https://h:9000/k?x=1");
        strips("https://u:p#ss@h/k", "https://h/k");
        strips("invalid port in 'http://AK:a/b/c@minio:9000' given", "invalid port in 'http://minio:9000' given");
        // A password that starts with a `/`.
        strips("http://u:/x@h/k", "http://h/k");
        // A port, then a path with an `@`: no password.
        let same = |s: &str| assert_eq!(redacted(s), None, "{s}");
        same("http://minio:9000/b/a@b.example");
        same("http://h/k:x/y@z");
    }

    /// OAuth's implicit flow and some SDKs put the token in the fragment.
    #[test]
    fn secret_parameters_in_a_fragment_are_removed() {
        strips("https://app.example/cb#access_token=TOKEN&state=1&token_type=x", "https://app.example/cb#state=1");
        strips("https://h/k?a=1#access_token=TOKEN", "https://h/k?a=1");
        strips("https://h/k?sig=SIG&a=1#id_token=JWT&b=2", "https://h/k?a=1#b=2");
        strips("redirected to /cb#sig=SIG", "redirected to /cb");
    }

    /// A URL passed as a value is percent-encoded, credentials included.
    #[test]
    fn a_percent_encoded_url_in_a_value_loses_its_credentials() {
        strips(
            "https://h/r?next=https%3A%2F%2Fu%3Ap%40b.example%2Fk%3FX-Amz-Expires%3D60%26X-Amz-Signature%3DSIG&v=1",
            "https://h/r?next=https%3A%2F%2Fb.example%2Fk%3FX-Amz-Expires%3D60&v=1",
        );
        // Beside a secret parameter of the outer URL, and without one.
        strips(
            "https://h/r?sig=SIG&u=http%3A%2F%2Fh%2Fk%3Ftoken%3DTOK",
            "https://h/r?u=http%3A%2F%2Fh%2Fk",
        );
        strips("u=http%3A%2F%2Fa%3Ab%40h%2F", "u=http%3A%2F%2Fh%2F");
        // Encoded twice.
        strips(
            "https://h/r?a=b%3Dhttp%253A%252F%252Fh%252Fk%253Fsig%253DSIG",
            "https://h/r?a=b%3Dhttp%253A%252F%252Fh%252Fk",
        );
        // Nested deeper than anything real: the value goes whole.
        let mut deep = "sig=SIG".to_owned();
        for _ in 0..6 {
            deep = format!("u={}", url::form_urlencoded::byte_serialize(deep.as_bytes()).collect::<String>());
        }
        let shown = strip_url_credentials(&format!("https://h/r?{deep}"));
        assert!(!shown.contains("SIG"), "{shown}");
        // A long run of escapes costs one pass per level, not one per escape.
        let long = format!("https://h/r?a={}", "%25".repeat(20_000));
        assert_eq!(redacted(&long), None);
    }

    /// The names the 9.2 rule missed, each in a URL.
    #[test]
    fn swift_gcs_and_token_parameters_are_removed() {
        strips(
            "https://swift.example/v1/a/c/o?temp_url_sig=SIG&temp_url_expires=1700000000",
            "https://swift.example/v1/a/c/o?temp_url_expires=1700000000",
        );
        strips(
            "PUT https://storage.googleapis.com/upload/storage/v1/b/b/o?uploadType=resumable&upload_id=SESSION failed",
            "PUT https://storage.googleapis.com/upload/storage/v1/b/b/o?uploadType=resumable failed",
        );
        strips("https://h/k?jwt=JWT&x=1", "https://h/k?x=1");
        strips("https://h/k?x=1&hmac=MAC", "https://h/k?x=1");
        strips("https://h/k?x_sig=SIG", "https://h/k");
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
