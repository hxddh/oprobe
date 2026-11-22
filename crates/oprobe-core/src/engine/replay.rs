//! Replay of real traffic (design v5 §3, fixes M14): S3 server access logs, CloudTrail S3 data
//! events and oprobe run files become an open-loop schedule of operations on the logged keys.
//!
//! - `--replay FILE [--speed N] [--map-prefix FROM=TO]`: the log's timing is kept (scaled by
//!   `speed`), so the replay is open loop; late and missed arrivals count as in any open-loop
//!   run (`report.schedule.arrivals = "replay"`).
//! - **Safe by default:** reads go to the logged (mapped) keys, but writes and deletes are only
//!   replayed when their mapped key is under the run prefix (map with `FROM=$RUN/`); every
//!   other write is refused (skipped and counted).
//! - Keys are relative to the target's root, like every other workload key.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::ops::{OpCtx, OpOutcome};

/// The map target that stands for the run prefix (`--map-prefix logs/=$RUN/`).
pub const RUN_PREFIX_TOKEN: &str = "$RUN/";
/// Events read from one log at most.
pub const MAX_EVENTS: usize = 5_000_000;

/// What to replay (`workload.replay`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplaySpec {
    /// The log: an S3 server access log, CloudTrail JSON (S3 data events) or an oprobe run file.
    #[schemars(with = "String")]
    pub file: PathBuf,
    /// Speed factor: 2 replays twice as fast as recorded.
    #[serde(default = "one")]
    pub speed: f64,
    /// Key rewrites, the first matching prefix wins (`$RUN/` targets the run prefix).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub map_prefix: Vec<(String, String)>,
}

fn one() -> f64 {
    1.0
}

impl ReplaySpec {
    pub fn new(file: PathBuf) -> Self {
        Self {
            file,
            speed: 1.0,
            map_prefix: Vec::new(),
        }
    }
}

/// One logged operation.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplayOp {
    /// GET, optionally of `len` bytes from the start (an access log only records the bytes
    /// sent, not the range).
    Read {
        key: String,
        len: Option<u64>,
    },
    Stat {
        key: String,
    },
    List {
        prefix: String,
    },
    Write {
        key: String,
        size: u64,
    },
    Delete {
        key: String,
    },
}

impl ReplayOp {
    pub fn verb(&self) -> &'static str {
        match self {
            ReplayOp::Read { .. } => "replay.read",
            ReplayOp::Stat { .. } => "replay.stat",
            ReplayOp::List { .. } => "replay.list",
            ReplayOp::Write { .. } => "replay.write",
            ReplayOp::Delete { .. } => "replay.delete",
        }
    }

    pub fn is_write(&self) -> bool {
        matches!(self, ReplayOp::Write { .. } | ReplayOp::Delete { .. })
    }

    fn key_mut(&mut self) -> &mut String {
        match self {
            ReplayOp::Read { key, .. }
            | ReplayOp::Stat { key }
            | ReplayOp::Write { key, .. }
            | ReplayOp::Delete { key } => key,
            ReplayOp::List { prefix } => prefix,
        }
    }

    pub fn key(&self) -> &str {
        match self {
            ReplayOp::Read { key, .. }
            | ReplayOp::Stat { key }
            | ReplayOp::Write { key, .. }
            | ReplayOp::Delete { key } => key,
            ReplayOp::List { prefix } => prefix,
        }
    }
}

/// One event: when (since the first event) and what.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayEvent {
    pub at: Duration,
    pub op: Arc<ReplayOp>,
}

/// The worker (of `count`) that replays `key` in a distributed run: a stable hash (FNV-1a),
/// the same on every machine and build.
pub fn shard_of(key: &str, count: u32) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h % u64::from(count.max(1))) as u32
}

/// A parsed, mapped log ready to replay.
#[derive(Clone, Debug, Default)]
pub struct ReplayLog {
    pub events: Vec<ReplayEvent>,
    /// `s3_access_log`, `cloudtrail` or `oprobe_run`.
    pub format: String,
    /// Lines / records that were not operations oprobe replays (other APIs, malformed).
    pub skipped_count: u64,
    /// Writes and deletes outside the run prefix, refused.
    pub refused_writes_count: u64,
}

