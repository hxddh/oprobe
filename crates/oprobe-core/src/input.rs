//! Typed parsers for every kind of user input (design v4 §8).
//!
//! One parser per input kind — durations, sizes, rates and shares, URLs and schemes, presets,
//! access patterns, pricing providers, enums, run references, `KEY=VALUE` pairs. Each returns
//! an [`OprobeError`] with a `usage.*` code and, where a near miss is likely, a "did you mean"
//! hint. The CLI (clap `value_parser`s in [`clap`]), the config and profile loaders and the MCP
//! server all call these, so one bad value gets the same code and message on every surface.
//!
//! Values that come from a file (a profile, `config.toml`) are re-coded `config.invalid` by
//! their loader; the message stays the same.

use std::time::Duration;

use crate::config::{Access, Preset};
use crate::errors::{ErrorCode, OprobeError};
use crate::net::HttpVersion;
use crate::util::ByteSize;

/// A typed input error: `usage.invalid_argument`.
pub fn invalid(msg: impl Into<String>) -> OprobeError {
    OprobeError::new(ErrorCode::UsageInvalidArgument, msg)
}

// ---------------------------------------------------------------------------------------
// "Did you mean"
// ---------------------------------------------------------------------------------------

/// Levenshtein distance (small strings only).
pub fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The candidate closest to `input`, when it is plausibly what was meant (case-insensitive; a
/// candidate containing the input, or contained in it, counts as close).
pub fn suggest<'a>(input: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let key = input.trim().to_ascii_lowercase();
    if key.is_empty() {
        return None;
    }
    candidates
        .into_iter()
        .map(|k| {
            let kl = k.to_ascii_lowercase();
            let d = if kl.contains(key.as_str()) || key.contains(kl.as_str()) {
                kl.len().abs_diff(key.len()).min(2)
            } else {
                distance(&key, &kl)
            };
            (d, k)
        })
        .filter(|(d, k)| *d <= (k.len() / 3).max(2))
        .min()
        .map(|(_, k)| k)
}

/// `did you mean `x`? (choose: a, b, c)` or just the choices.
pub fn choices_hint(input: &str, candidates: &[&str]) -> String {
    let all = candidates.join(", ");
    match suggest(input, candidates.iter().copied()) {
        Some(s) => format!("did you mean `{s}`? (choose: {all})"),
        None => format!("choose: {all}"),
    }
}

/// Pick one of `choices` (`(name, value)`, aliases allowed) for an input of kind `what`.
pub fn choice<T: Copy>(what: &str, raw: &str, choices: &[(&str, T)]) -> Result<T, OprobeError> {
    let s = raw.trim().to_ascii_lowercase().replace('_', "-");
    if let Some((_, v)) = choices.iter().find(|(n, _)| *n == s) {
        return Ok(*v);
    }
    let mut names: Vec<&str> = choices.iter().map(|(n, _)| *n).collect();
    names.dedup();
    Err(invalid(format!("unknown {what} `{}`", raw.trim())).with_hint(choices_hint(raw, &names)))
}

// ---------------------------------------------------------------------------------------
// Numbers and quantities
// ---------------------------------------------------------------------------------------

/// A duration: `10s`, `500ms`, `1.5m`, `2h`, `250us` or plain seconds (`5`). Never negative.
pub fn duration(raw: &str) -> Result<Duration, OprobeError> {
    let s = raw.trim().to_ascii_lowercase();
    let examples = "examples: 10s, 500ms, 2m";
    if s.is_empty() {
        return Err(invalid("duration cannot be empty").with_hint(examples));
    }
    if s.starts_with('-') {
        return Err(invalid(format!("duration `{}` is negative", raw.trim()))
            .with_hint(format!("pass a non-negative duration ({examples})")));
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
    } else if let Some(v) = s.strip_suffix('d') {
        (v, 86_400.0)
    } else {
        (s.as_str(), 1.0)
    };
    let val: f64 = num.trim().parse().map_err(|_| {
        let unit = s.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        let hint = match suggest(unit, ["ms", "s", "m", "h", "us", "d"]) {
            Some(u) if !unit.is_empty() => format!("did you mean `{}{u}`? ({examples})", {
                let n = &s[..s.len() - unit.len()];
                if n.is_empty() { "10" } else { n }
            }),
            _ => examples.to_owned(),
        };
        invalid(format!("invalid duration `{}`", raw.trim())).with_hint(hint)
    })?;
    if !val.is_finite() || val < 0.0 {
        return Err(invalid(format!(
            "invalid duration `{}`: must be a non-negative number",
            raw.trim()
        ))
        .with_hint(examples));
    }
    let secs = val * mult;
    if secs > 10.0 * 365.0 * 86_400.0 {
        return Err(invalid(format!("duration `{}` is too long", raw.trim())).with_hint(examples));
    }
    Ok(Duration::from_secs_f64(secs))
}