impl ReplayLog {
    /// Read `spec.file`, map keys, refuse writes outside `run_prefix` and scale by `speed`.
    pub fn load(spec: &ReplaySpec, run_prefix: &str) -> Result<Self> {
        if !(spec.speed.is_finite() && spec.speed > 0.0) {
            bail!("--speed must be > 0");
        }
        let mut log = parse_file(&spec.file)?;
        let mut kept = Vec::with_capacity(log.events.len());
        for mut ev in std::mem::take(&mut log.events) {
            let op = Arc::make_mut(&mut ev.op);
            map_key(op.key_mut(), &spec.map_prefix, run_prefix);
            if op.is_write() && !op.key().starts_with(run_prefix) {
                log.refused_writes_count += 1;
                continue;
            }
            ev.at = ev.at.div_f64(spec.speed);
            kept.push(ev);
        }
        log.events = kept;
        if log.events.is_empty() {
            bail!(
                "`{}` holds no operation to replay ({} lines skipped, {} writes outside the run prefix refused)",
                spec.file.display(),
                log.skipped_count,
                log.refused_writes_count
            );
        }
        Ok(log)
    }

    /// Recorded span (after the speed factor).
    pub fn span(&self) -> Duration {
        self.events.last().map_or(Duration::ZERO, |e| e.at)
    }
}

fn map_key(key: &mut String, map: &[(String, String)], run_prefix: &str) {
    for (from, to) in map {
        if let Some(rest) = key.strip_prefix(from.as_str()) {
            let to = match to.strip_prefix(RUN_PREFIX_TOKEN) {
                Some(after) => format!("{run_prefix}{after}"),
                None if to == "$RUN" => run_prefix.to_owned(),
                None => to.clone(),
            };
            *key = format!("{to}{rest}");
            return;
        }
    }
}

/// Parse a log, detecting its format.
pub fn parse_file(path: &Path) -> Result<ReplayLog> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read `{}`", path.display()))?;
    if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return parse_run_file(path);
    }
    let text = String::from_utf8_lossy(&bytes);
    let head = text.trim_start();
    if head.starts_with('{') || head.starts_with('[') {
        parse_cloudtrail(head)
    } else {
        Ok(parse_access_log(&text))
    }
}

/// Seconds since the epoch of a civil date (proleptic Gregorian, UTC).
fn epoch_s(y: i64, m: u32, d: u32, hh: u32, mm: u32, ss: f64) -> f64 {
    let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (i64::from(m) - 3) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days as f64 * 86_400.0 + f64::from(hh * 3600 + mm * 60) + ss
}

fn month(s: &str) -> Option<u32> {
    let m = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    m.iter()
        .position(|x| x.eq_ignore_ascii_case(s))
        .map(|i| i as u32 + 1)
}

fn tz_offset_s(z: &str) -> Option<f64> {
    if z.is_empty() || z == "Z" || z == "z" {
        return Some(0.0);
    }
    let sign = match z.as_bytes()[0] {
        b'+' => 1.0,
        b'-' => -1.0,
        _ => return None,
    };
    let digits: String = z[1..].chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != 4 {
        return None;
    }
    let h: f64 = digits[..2].parse().ok()?;
    let m: f64 = digits[2..].parse().ok()?;
    Some(sign * (h * 3600.0 + m * 60.0))
}

/// `06/Feb/2019:00:00:38 +0000` (access log, without the brackets).
fn parse_clf_time(s: &str) -> Option<f64> {
    let (dt, z) = s.split_once(' ').unwrap_or((s, ""));
    let mut it = dt.splitn(4, [':', '/']);
    let d: u32 = it.next()?.parse().ok()?;
    let m = month(it.next()?)?;
    let y: i64 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':');
    let hh: u32 = hms.next()?.parse().ok()?;
    let mm: u32 = hms.next()?.parse().ok()?;
    let ss: f64 = hms.next()?.parse().ok()?;
    Some(epoch_s(y, m, d, hh, mm, ss) - tz_offset_s(z)?)
}

/// `2019-02-06T00:00:38Z`, with optional fractional seconds and offset.
fn parse_iso_time(s: &str) -> Option<f64> {
    let (date, time) = s.split_once(['T', ' '])?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let zpos = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, z) = time.split_at(zpos);
    let mut c = clock.split(':');
    let hh: u32 = c.next()?.parse().ok()?;
    let mm: u32 = c.next()?.parse().ok()?;
    let ss: f64 = c.next().unwrap_or("0").parse().ok()?;
    Some(epoch_s(y, m, day, hh, mm, ss) - tz_offset_s(z)?)
}