/// A duration greater than zero.
pub fn positive_duration(raw: &str) -> Result<Duration, OprobeError> {
    let d = duration(raw)?;
    if d.is_zero() {
        return Err(invalid(format!(
            "duration `{}` must be greater than zero",
            raw.trim()
        ))
        .with_hint("examples: 10s, 500ms, 2m"));
    }
    Ok(d)
}

/// A byte size: `4MiB`, `512KiB`, `1GB`, `65536`.
pub fn size(raw: &str) -> Result<ByteSize, OprobeError> {
    let s = raw.trim();
    let examples = "examples: 4MiB, 512KiB, 1GB, 65536";
    if s.is_empty() {
        return Err(invalid("size cannot be empty").with_hint(examples));
    }
    if s.starts_with('-') {
        return Err(invalid(format!("size `{s}` is negative")).with_hint(examples));
    }
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num
        .replace('_', "")
        .parse()
        .map_err(|_| invalid(format!("invalid size `{s}`")).with_hint(examples))?;
    const UNITS: [(&str, f64); 14] = [
        ("", 1.0),
        ("b", 1.0),
        ("k", 1e3),
        ("kb", 1e3),
        ("ki", 1024.0),
        ("kib", 1024.0),
        ("m", 1e6),
        ("mb", 1e6),
        ("mi", 1048576.0),
        ("mib", 1048576.0),
        ("g", 1e9),
        ("gb", 1e9),
        ("gi", 1073741824.0),
        ("gib", 1073741824.0),
    ];
    let u = unit.trim().to_ascii_lowercase();
    let mult = match u.as_str() {
        "t" | "tb" => 1e12,
        "ti" | "tib" => 1024.0_f64.powi(4),
        _ => match UNITS.iter().find(|(n, _)| *n == u) {
            Some((_, m)) => *m,
            None => {
                let names = ["B", "KiB", "MiB", "GiB", "TiB", "KB", "MB", "GB", "TB"];
                return Err(
                    invalid(format!("unknown size unit `{}` in `{s}`", unit.trim()))
                        .with_hint(choices_hint(unit.trim(), &names)),
                );
            }
        },
    };
    if !num.is_finite() || num < 0.0 {
        return Err(invalid(format!("size `{s}` must be non-negative")).with_hint(examples));
    }
    Ok(ByteSize((num * mult).round() as u64))
}

/// A size greater than zero.
pub fn nonzero_size(raw: &str) -> Result<ByteSize, OprobeError> {
    let v = size(raw)?;
    if v.0 == 0 {
        return Err(
            invalid(format!("size `{}` must be greater than zero", raw.trim()))
                .with_hint("examples: 64KiB, 4MiB"),
        );
    }
    Ok(v)
}

fn number(what: &str, raw: &str) -> Result<f64, OprobeError> {
    let s = raw.trim();
    let v: f64 = s
        .parse()
        .map_err(|_| invalid(format!("invalid {what} `{s}`: not a number")))?;
    if !v.is_finite() {
        return Err(invalid(format!(
            "invalid {what} `{s}`: not a finite number"
        )));
    }
    Ok(v)
}

/// A finite number `>= 0` (money, volumes) for an input named `what`.
pub fn non_negative(what: &str, raw: &str) -> Result<f64, OprobeError> {
    let v = number(what, raw)?;
    if v < 0.0 {
        return Err(invalid(format!("{what} must be >= 0 (got {})", raw.trim())));
    }
    Ok(v)
}

/// A finite number `> 0` (an arrival rate in ops/s, a latency limit) for an input named `what`.
pub fn positive(what: &str, raw: &str) -> Result<f64, OprobeError> {
    let v = number(what, raw)?;
    if v <= 0.0 {
        return Err(invalid(format!("{what} must be > 0 (got {})", raw.trim())));
    }
    Ok(v)
}

/// A whole number of at least `min` for an input named `what` (rejected, never clamped).
pub fn count_at_least(what: &str, raw: &str, min: u64) -> Result<u64, OprobeError> {
    let s = raw.trim();
    let n: u64 = s
        .parse()
        .map_err(|_| invalid(format!("invalid {what} `{s}`: not a whole number")))?;
    if n < min {
        return Err(invalid(format!("{what} must be at least {min} (got {n})")));
    }
    Ok(n)
}

/// A share (`1%`, `0.5 %`, or a fraction `0.01`), within 0..1.
pub fn share(what: &str, raw: &str) -> Result<f64, OprobeError> {
    let t = raw.trim();
    let r = match t.strip_suffix('%') {
        Some(p) => p.trim().parse::<f64>().map(|x| x / 100.0),
        None => t.parse::<f64>(),
    }
    .map_err(|_| invalid(format!("invalid {what} `{t}`")).with_hint("examples: 1%, 0.01"))?;
    if !(0.0..=1.0).contains(&r) {
        return Err(
            invalid(format!("{what} must be between 0% and 100% (got {t})"))
                .with_hint("examples: 1%, 0.01"),
        );
    }
    Ok(r)
}

/// A boolean: `true|false`, `on|off`, `yes|no`, `1|0`.
pub fn boolean(what: &str, raw: &str) -> Result<bool, OprobeError> {
    choice(
        what,
        raw,
        &[
            ("true", true),
            ("on", true),
            ("yes", true),
            ("1", true),
            ("false", false),
            ("off", false),
            ("no", false),
            ("0", false),
        ],
    )
}

// ---------------------------------------------------------------------------------------
// Domain values
// ---------------------------------------------------------------------------------------

/// A workload preset (aliases: `lakehouse`, `small`, `meta`, `ckpt`, …).
pub fn preset(raw: &str) -> Result<Preset, OprobeError> {
    let mut table: Vec<(&str, Preset)> = Preset::ALL.iter().map(|p| (p.slug(), *p)).collect();
    // The 5.x names map to the preset that keeps their access pattern.
    table.extend([
        ("small", Preset::SmallFiles),
        ("large", Preset::LargeObjects),
        ("checkpoint", Preset::LargeObjects),
        ("parquet", Preset::RangeReads),
        ("parquet-suffix", Preset::RangeReads),
        ("ranges", Preset::RangeReads),
        ("metadata", Preset::ListStat),
        ("metadata-heavy", Preset::ListStat),
        ("list-paged", Preset::ListStat),
        ("mixed", Preset::SmallFiles),
    ]);
    let s = raw.trim().to_ascii_lowercase().replace('_', "-");
    if let Some((_, p)) = table.iter().find(|(n, _)| *n == s) {
        return Ok(*p);
    }
    let names: Vec<&str> = Preset::ALL.iter().map(|p| p.slug()).collect();
    Err(invalid(format!("unknown preset `{}`", raw.trim())).with_hint(choices_hint(raw, &names)))
}

/// A key popularity pattern: `uniform`, `zipf[:S]`, `hotset[:F/P]`.
pub fn access(raw: &str) -> Result<Access, OprobeError> {
    let s = raw.trim().to_ascii_lowercase();
    let hint = "uniform | zipf:1.1 | hotset:0.2/0.8";
    if s == "uniform" {
        return Ok(Access::Uniform);
    }
    if let Some(v) = s.strip_prefix("zipf") {
        let v = v.trim_start_matches([':', '=']);
        let exp = if v.is_empty() {
            1.1
        } else {
            v.parse::<f64>().map_err(|_| {
                invalid(format!("invalid zipf exponent in `{}`", raw.trim())).with_hint(hint)
            })?
        };
        if !(exp.is_finite() && exp > 0.0) {
            return Err(
                invalid(format!("zipf exponent must be > 0 (got `{}`)", raw.trim()))
                    .with_hint(hint),
            );
        }
        return Ok(Access::Zipf(exp));
    }
    if let Some(v) = s.strip_prefix("hotset") {
        let v = v.trim_start_matches([':', '=']);
        let (f, p) = if v.is_empty() {
            ("0.2", "0.8")
        } else {
            v.split_once('/').ok_or_else(|| {
                invalid(format!(
                    "invalid hotset `{}`: expected FRACTION/PROBABILITY",
                    raw.trim()
                ))
                .with_hint(hint)
            })?
        };
        let parse = |x: &str| {
            x.trim()
                .parse::<f64>()
                .map_err(|_| invalid(format!("invalid hotset `{}`", raw.trim())).with_hint(hint))
        };
        let (fraction, probability) = (parse(f)?, parse(p)?);
        if !((0.0..=1.0).contains(&fraction) && (0.0..=1.0).contains(&probability)) {
            return Err(invalid(format!(
                "hotset fractions must be within 0..1 (got `{}`)",
                raw.trim()
            ))
            .with_hint(hint));
        }
        return Ok(Access::Hotset {
            fraction,
            probability,
        });
    }
    Err(
        invalid(format!("unknown access pattern `{}`", raw.trim())).with_hint(
            match suggest(&s, ["uniform", "zipf", "hotset"]) {
                Some(x) => format!("did you mean `{x}`? ({hint})"),
                None => hint.to_owned(),
            },
        ),
    )
}