/// Split an access-log line into fields: `[…]` and `"…"` are single fields.
fn fields(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let (open, close) = match b[i] {
            b'[' => (true, b']'),
            b'"' => (true, b'"'),
            _ => (false, b' '),
        };
        let start = if open { i + 1 } else { i };
        let mut j = start;
        while j < b.len() && b[j] != close {
            j += 1;
        }
        out.push(&line[start..j]);
        i = j + 1;
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 3 <= b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn finish(mut timed: Vec<(f64, ReplayOp)>, format: &str, skipped: u64) -> ReplayLog {
    timed.sort_by(|a, b| a.0.total_cmp(&b.0));
    let t0 = timed.first().map_or(0.0, |t| t.0);
    ReplayLog {
        events: timed
            .into_iter()
            .map(|(t, op)| ReplayEvent {
                at: Duration::from_secs_f64((t - t0).max(0.0)),
                op: Arc::new(op),
            })
            .collect(),
        format: format.into(),
        skipped_count: skipped,
        refused_writes_count: 0,
    }
}

/// S3 server access log: `owner bucket [time] ip requester reqid OPERATION key "URI" status
/// error bytes_sent object_size total_ms turnaround_ms …` (fields 11 and 12 are the bytes).
pub fn parse_access_log(text: &str) -> ReplayLog {
    let mut timed = Vec::new();
    let mut skipped = 0u64;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if timed.len() >= MAX_EVENTS {
            break;
        }
        let f = fields(line);
        let parsed = (|| {
            let t = parse_clf_time(f.get(2)?)?;
            let operation = *f.get(6)?;
            let key = percent_decode(f.get(7)?);
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
            let op = match operation {
                "REST.GET.OBJECT" => {
                    let sent = num(11);
                    let size = num(12);
                    ReplayOp::Read {
                        len: match (sent, size) {
                            (Some(s), Some(z)) if s < z => Some(s),
                            _ => None,
                        },
                        key,
                    }
                }
                "REST.HEAD.OBJECT" => ReplayOp::Stat { key },
                "REST.GET.BUCKET" => {
                    // The prefix is in the request URI's query.
                    let uri = f.get(8).copied().unwrap_or_default();
                    let prefix = uri
                        .split_whitespace()
                        .nth(1)
                        .and_then(|u| u.split_once('?'))
                        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("prefix=")))
                        .map(percent_decode)
                        .unwrap_or_default();
                    ReplayOp::List { prefix }
                }
                "REST.PUT.OBJECT" | "REST.POST.UPLOAD" => ReplayOp::Write {
                    size: num(12).or(num(11)).unwrap_or(0),
                    key,
                },
                "REST.DELETE.OBJECT" => ReplayOp::Delete { key },
                _ => return None,
            };
            if key_is_empty(&op) {
                return None;
            }
            Some((t, op))
        })();
        match parsed {
            Some(e) => timed.push(e),
            None => skipped += 1,
        }
    }
    finish(timed, "s3_access_log", skipped)
}

fn key_is_empty(op: &ReplayOp) -> bool {
    !matches!(op, ReplayOp::List { .. }) && (op.key().is_empty() || op.key() == "-")
}

/// CloudTrail: `{"Records": [...]}`, a bare array, or one record per line.
pub fn parse_cloudtrail(text: &str) -> Result<ReplayLog> {
    let records: Vec<serde_json::Value> = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(mut o)) if o.contains_key("Records") => {
            match o.remove("Records") {
                Some(serde_json::Value::Array(a)) => a,
                _ => bail!("CloudTrail `Records` is not an array"),
            }
        }
        Ok(serde_json::Value::Array(a)) => a,
        Ok(v @ serde_json::Value::Object(_)) => vec![v],
        _ => text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()
            .context("not CloudTrail JSON (an object with `Records`, an array or JSON lines)")?,
    };
    let mut timed = Vec::new();
    let mut skipped = 0u64;
    for r in records.iter().take(MAX_EVENTS) {
        let s = |p: &str| r.pointer(p).and_then(|v| v.as_str()).map(str::to_owned);
        let n = |p: &str| {
            r.pointer(p)
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        };
        let parsed = (|| {
            let t = parse_iso_time(&s("/eventTime")?)?;
            let key = s("/requestParameters/key").unwrap_or_default();
            let op = match s("/eventName")?.as_str() {
                "GetObject" => ReplayOp::Read {
                    key,
                    len: n("/additionalEventData/bytesTransferredOut")
                        .filter(|_| r.pointer("/requestParameters/Range").is_some()),
                },
                "HeadObject" => ReplayOp::Stat { key },
                "ListObjects" | "ListObjectsV2" => ReplayOp::List {
                    prefix: s("/requestParameters/prefix").unwrap_or_default(),
                },
                "PutObject" | "CompleteMultipartUpload" => ReplayOp::Write {
                    key,
                    size: n("/additionalEventData/bytesTransferredIn").unwrap_or(0),
                },
                "DeleteObject" => ReplayOp::Delete { key },
                _ => return None,
            };
            if key_is_empty(&op) {
                return None;
            }
            Some((t, op))
        })();
        match parsed {
            Some(e) => timed.push(e),
            None => skipped += 1,
        }
    }
    Ok(finish(timed, "cloudtrail", skipped))
}