/// HTTP version: `auto`, `h1` (`http1`, `http/1.1`), `h2` (`http2`).
pub fn http_version(raw: &str) -> Result<HttpVersion, OprobeError> {
    let s = raw.trim().to_ascii_lowercase();
    match s.as_str() {
        "auto" => Ok(HttpVersion::Auto),
        "h1" | "http1" | "http/1.1" | "http1.1" | "1.1" => Ok(HttpVersion::H1),
        "h2" | "http2" | "http/2" | "2" => Ok(HttpVersion::H2),
        _ => Err(invalid(format!("invalid HTTP version `{}`", raw.trim()))
            .with_hint(choices_hint(&s, &["auto", "h1", "h2"]))),
    }
}

/// A `lab://` scenario name.
pub fn lab_scenario(raw: &str) -> Result<crate::lab::Scenario, OprobeError> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Ok(crate::lab::Scenario::Clean);
    }
    let names: Vec<&str> = crate::lab::Scenario::ALL.iter().map(|x| x.name()).collect();
    crate::lab::Scenario::ALL
        .into_iter()
        .find(|x| x.name() == s)
        .ok_or_else(|| {
            invalid(format!("unknown lab scenario `{}`", raw.trim()))
                .with_hint(choices_hint(&s, &names))
        })
}

/// URL schemes oprobe resolves, with the OpenDAL service each maps to.
pub const SCHEMES: &[(&str, &str)] = &[
    ("s3", "s3"),
    ("s3a", "s3"),
    ("r2", "s3"),
    ("minio", "s3"),
    ("gs", "gcs"),
    ("gcs", "gcs"),
    ("azblob", "azblob"),
    ("az", "azblob"),
    ("azure", "azblob"),
    ("azdls", "azdls"),
    ("azfile", "azfile"),
    ("oss", "oss"),
    ("cos", "cos"),
    ("obs", "obs"),
    ("tos", "tos"),
    ("b2", "b2"),
    ("swift", "swift"),
    ("webdav", "webdav"),
    ("webdavs", "webdav"),
    ("webhdfs", "webhdfs"),
    ("upyun", "upyun"),
    ("http", "http"),
    ("https", "http"),
    ("fs", "fs"),
    ("file", "fs"),
    ("memory", "memory"),
    ("mem", "memory"),
    ("lab", "lab"),
];

/// The OpenDAL service of a URL scheme (`s3a` → `s3`, `gs` → `gcs`, …).
pub fn scheme(raw: &str) -> Result<&'static str, OprobeError> {
    let s = raw.trim().to_ascii_lowercase();
    if let Some((_, svc)) = SCHEMES.iter().find(|(n, _)| *n == s) {
        return Ok(svc);
    }
    let names: Vec<&str> = SCHEMES.iter().map(|(n, _)| *n).collect();
    Err(
        invalid(format!("unsupported storage scheme `{}`", raw.trim())).with_hint(format!(
            "{} (examples: s3://bucket/prefix, gcs://bucket, fs:///tmp/bench, memory://, lab://demo)",
            choices_hint(&s, &names)
        )),
    )
}

/// The examples every URL error points at.
pub const URL_EXAMPLES: &str = "examples: s3://bucket/prefix, gcs://bucket, azblob://container, fs:///tmp/bench, memory://, lab://demo";

/// Syntax of a storage URL (`SCHEME://…`): the scheme must be known and bucket-style schemes
/// need a bucket (or container). Returns the OpenDAL service. Resolution of the URL into a
/// target (`TargetSpec::from_url`) raises the same errors.
pub fn storage_url(raw: &str) -> Result<&'static str, OprobeError> {
    let s = raw.trim();
    let Some((sch, rest)) = s.split_once("://") else {
        return Err(
            invalid(format!("invalid storage URL `{s}`: no `SCHEME://`")).with_hint(URL_EXAMPLES),
        );
    };
    let svc = scheme(sch)?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if let Some(what) = bucket_kind(svc)
        && host.is_empty()
    {
        return Err(missing_bucket(&sch.to_ascii_lowercase(), what));
    }
    Ok(svc)
}

/// `bucket` / `container` for services addressed by one, `None` otherwise.
pub fn bucket_kind(service: &str) -> Option<&'static str> {
    match service {
        "s3" | "gcs" | "oss" | "cos" | "b2" | "obs" | "tos" => Some("bucket"),
        "azblob" => Some("container"),
        _ => None,
    }
}

/// The error of a bucket-style URL without its bucket.
pub fn missing_bucket(scheme: &str, what: &str) -> OprobeError {
    invalid(format!("`{scheme}://` URL needs a {what}"))
        .with_hint(format!("put the {what} first: {scheme}://my-{what}/prefix"))
}

/// A bucket (or container) name: 3–63 characters of lowercase letters, digits, `-`, `.` and
/// `_`, starting and ending with a letter or digit.
pub fn bucket(raw: &str) -> Result<String, OprobeError> {
    let s = raw.trim();
    let ok_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "-._".contains(c);
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    if (3..=63).contains(&s.len())
        && s.chars().all(ok_char)
        && edge(s.chars().next())
        && edge(s.chars().last())
    {
        return Ok(s.to_owned());
    }
    let mut hint = "3–63 lowercase letters, digits, `-`, `.` or `_`".to_owned();
    if s.chars().any(|c| c.is_ascii_uppercase()) {
        hint = format!("did you mean `{}`? ({hint})", s.to_ascii_lowercase());
    }
    Err(invalid(format!("invalid bucket name `{s}`")).with_hint(hint))
}

/// An HTTP(S) endpoint URL with a host (`--endpoint`).
pub fn endpoint(raw: &str) -> Result<String, OprobeError> {
    let s = raw.trim();
    let hint = "examples: https://s3.eu-west-1.amazonaws.com, http://127.0.0.1:9000";
    let u = url::Url::parse(s)
        .map_err(|_| invalid(format!("invalid endpoint `{s}`")).with_hint(hint))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(invalid(format!(
            "invalid endpoint `{s}`: the scheme must be http or https, not `{}`",
            u.scheme()
        ))
        .with_hint(hint));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(invalid(format!("invalid endpoint `{s}`: no host")).with_hint(hint));
    }
    Ok(s.to_owned())
}

/// `KEY=VALUE` (an `--option` or a `--set`).
pub fn key_value(what: &str, raw: &str) -> Result<(String, String), OprobeError> {
    match raw.split_once('=') {
        Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_owned(), v.trim().to_owned())),
        _ => Err(
            invalid(format!("invalid {what} `{raw}`: expected KEY=VALUE"))
                .with_hint(format!("e.g. {what} concurrency=8")),
        ),
    }
}

/// A RUNREF's syntax: an id, an id prefix, `last`, `last~N`, a label or a path. Only the
/// `last~N` form can be malformed; resolution (`store::resolve_run`) decides whether it exists.
pub fn run_ref(raw: &str) -> Result<String, OprobeError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(invalid("empty run reference")
            .with_hint("pass a run id, `last`, `last~N`, a label or a .oprobe path"));
    }
    if let Some(n) = s.strip_prefix("last~")
        && n.parse::<usize>().is_err()
    {
        return Err(invalid(format!("invalid run reference `{s}`"))
            .with_hint("`last~N` takes a whole number, e.g. last~1"));
    }
    Ok(s.to_owned())
}

/// clap `value_parser`s: validate with the typed parsers above and keep the text as typed, so
/// a bad flag is rejected before anything runs with the same code and hint as everywhere else
/// (`cli::parse_cli` reads the [`OprobeError`] back out of clap's error).
pub mod clap {
    use super::*;