/// An oprobe run file: its operation rows (verb, key, start, bytes).
pub fn parse_run_file(path: &Path) -> Result<ReplayLog> {
    let run = crate::store::read_run_file(path)?;
    let mut timed = Vec::new();
    let mut skipped = 0u64;
    for o in run.ops.iter().take(MAX_EVENTS) {
        let key = o.key.clone();
        let v = o.verb.as_ref();
        let op = if v.starts_with("read") || v.starts_with("parquet") || v == "replay.read" {
            ReplayOp::Read { key, len: None }
        } else if v == "stat" || v == "replay.stat" {
            ReplayOp::Stat { key }
        } else if v.starts_with("list") || v == "replay.list" {
            ReplayOp::List { prefix: key }
        } else if v.starts_with("write") || v == "replay.write" {
            ReplayOp::Write { key, size: o.bytes }
        } else if v.starts_with("delete") || v == "replay.delete" {
            ReplayOp::Delete { key }
        } else {
            skipped += 1;
            continue;
        };
        timed.push((o.t_us as f64 / 1e6, op));
    }
    Ok(finish(timed, "oprobe_run", skipped))
}

/// Execute one replayed operation on the measured operator.
pub async fn execute(ctx: &OpCtx, op: &ReplayOp, key: &mut String) -> Result<OpOutcome> {
    key.clear();
    key.push_str(op.key());
    match op {
        ReplayOp::Read { key, len } => {
            let buf = match len {
                Some(l) => ctx.op.read_with(key).range(0..*l).await?,
                None => ctx.op.read(key).await?,
            };
            Ok(OpOutcome {
                logical_read: buf.len() as u64,
                logical_write: 0,
            })
        }
        ReplayOp::Stat { key } => {
            ctx.op.stat(key).await?;
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: 0,
            })
        }
        ReplayOp::List { prefix } => {
            let dir = match prefix.rfind('/') {
                Some(i) => &prefix[..=i],
                None => "",
            };
            let entries = ctx.op.list(if dir.is_empty() { "/" } else { dir }).await?;
            std::hint::black_box(entries.len());
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: 0,
            })
        }
        ReplayOp::Write { key, size } => {
            // Capped at 256 MiB: a replay measures requests, not the client's memory.
            let n = (*size as usize).clamp(1, 256 << 20);
            ctx.op.write(key, vec![0x5au8; n]).await?;
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: n as u64,
            })
        }
        ReplayOp::Delete { key } => {
            ctx.op.delete(key).await?;
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: 0,
            })
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn shards_split_every_key_to_exactly_one_worker() {
        let keys: Vec<String> = (0..1000).map(|i| format!("logs/k{i}")).collect();
        let mut counts = [0u32; 3];
        for k in &keys {
            let s = shard_of(k, 3);
            assert!(s < 3);
            assert_eq!(s, shard_of(k, 3), "stable");
            counts[s as usize] += 1;
        }
        assert!(counts.iter().all(|c| *c > 250), "{counts:?}");
        assert_eq!(shard_of("x", 1), 0);
        assert_eq!(shard_of("x", 0), 0);
    }

    use super::*;

    const ACCESS_LOG: &str = r#"79a5 awsexamplebucket1 [06/Feb/2019:00:00:38 +0000] 192.0.2.3 79a5 3E57427F3EXAMPLE REST.GET.OBJECT data/a.parquet "GET /awsexamplebucket1/data/a.parquet HTTP/1.1" 206 - 4096 8388608 70 10 "-" "S3Console/0.4" - s9lzHYrFp76ZVxRcpX9+5cjAnEH2ROuNkd2BHfIa6UkFVdtjf5mKR3/eTPFvsiP/XV/VLi31234= SigV4 ECDHE-RSA-AES128-GCM-SHA256 AuthHeader awsexamplebucket1.s3.us-west-1.amazonaws.com TLSV1.2 - -