    pub fn duration(s: &str) -> Result<String, OprobeError> {
        super::duration(s).map(|_| s.trim().to_owned())
    }
    pub fn positive_duration(s: &str) -> Result<String, OprobeError> {
        super::positive_duration(s).map(|_| s.trim().to_owned())
    }
    pub fn size(s: &str) -> Result<String, OprobeError> {
        super::size(s).map(|_| s.trim().to_owned())
    }
    pub fn nonzero_size(s: &str) -> Result<String, OprobeError> {
        super::nonzero_size(s).map(|_| s.trim().to_owned())
    }
    /// A size, or a size distribution (`dist:lognormal(256KiB,1.5)`, `file:PATH`).
    pub fn object_size(s: &str) -> Result<String, OprobeError> {
        if crate::config::SizeDist::is_dist(s) {
            s.parse::<crate::config::SizeDist>()
                .map(|_| s.trim().to_owned())
        } else {
            nonzero_size(s)
        }
    }
    pub fn share(s: &str) -> Result<String, OprobeError> {
        super::share("share", s).map(|_| s.trim().to_owned())
    }
    pub fn preset(s: &str) -> Result<String, OprobeError> {
        super::preset(s).map(|_| s.trim().to_owned())
    }
    pub fn access(s: &str) -> Result<String, OprobeError> {
        super::access(s).map(|_| s.trim().to_owned())
    }
    pub fn endpoint(s: &str) -> Result<String, OprobeError> {
        super::endpoint(s)
    }
    pub fn storage_url(s: &str) -> Result<String, OprobeError> {
        super::storage_url(s).map(|_| s.trim().to_owned())
    }
    pub fn lab_scenario(s: &str) -> Result<String, OprobeError> {
        super::lab_scenario(s).map(|_| s.trim().to_owned())
    }
    pub fn run_ref(s: &str) -> Result<String, OprobeError> {
        super::run_ref(s)
    }
    pub fn key_value(s: &str) -> Result<String, OprobeError> {
        super::key_value("PATH=VALUE", s).map(|_| s.to_owned())
    }
    pub fn set(s: &str) -> Result<String, OprobeError> {
        let (k, v) = super::key_value("--set", s)?;
        crate::model::Overrides::new().set(&k, &v)?;
        Ok(s.to_owned())
    }
    /// Money, volumes, fractions given as numbers: finite and `>= 0`.
    pub fn non_negative(s: &str) -> Result<f64, OprobeError> {
        super::non_negative("value", s)
    }
    /// Arrival rates, latency limits: finite and `> 0`.
    pub fn positive(s: &str) -> Result<f64, OprobeError> {
        super::positive("value", s)
    }
    /// A fraction within 0..1 given as a number.
    pub fn fraction(s: &str) -> Result<f64, OprobeError> {
        let v = super::non_negative("fraction", s)?;
        if v > 1.0 {
            return Err(invalid(format!("fraction must be within 0..1 (got {s})")));
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code<T: std::fmt::Debug>(r: Result<T, OprobeError>) -> (ErrorCode, String) {
        let e = r.unwrap_err();
        (e.code, e.hint.unwrap_or_default())
    }

    #[test]
    fn durations() {
        assert_eq!(duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(duration("1.5m").unwrap(), Duration::from_secs(90));
        assert_eq!(duration("5").unwrap(), Duration::from_secs(5));
        assert_eq!(duration("2d").unwrap(), Duration::from_secs(172_800));
        let (c, h) = code(duration("5x"));
        assert_eq!(c, ErrorCode::UsageInvalidArgument);
        assert!(h.contains("10s"), "{h}");
        let (_, h) = code(duration("5sec"));
        assert!(h.contains("did you mean `5s`"), "{h}");
        assert!(duration("-3s").unwrap_err().message.contains("negative"));
        assert!(duration("").is_err());
        assert!(duration("inf").is_err());
        assert!(positive_duration("0s").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(size("4MiB").unwrap(), ByteSize::mib(4));
        assert_eq!(size("1GB").unwrap(), ByteSize(1_000_000_000));
        assert_eq!(size("65_536").unwrap(), ByteSize(65536));
        let (_, h) = code(size("4MiBs"));
        assert!(h.contains("MiB"), "{h}");
        assert!(size("-1KiB").is_err());
        assert!(nonzero_size("0").is_err());
        assert!(nonzero_size("1").is_ok());
    }

    #[test]
    fn shares_and_numbers() {
        assert_eq!(share("x", "1%").unwrap(), 0.01);
        assert_eq!(share("x", "0.5").unwrap(), 0.5);
        assert!(share("x", "150%").is_err());
        assert!(non_negative("x", "-1").is_err());
        assert!(positive("x", "0").is_err());
        assert!(non_negative("x", "NaN").is_err());
        assert_eq!(count_at_least("x", "3", 1).unwrap(), 3);
        assert!(count_at_least("x", "0", 1).is_err());
        assert!(boolean("x", "on").unwrap());
        assert!(!boolean("x", "No").unwrap());
    }

    #[test]
    fn presets_access_and_providers() {
        assert_eq!(preset("small_files").unwrap(), Preset::SmallFiles);
        assert_eq!(preset("parquet").unwrap(), Preset::RangeReads);
        assert_eq!(preset("large-objects").unwrap(), Preset::LargeObjects);
        let (c, h) = code(preset("range-raeds"));
        assert_eq!(c, ErrorCode::UsageInvalidArgument);
        assert!(h.contains("did you mean `range-reads`"), "{h}");
        let (_, h) = code(preset("zzzzzz"));
        assert!(h.starts_with("choose:"), "{h}");
        assert_eq!(access("zipf:1.2").unwrap(), Access::Zipf(1.2));
        assert_eq!(access("zipf").unwrap(), Access::Zipf(1.1));
        assert_eq!(
            access("hotset:0.1/0.9").unwrap(),
            Access::Hotset {
                fraction: 0.1,
                probability: 0.9
            }
        );
        let (_, h) = code(access("unifrom"));
        assert!(h.contains("did you mean `uniform`"), "{h}");
        assert!(access("weird").is_err());
        assert!(access("zipf:-1").is_err());
        assert!(access("hotset:2/0.5").is_err());
    }

    #[test]
    fn urls_schemes_buckets_endpoints() {
        assert_eq!(scheme("S3A").unwrap(), "s3");
        assert_eq!(storage_url("gs://b/x").unwrap(), "gcs");
        let (c, h) = code(storage_url("foo://bar"));
        assert_eq!(c, ErrorCode::UsageInvalidArgument);
        assert!(h.contains("choose:") || h.contains("did you mean"), "{h}");
        let (_, h) = code(storage_url("s4://bar"));
        assert!(h.contains("did you mean `s3`"), "{h}");
        assert!(storage_url("s3://").unwrap_err().message.contains("bucket"));
        assert!(storage_url("s3:///x").is_err());
        assert!(
            storage_url("azblob://")
                .unwrap_err()
                .message
                .contains("container")
        );
        assert!(storage_url("memory://").is_ok());
        assert!(storage_url("bucket/x").is_err());
        assert_eq!(bucket("my-bucket").unwrap(), "my-bucket");
        let (_, h) = code(bucket("MyBucket"));
        assert!(h.contains("mybucket"), "{h}");
        assert!(bucket("a").is_err());
        assert!(endpoint("https://s3.example.com").is_ok());
        assert!(endpoint("ftp://x").is_err());
        assert!(endpoint("not a url").is_err());
        assert_eq!(http_version("HTTP2").unwrap(), HttpVersion::H2);
        assert!(http_version("h3").is_err());
        assert!(lab_scenario("demo").is_ok());
        assert!(
            lab_scenario("dmeo")
                .unwrap_err()
                .hint
                .unwrap()
                .contains("demo")
        );
    }

    #[test]
    fn key_values_and_run_refs() {
        assert_eq!(
            key_value("--set", "concurrency = 8").unwrap(),
            ("concurrency".into(), "8".into())
        );
        assert!(key_value("--set", "novalue").is_err());
        assert!(key_value("--set", "=x").is_err());
        assert!(run_ref("last~2").is_ok());
        assert!(run_ref("last~x").is_err());
        assert!(run_ref(" ").is_err());
    }

    #[test]
    fn suggestions() {
        assert_eq!(
            suggest("concurency", ["concurrency", "warmup"]),
            Some("concurrency")
        );
        assert_eq!(suggest("zzzzzz", ["concurrency", "warmup"]), None);
        assert_eq!(suggest("", ["a"]), None);
    }
}