79a5 awsexamplebucket1 [06/Feb/2019:00:00:39 +0000] 192.0.2.3 79a5 891CE47D2EXAMPLE REST.HEAD.OBJECT data/a%20b.txt "HEAD /awsexamplebucket1/data/a%20b.txt HTTP/1.1" 200 - - 113 9 - "-" "S3Console/0.4" -
79a5 awsexamplebucket1 [06/Feb/2019:00:00:40 +0000] 192.0.2.3 79a5 A1206F460EXAMPLE REST.GET.BUCKET - "GET /awsexamplebucket1?list-type=2&prefix=data%2F HTTP/1.1" 200 - 242 - 11 - "-" "S3Console/0.4" -
79a5 awsexamplebucket1 [06/Feb/2019:00:00:40 +0000] 192.0.2.3 79a5 A1206F460EXAMPLE REST.PUT.OBJECT up/x.bin "PUT /awsexamplebucket1/up/x.bin HTTP/1.1" 200 - - 1024 11 - "-" "S3Console/0.4" -
79a5 awsexamplebucket1 [06/Feb/2019:00:00:41 +0000] 192.0.2.3 79a5 BC3C074D0EXAMPLE REST.GET.VERSIONING - "GET /awsexamplebucket1?versioning HTTP/1.1" 200 - 113 - 7 - "-" "S3Console/0.4" -
garbage line
"#;

    #[test]
    fn parses_s3_access_logs() {
        let log = parse_access_log(ACCESS_LOG);
        assert_eq!(log.format, "s3_access_log");
        assert_eq!(log.events.len(), 4);
        assert_eq!(log.skipped_count, 2);
        assert_eq!(log.events[0].at, Duration::ZERO);
        assert_eq!(
            *log.events[0].op,
            ReplayOp::Read {
                key: "data/a.parquet".into(),
                len: Some(4096)
            }
        );
        assert_eq!(log.events[1].at, Duration::from_secs(1));
        assert_eq!(
            *log.events[1].op,
            ReplayOp::Stat {
                key: "data/a b.txt".into()
            }
        );
        assert_eq!(
            *log.events[2].op,
            ReplayOp::List {
                prefix: "data/".into()
            }
        );
        assert!(log.events[3].op.is_write());
    }

    #[test]
    fn parses_cloudtrail_in_every_layout() {
        let rec = |t: &str, name: &str, key: &str| {
            format!(
                r#"{{"eventTime":"{t}","eventName":"{name}","requestParameters":{{"bucketName":"b","key":"{key}"}}}}"#
            )
        };
        let a = rec("2024-03-01T10:00:00Z", "GetObject", "k/1");
        let b = rec("2024-03-01T10:00:00.500Z", "HeadObject", "k/2");
        let c = rec("2024-03-01T11:00:01+01:00", "PutObject", "k/3");
        let d = rec("2024-03-01T10:00:02Z", "GetBucketAcl", "");
        for text in [
            format!(r#"{{"Records":[{a},{b},{c},{d}]}}"#),
            format!("[{a},{b},{c},{d}]"),
            format!("{a}\n{b}\n{c}\n{d}\n"),
        ] {
            let log = parse_cloudtrail(&text).unwrap();
            assert_eq!(log.events.len(), 3, "{text}");
            assert_eq!(log.skipped_count, 1);
            assert_eq!(log.events[1].at, Duration::from_millis(500));
            // 11:00:01+01:00 is 10:00:01Z.
            assert_eq!(log.events[2].at, Duration::from_secs(1));
            assert_eq!(log.events[2].op.verb(), "replay.write");
        }
    }

    #[test]
    fn maps_keys_scales_time_and_refuses_writes_outside_the_run_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("access.log");
        std::fs::write(&f, ACCESS_LOG).unwrap();
        let run = "oprobe-run-x/";
        let mut spec = ReplaySpec::new(f.clone());
        spec.speed = 2.0;
        spec.map_prefix = vec![("data/".into(), "mirror/".into())];
        let log = ReplayLog::load(&spec, run).unwrap();
        assert_eq!(log.refused_writes_count, 1);
        assert_eq!(log.events.len(), 3);
        assert_eq!(log.events[0].op.key(), "mirror/a.parquet");
        assert_eq!(log.events[1].at, Duration::from_millis(500));
        // Writes mapped under the run prefix are replayed.
        spec.map_prefix.push(("up/".into(), "$RUN/up/".into()));
        let log = ReplayLog::load(&spec, run).unwrap();
        assert_eq!(log.refused_writes_count, 0);
        assert_eq!(log.events[3].op.key(), "oprobe-run-x/up/x.bin");
        assert_eq!(log.span(), Duration::from_secs(1));
    }
}
