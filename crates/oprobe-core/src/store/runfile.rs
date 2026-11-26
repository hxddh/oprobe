//! The run file `oprobe.run/7` (design v5 §2): a zstd stream of framed blocks, written while
//! the run is in progress and read back block by block. The process never encodes the whole
//! file in memory.
//!
//! ```text
//! zstd( frame* )         frame = kind:u8  len:u32 (LE)  payload[len] (JSON)
//! kind 1  header         RunHeader: format, run id, spec, versions
//! kind 2  segment        Segment: columnar op and request rows of one time slice
//! kind 3  interval       IntervalBlock: one per-second slice of the timeline
//! kind 4  report         the Report, written last
//! ```
//!
//! Segments are columnar: every row field is a column, strings (verbs, APIs, keys, errors,
//! request ids) are interned in the segment's table, times are `u32` microseconds after the
//! segment's base (durations are `u32` microseconds, saturating at ~71 minutes). Unknown block
//! kinds are skipped by readers, so later streams can add blocks without breaking old files.
//!
//! **Limits.** A run file is untrusted input, and a reader bounds what it takes of one
//! ([`Limits`]): the bytes of a block, the entries of a segment's column (checked while the
//! column is deserialised, before anything is built of it), the bytes the file decodes to and
//! the memory of its rows. The writers keep to the same limits: a segment is cut at
//! [`SEGMENT_ROWS`] rows or before its JSON passes [`MAX_SEGMENT_BYTES`], and a streamed run
//! ([`RunFileStream`]) stops keeping rows before the file passes what a reader accepts.
//!
//! [`RunFileWriter`] writes to `<path>.tmp` and renames on [`RunFileWriter::finish`]; a run that
//! crashes leaves no half-written `.oprobe` behind.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::{
    OpBreakdownUs, OpRow, RUN_FORMAT, Report, ReqDetail, ReqRow, Run, RunSpec, TcpRow, TlsRow,
    WireProtocol,
};
use crate::trace::rows::FailureCode;

const ZSTD_LEVEL: i32 = 3;
/// Rows (operations plus requests) a writer puts in one segment at most.
pub const SEGMENT_ROWS: usize = 16_384;
/// The most entries one column of a segment may hold: a reader refuses the segment at the
/// entry after, before it is built. Four times what a writer cuts at: the streams of 9.2 and
/// earlier cut only between operations (one with many requests ran past [`SEGMENT_ROWS`]),
/// and [`write_run`] cut at [`SEGMENT_ROWS`] rows of each kind.
pub const MAX_SEGMENT_ROWS: usize = 4 * SEGMENT_ROWS;
/// The most strings a segment's table may hold (a row interns a handful: its key, its error,
/// its request id, its address).
pub const MAX_SEGMENT_STRINGS: usize = 8 * MAX_SEGMENT_ROWS;

const KIND_HEADER: u8 = 1;
const KIND_SEGMENT: u8 = 2;
const KIND_INTERVAL: u8 = 3;
const KIND_REPORT: u8 = 4;

/// The largest segment block a run file may hold, as JSON. A row is about 220 bytes in a lab
/// run (a segment of [`SEGMENT_ROWS`] rows: 2 to 7 MiB), so this leaves 4 KiB a row for long
/// keys, error texts and await trees; a writer cuts a segment before it gets there.
pub const MAX_SEGMENT_BYTES: u32 = 64 << 20;
/// The largest header, interval or report block (a lab run's report is some 25 KiB; a long
/// run's, with its 2 000 intervals, a few MiB).
pub const MAX_BLOCK_BYTES: u32 = 64 << 20;
/// The most a run file may decompress to, blocks of unknown kinds included (they are skipped,
/// never buffered). A run at the default row cap (`max_requests`, 1 000 000 rows) decompresses
/// to about 220 MiB: this reads runs kept at nineteen times that cap.
pub const MAX_DECODED_BYTES: u64 = 4 << 30;
/// The most memory the rows decoded from one run file may take: the row structs and the
/// strings they own. A segment's interned strings are copied into every row naming them, so
/// the bytes of the file do not bound it. A lab run's rows take about 440 bytes each: this
/// holds nine times the default row cap.
pub const MAX_ROW_BYTES: u64 = 4 << 30;

/// What a reader accepts from one run file: files are untrusted input (they are attached to
/// issues and opened from anywhere), so no length a file declares is believed before the
/// bytes are there, and what it decodes to is capped.
#[derive(Clone, Copy, Debug)]
struct Limits {
    segment_bytes: u32,
    block_bytes: u32,
    decoded_bytes: u64,
    row_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            segment_bytes: MAX_SEGMENT_BYTES,
            block_bytes: MAX_BLOCK_BYTES,
            decoded_bytes: MAX_DECODED_BYTES,
            row_bytes: MAX_ROW_BYTES,
        }
    }
}

impl Limits {
    /// The largest block of `kind` (a kind this version does not know may be as large as a
    /// segment: it is skipped, not buffered).
    fn block(&self, kind: u8) -> u32 {
        match kind {
            KIND_HEADER | KIND_INTERVAL | KIND_REPORT => self.block_bytes,
            _ => self.segment_bytes,
        }
    }
}

/// The message of a run file refused for its size ([`super::runs::decode_run`] gives it its
/// own hint: the file is not damaged).
pub(super) const TOO_LARGE: &str = "run file too large to read";

/// The memory the rows of one run file may still take ([`MAX_ROW_BYTES`] at first).
#[derive(Debug)]
pub struct RowBudget {
    left: u64,
}

impl Default for RowBudget {
    fn default() -> Self {
        Self::new(MAX_ROW_BYTES)
    }
}

impl RowBudget {
    fn new(bytes: u64) -> Self {
        Self { left: bytes }
    }

    /// Take `bytes` of it, before they are allocated.
    fn take(&mut self, bytes: usize) -> Result<()> {
        match self.left.checked_sub(bytes as u64) {
            Some(left) => {
                self.left = left;
                Ok(())
            }
            None => bail!("{TOO_LARGE}: its rows take more memory than this oprobe reads"),
        }
    }
}

/// The first block: what the run is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunHeader {
    /// Always [`RUN_FORMAT`].
    pub format: String,
    pub run_id: String,
    pub oprobe_version: String,
    pub created_at: String,
    pub spec: RunSpec,
}

impl RunHeader {
    pub fn new(run_id: impl Into<String>, spec: RunSpec) -> Self {
        Self {
            format: RUN_FORMAT.to_owned(),
            run_id: run_id.into(),
            oprobe_version: env!("CARGO_PKG_VERSION").to_owned(),
            created_at: crate::util::now_rfc3339(),
            spec,
        }
    }
}

/// One slice of the run's timeline: 1 s for the first ten minutes, then 10 s (design v5 §3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IntervalBlock {
    /// Start of the slice, seconds from the window start.
    pub t_s: f64,
    pub len_s: f64,
    /// Operations started in the slice.
    pub ops_count: u64,
    /// Operations that failed in the slice (when they ended).
    pub errors_count: u64,
    /// Throttled responses received in the slice (before 6.3: throttled operations).
    pub throttled_count: u64,
    pub transferred_bytes: u64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    /// Peak requests in flight in the slice (when measured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inflight_count: Option<u64>,
    #[serde(default)]
    pub p90_ms: f64,
    #[serde(default)]
    pub p999_ms: f64,
    #[serde(default)]
    pub max_ms: f64,
    #[serde(default)]
    pub missed_arrivals_count: u64,
    #[serde(default)]
    pub new_connections_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f64>,
    /// Operations per heatmap latency row (the interval's latency sketch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub latency_rows_count: Vec<u64>,
    /// Operations started in the slice that succeeded.
    #[serde(default)]
    pub ok_ops_count: u64,
    /// Operations started in the slice that saw a throttled response.
    #[serde(default)]
    pub throttled_ops_count: u64,
    /// Requests sent in the slice, retries included.
    #[serde(default)]
    pub requests_count: u64,
    /// Retries issued in the slice.
    #[serde(default)]
    pub retries_count: u64,
}

impl IntervalBlock {
    /// The block of one timeline point.
    pub fn of(p: &crate::trace::TimelinePoint) -> Self {
        let len = if p.len_s > 0.0 { p.len_s } else { 1.0 };
        Self {
            t_s: p.t_s,
            len_s: len,
            ops_count: p.ops_count,
            errors_count: p.errors,
            throttled_count: p.throttled,
            transferred_bytes: (p.mib_per_s * len * 1024.0 * 1024.0).round().max(0.0) as u64,
            p50_ms: p.p50_ms,
            p99_ms: p.p99_ms,
            inflight_count: (p.inflight_peak > 0).then_some(p.inflight_peak),
            p90_ms: p.p90_ms,
            p999_ms: p.p999_ms,
            max_ms: p.max_ms,
            missed_arrivals_count: p.missed_arrivals,
            new_connections_count: p.new_connections,
            cpu_pct: p.cpu_pct,
            latency_rows_count: p.latency_rows_count.clone(),
            ok_ops_count: p.ok_ops_count,
            throttled_ops_count: p.throttled_ops,
            requests_count: p.requests_count,
            retries_count: p.retries_count,
        }
    }
}

/// Interned strings of one segment.
#[derive(Debug, Default)]
struct Interner {
    table: Vec<String>,
    index: HashMap<String, u32>,
    /// What the table takes of the segment's JSON, at most ([`json::string`] of each).
    json: usize,
}

impl Interner {
    fn id(&mut self, s: &str) -> u32 {
        if let Some(i) = self.index.get(s) {
            return *i;
        }
        let i = self.table.len() as u32;
        self.table.push(s.to_owned());
        self.index.insert(s.to_owned(), i);
        self.json += json::string(s);
        i
    }

    fn opt(&mut self, s: Option<&str>) -> Option<u32> {
        s.map(|s| self.id(s))
    }
}

fn u32_sat(v: u64) -> u32 {
    v.min(u64::from(u32::MAX)) as u32
}

/// Upper bounds of what rows take of a segment's JSON, so a writer knows what a segment
/// serialises to before it does ([`SegmentBuilder::json_bytes`]). Each is the most the value
/// can take, with the comma after it; a segment's real JSON is about a third of its bound.
mod json {
    use serde::Serialize;

    use super::{OpRow, ReqRow, Segment};

    /// The digits of any `u32`, `u64` and `f64` (ryu prints at most 24 characters), a `bool`
    /// and `null`.
    const U32: usize = 10;
    const U64: usize = 20;
    const F64: usize = 24;
    const BOOL: usize = 5;
    /// An index into the string table, or `null`.
    const ID: usize = U32;
    /// `[i,` … `],` around a sparse column's value.
    const SPARSE: usize = U32 + 4;

    /// The bytes serde_json writes for `v`.
    pub(super) fn len<T: Serialize>(v: &T) -> usize {
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0 += b.len();
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut c = Count(0);
        // Counting cannot fail; a value serde_json cannot write is refused when it is written.
        let _ = serde_json::to_writer(&mut c, v);
        c.0
    }

    /// A string in the table: its bytes as JSON escapes them, the quotes and the comma.
    pub(super) fn string(s: &str) -> usize {
        let escaped: usize = s
            .bytes()
            .map(|b| match b {
                b'"' | b'\\' => 2,
                // `\u00XX` at most (`\n`, `\t`, … take two).
                0..=0x1f => 6,
                _ => 1,
            })
            .sum();
        escaped + 3
    }

    fn opt(s: Option<&str>) -> usize {
        s.map_or(0, string)
    }

    /// A segment without rows (the column names, the base time) and its block's head.
    pub(super) fn empty() -> usize {
        static EMPTY: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        // `base_us` may be any `i64`; an empty optional column is not written at all.
        *EMPTY.get_or_init(|| len(&Segment::default()) + U64 + 64 + 5)
    }

    /// An operation row's columns, its strings apart.
    pub(super) fn op(o: &OpRow) -> usize {
        let code = o.error_code.map_or(4, |c| len(&c));
        let weight = if (o.weight - 1.0).abs() > f64::EPSILON {
            SPARSE + F64
        } else {
            0
        };
        // op_id, verb, key, t_us, latency_us, bytes, ok, error_code, error, throttled,
        // attempts, breakdown, requests, await_tree: fourteen columns.
        U64 + ID
            + ID
            + U32
            + U32
            + U64
            + BOOL
            + code
            + ID
            + BOOL
            + U32
            + (5 * (U32 + 1) + 2)
            + U32
            + ID
            + 14
            + weight
    }

    /// The strings an operation row names, were none of them in the table yet.
    pub(super) fn op_strings(o: &OpRow) -> usize {
        string(&o.verb) + string(&o.key) + opt(o.error.as_deref()) + opt(o.await_tree.as_deref())
    }

    /// A request row's columns, its strings apart.
    pub(super) fn req(r: &ReqRow) -> usize {
        let code = r.error_code.map_or(4, |c| len(&c));
        let sparse = |n: usize| SPARSE + n;
        // t_us, op_id, attempt, api, method, status, protocol, reused, conn_id, remote,
        // stages, bytes_up, bytes_down, request_id, retry_after_ms, error_code, error:
        // seventeen columns.
        U32 + U64
            + U32
            + (r.api.label().len() + 2)
            + ID
            + 5
            + len(&r.protocol)
            + BOOL
            + U64
            + ID
            + (10 * (U32 + 1) + 2)
            + U64
            + U64
            + ID
            + U64
            + code
            + ID
            + 17
            + r.service_op.as_ref().map_or(0, |_| sparse(ID))
            + r.tls.as_ref().map_or(0, |t| sparse(len(&**t)))
            + r.tcp.as_ref().map_or(0, |t| sparse(len(&**t)))
            + r.detail.as_ref().map_or(0, |d| sparse(len(&**d)))
    }

    /// The strings a request row names, were none of them in the table yet (an address
    /// prints in at most 61 characters: IPv6, a scope and a port).
    pub(super) fn req_strings(r: &ReqRow) -> usize {
        string(&r.method)
            + opt(r.service_op.as_deref())
            + r.remote.map_or(0, |_| 64)
            + opt(r.request_id.as_deref())
            + opt(r.error.as_deref())
    }
}

/// A column of a segment as a reader takes it: at most `MAX` entries. The entry after is an
/// error before the next is parsed, so a column costs a reader at most `MAX` values however
/// many the file writes (`[0,{}],` is seven bytes of JSON and three hundred of struct).
fn capped<'de, D, T, const MAX: usize>(d: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Capped<T, const MAX: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const MAX: usize> serde::de::Visitor<'de> for Capped<T, MAX> {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "a column of at most {MAX} entries")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Vec<T>, A::Error> {
            // The length a format announces is not believed either.
            let mut v = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX).min(4096));
            while let Some(x) = seq.next_element()? {
                if v.len() == MAX {
                    return Err(serde::de::Error::custom(format_args!(
                        "more than {MAX} entries in one column of a segment"
                    )));
                }
                v.push(x);
            }
            Ok(v)
        }
    }
    d.deserialize_seq(Capped::<T, MAX>(std::marker::PhantomData))
}

/// A row column: at most [`MAX_SEGMENT_ROWS`] entries.
fn column<'de, D, T>(d: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    capped::<D, T, MAX_SEGMENT_ROWS>(d)
}

/// The string table: at most [`MAX_SEGMENT_STRINGS`] entries.
fn string_table<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<String>, D::Error> {
    capped::<D, String, MAX_SEGMENT_STRINGS>(d)
}

/// Columnar operation rows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
struct OpColumns {
    #[serde(deserialize_with = "column")]
    op_id: Vec<u64>,
    #[serde(deserialize_with = "column")]
    verb: Vec<u32>,
    #[serde(deserialize_with = "column")]
    key: Vec<u32>,
    #[serde(deserialize_with = "column")]
    t_us: Vec<u32>,
    #[serde(deserialize_with = "column")]
    latency_us: Vec<u32>,
    #[serde(deserialize_with = "column")]
    bytes: Vec<u64>,
    #[serde(deserialize_with = "column")]
    ok: Vec<bool>,
    #[serde(deserialize_with = "column")]
    error_code: Vec<Option<FailureCode>>,
    #[serde(deserialize_with = "column")]
    error: Vec<Option<u32>>,
    #[serde(deserialize_with = "column")]
    throttled: Vec<bool>,
    #[serde(deserialize_with = "column")]
    attempts: Vec<u32>,
    /// queue, cache, backend, retry, client (µs) per row.
    #[serde(deserialize_with = "column")]
    breakdown: Vec<[u32; 5]>,
    #[serde(deserialize_with = "column")]
    requests: Vec<u32>,
    #[serde(deserialize_with = "column")]
    await_tree: Vec<Option<u32>>,
    /// Sparse sampling weights `(row index, weight)` when `weight != 1.0`.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "column"
    )]
    weights: Vec<(u32, f64)>,
}

/// Columnar request rows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
struct ReqColumns {
    #[serde(deserialize_with = "column")]
    t_us: Vec<u32>,
    #[serde(deserialize_with = "column")]
    op_id: Vec<u64>,
    #[serde(deserialize_with = "column")]
    attempt: Vec<u32>,
    #[serde(deserialize_with = "column")]
    api: Vec<crate::trace::Api>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "column"
    )]
    service_op: Vec<(u32, u32)>,
    #[serde(deserialize_with = "column")]
    method: Vec<u32>,
    #[serde(deserialize_with = "column")]
    status: Vec<u16>,
    #[serde(deserialize_with = "column")]
    protocol: Vec<WireProtocol>,
    #[serde(deserialize_with = "column")]
    reused: Vec<bool>,
    #[serde(deserialize_with = "column")]
    conn_id: Vec<u64>,
    #[serde(deserialize_with = "column")]
    remote: Vec<Option<u32>>,
    /// queue, dns, tcp, proxy, tls, send, ttfb, body, stall, total (µs) per row.
    #[serde(deserialize_with = "column")]
    stages: Vec<[u32; 10]>,
    #[serde(deserialize_with = "column")]
    bytes_up: Vec<u64>,
    #[serde(deserialize_with = "column")]
    bytes_down: Vec<u64>,
    #[serde(deserialize_with = "column")]
    request_id: Vec<Option<u32>>,
    #[serde(deserialize_with = "column")]
    retry_after_ms: Vec<Option<u64>>,
    #[serde(deserialize_with = "column")]
    error_code: Vec<Option<FailureCode>>,
    #[serde(deserialize_with = "column")]
    error: Vec<Option<u32>>,
    /// Sparse: (row index, value).
    #[serde(deserialize_with = "column")]
    tls: Vec<(u32, TlsRow)>,
    #[serde(deserialize_with = "column")]
    tcp: Vec<(u32, TcpRow)>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "column"
    )]
    detail: Vec<(u32, ReqDetail)>,
}

/// One block of rows: a time slice of the run (columnar, interned strings).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Segment {
    /// Times are µs after this base (µs from the window start; may be negative).
    base_us: i64,
    #[serde(deserialize_with = "string_table")]
    strings: Vec<String>,
    ops: OpColumns,
    reqs: ReqColumns,
}

/// Builds one [`Segment`] row by row (rows in any order; times are rebased when it is
/// finished), so a streaming writer never holds owned copies of the rows.
#[derive(Debug, Default)]
pub struct SegmentBuilder {
    strings: Interner,
    ops: OpColumns,
    op_t: Vec<i64>,
    reqs: ReqColumns,
    req_t: Vec<i64>,
    /// What the rows' columns take of the segment's JSON, at most (the strings apart).
    rows_json: usize,
}

impl SegmentBuilder {
    /// What the segment serialises to, at most.
    pub fn json_bytes(&self) -> usize {
        json::empty() + self.strings.json + self.rows_json
    }

    /// Whether a row of at most `row_json` bytes still goes into this segment: it has room
    /// for a row, and its JSON stays within `segment_bytes`. An empty segment takes any row.
    fn takes(&self, row_json: usize, segment_bytes: u32) -> bool {
        self.is_empty()
            || (self.rows() < SEGMENT_ROWS
                && self.json_bytes().saturating_add(row_json) <= segment_bytes as usize)
    }

    pub fn push_op(&mut self, o: &OpRow) {
        self.rows_json += json::op(o);
        let s = &mut self.strings;
        let oc = &mut self.ops;
        let idx = oc.op_id.len() as u32;
        oc.op_id.push(o.op_id);
        oc.verb.push(s.id(&o.verb));
        oc.key.push(s.id(&o.key));
        self.op_t.push(o.t_us);
        oc.latency_us.push(u32_sat(o.latency_us));
        oc.bytes.push(o.bytes);
        oc.ok.push(o.ok);
        oc.error_code.push(o.error_code);
        oc.error.push(s.opt(o.error.as_deref()));
        oc.throttled.push(o.throttled);
        oc.attempts.push(o.attempts);
        let b = &o.breakdown;
        oc.breakdown.push(
            [
                b.queue_us,
                b.cache_us,
                b.backend_us,
                b.retry_us,
                b.client_us,
            ]
            .map(u32_sat),
        );
        oc.requests.push(o.requests);
        oc.await_tree.push(s.opt(o.await_tree.as_deref()));
        if (o.weight - 1.0).abs() > f64::EPSILON {
            oc.weights.push((idx, o.weight));
        }
    }

    pub fn push_req(&mut self, r: &ReqRow) {
        self.rows_json += json::req(r);
        let s = &mut self.strings;
        let rc = &mut self.reqs;
        let i = rc.op_id.len();
        self.req_t.push(r.t_us);
        rc.op_id.push(r.op_id);
        rc.attempt.push(r.attempt);
        rc.api.push(r.api);
        if let Some(so) = &r.service_op {
            rc.service_op.push((i as u32, s.id(so)));
        }
        rc.method.push(s.id(&r.method));
        rc.status.push(r.status);
        rc.protocol.push(r.protocol);
        rc.reused.push(r.reused);
        rc.conn_id.push(r.conn_id);
        rc.remote.push(r.remote.map(|a| s.id(&a.to_string())));
        rc.stages.push(
            [
                r.queue_us, r.dns_us, r.tcp_us, r.proxy_us, r.tls_us, r.send_us, r.ttfb_us,
                r.body_us, r.stall_us, r.total_us,
            ]
            .map(u32_sat),
        );
        rc.bytes_up.push(r.bytes_up);
        rc.bytes_down.push(r.bytes_down);
        rc.request_id.push(s.opt(r.request_id.as_deref()));
        rc.retry_after_ms.push(r.retry_after_ms);
        rc.error_code.push(r.error_code);
        rc.error.push(s.opt(r.error.as_deref()));
        if let Some(t) = &r.tls {
            rc.tls.push((i as u32, (**t).clone()));
        }
        if let Some(t) = &r.tcp {
            rc.tcp.push((i as u32, (**t).clone()));
        }
        if let Some(x) = &r.detail {
            rc.detail.push((i as u32, (**x).clone()));
        }
    }

    /// Rows (operations plus requests) so far.
    pub fn rows(&self) -> usize {
        self.op_t.len() + self.req_t.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    /// The segment (times rebased on the earliest row); the builder is left empty.
    pub fn finish(&mut self) -> Segment {
        let b = std::mem::take(self);
        let base_us = b.op_t.iter().chain(&b.req_t).copied().min().unwrap_or(0);
        let rel = |t: i64| u32_sat(t.saturating_sub(base_us).max(0) as u64);
        let mut ops = b.ops;
        ops.t_us = b.op_t.iter().map(|t| rel(*t)).collect();
        let mut reqs = b.reqs;
        reqs.t_us = b.req_t.iter().map(|t| rel(*t)).collect();
        Segment {
            base_us,
            strings: b.strings.table,
            ops,
            reqs,
        }
    }
}

impl Segment {
    /// Encode rows (any order) into one segment.
    pub fn encode(ops: &[OpRow], reqs: &[ReqRow]) -> Self {
        let mut b = SegmentBuilder::default();
        for o in ops {
            b.push_op(o);
        }
        for r in reqs {
            b.push_req(r);
        }
        b.finish()
    }

    /// Operation and request rows of this segment, within `budget`: every row is paid for
    /// before it is built, with the strings it copies out of the segment's table.
    pub fn decode(&self, budget: &mut RowBudget) -> Result<(Vec<OpRow>, Vec<ReqRow>)> {
        let st = |i: u32| -> Result<&str> {
            self.strings
                .get(i as usize)
                .map(String::as_str)
                .with_context(|| format!("run file segment: no string #{i}"))
        };
        let sto = |i: Option<u32>| -> Result<Option<&str>> { i.map(st).transpose() };
        let len = |s: Option<&str>| s.map_or(0, str::len);
        let t = |v: u32| -> Result<i64> {
            self.base_us
                .checked_add(i64::from(v))
                .context("corrupt run file: a row's time is out of range")
        };
        let o = &self.ops;
        let n = o.op_id.len();
        let cols_ok = [
            o.verb.len(),
            o.key.len(),
            o.t_us.len(),
            o.latency_us.len(),
            o.bytes.len(),
            o.ok.len(),
            o.error_code.len(),
            o.error.len(),
            o.throttled.len(),
            o.attempts.len(),
            o.breakdown.len(),
            o.requests.len(),
            o.await_tree.len(),
        ]
        .iter()
        .all(|l| *l == n);
        if !cols_ok {
            bail!("run file segment: operation columns differ in length");
        }
        budget.take(n.saturating_mul(std::mem::size_of::<OpRow>()))?;
        let mut ops = Vec::with_capacity(n);
        for i in 0..n {
            let b = o.breakdown[i].map(u64::from);
            let (verb, verb_len) = static_name(st(o.verb[i])?);
            let key = st(o.key[i])?;
            let (error, await_tree) = (sto(o.error[i])?, sto(o.await_tree[i])?);
            budget.take(verb_len + key.len() + len(error) + len(await_tree))?;
            ops.push(OpRow {
                op_id: o.op_id[i],
                verb,
                key: key.to_owned(),
                t_us: t(o.t_us[i])?,
                latency_us: u64::from(o.latency_us[i]),
                bytes: o.bytes[i],
                ok: o.ok[i],
                error_code: o.error_code[i],
                error: error.map(str::to_owned),
                throttled: o.throttled[i],
                attempts: o.attempts[i],
                breakdown: OpBreakdownUs {
                    queue_us: b[0],
                    cache_us: b[1],
                    backend_us: b[2],
                    retry_us: b[3],
                    client_us: b[4],
                },
                requests: o.requests[i],
                await_tree: await_tree.map(str::to_owned),
                weight: 1.0,
            });
        }
        for &(i, w) in &o.weights {
            if let Some(row) = ops.get_mut(i as usize) {
                row.weight = w;
            }
        }
        let r = &self.reqs;
        let m = r.t_us.len();
        let cols_ok = [
            r.op_id.len(),
            r.attempt.len(),
            r.api.len(),
            r.method.len(),
            r.status.len(),
            r.protocol.len(),
            r.reused.len(),
            r.conn_id.len(),
            r.remote.len(),
            r.stages.len(),
            r.bytes_up.len(),
            r.bytes_down.len(),
            r.request_id.len(),
            r.retry_after_ms.len(),
            r.error_code.len(),
            r.error.len(),
        ]
        .iter()
        .all(|l| *l == m);
        if !cols_ok {
            bail!("run file segment: request columns differ in length");
        }
        // The sparse columns (TLS, TCP, detail) box one value per entry.
        budget.take(
            m.saturating_mul(std::mem::size_of::<ReqRow>())
                .saturating_add(r.tls.len().saturating_mul(std::mem::size_of::<TlsRow>()))
                .saturating_add(r.tcp.len().saturating_mul(std::mem::size_of::<TcpRow>()))
                .saturating_add(
                    r.detail
                        .len()
                        .saturating_mul(std::mem::size_of::<ReqDetail>()),
                ),
        )?;
        let mut reqs = Vec::with_capacity(m);
        for i in 0..m {
            let s = r.stages[i].map(u64::from);
            let (method, method_len) = static_name(st(r.method[i])?);
            let (request_id, error) = (sto(r.request_id[i])?, sto(r.error[i])?);
            budget.take(method_len + len(request_id) + len(error))?;
            reqs.push(ReqRow {
                t_us: t(r.t_us[i])?,
                op_id: r.op_id[i],
                attempt: r.attempt[i],
                api: r.api[i],
                service_op: None,
                method,
                status: r.status[i],
                protocol: r.protocol[i],
                reused: r.reused[i],
                conn_id: r.conn_id[i],
                remote: match r.remote[i] {
                    Some(a) => Some(
                        st(a)?
                            .parse()
                            .with_context(|| format!("run file segment: bad address #{a}"))?,
                    ),
                    None => None,
                },
                queue_us: s[0],
                dns_us: s[1],
                tcp_us: s[2],
                proxy_us: s[3],
                tls_us: s[4],
                send_us: s[5],
                ttfb_us: s[6],
                body_us: s[7],
                stall_us: s[8],
                total_us: s[9],
                bytes_up: r.bytes_up[i],
                bytes_down: r.bytes_down[i],
                request_id: request_id.map(str::to_owned),
                retry_after_ms: r.retry_after_ms[i],
                error_code: r.error_code[i],
                error: error.map(str::to_owned),
                tls: None,
                tcp: None,
                detail: None,
            });
        }
        for &(i, sid) in &r.service_op {
            let (name, name_len) = static_name(st(sid)?);
            budget.take(name_len)?;
            if let Some(q) = reqs.get_mut(i as usize) {
                q.service_op = Some(name);
            }
        }
        for (i, v) in &r.tls {
            if let Some(q) = reqs.get_mut(*i as usize) {
                q.tls = Some(Box::new(v.clone()));
            }
        }
        for (i, v) in &r.tcp {
            if let Some(q) = reqs.get_mut(*i as usize) {
                q.tcp = Some(Box::new(v.clone()));
            }
        }
        for (i, v) in &r.detail {
            if let Some(q) = reqs.get_mut(*i as usize) {
                q.detail = Some(Box::new(v.clone()));
            }
        }
        Ok((ops, reqs))
    }

    pub fn ops_count(&self) -> usize {
        self.ops.op_id.len()
    }

    pub fn requests_count(&self) -> usize {
        self.reqs.t_us.len()
    }
}

/// The names oprobe itself records as static strings (HTTP methods, the workloads' verbs),
/// sorted: a row naming one of them borrows it instead of owning a copy.
const STATIC_NAMES: [&str; 21] = [
    "DELETE",
    "GET",
    "HEAD",
    "OPTIONS",
    "PATCH",
    "POST",
    "PUT",
    "copy",
    "delete.batch",
    "list",
    "list.paged",
    "parquet.scan",
    "parquet.scan.suffix",
    "read.full",
    "read.presigned",
    "read.range",
    "stat",
    "write",
    "write.conditional",
    "write.metadata",
    "write.multipart",
];

/// The name oprobe knows that `s` spells ([`STATIC_NAMES`], the API labels).
fn known_name(s: &str) -> Option<&'static str> {
    if let Ok(i) = STATIC_NAMES.binary_search(&s) {
        return Some(STATIC_NAMES[i]);
    }
    crate::trace::Api::ALL
        .iter()
        .map(|a| a.label())
        .find(|l| *l == s)
}

/// A method, verb or service operation name as a row holds it, and the heap bytes it takes:
/// one of the names oprobe knows is borrowed, any other name owned. Nothing a file says
/// outlives the rows decoded from it.
fn static_name(s: &str) -> (Cow<'static, str>, usize) {
    match known_name(s) {
        Some(n) => (Cow::Borrowed(n), 0),
        None => (Cow::Owned(s.to_owned()), s.len()),
    }
}

/// What a reader's [`RowBudget`] pays for a row ([`Segment::decode`]): the writers count the
/// same, so a stream knows what its file costs to read.
mod mem {
    use super::{OpRow, ReqDetail, ReqRow, TcpRow, TlsRow, known_name};

    fn name(s: &str) -> usize {
        known_name(s).map_or(s.len(), |_| 0)
    }

    fn opt(s: Option<&str>) -> usize {
        s.map_or(0, str::len)
    }

    pub(super) fn op(o: &OpRow) -> usize {
        std::mem::size_of::<OpRow>()
            + name(&o.verb)
            + o.key.len()
            + opt(o.error.as_deref())
            + opt(o.await_tree.as_deref())
    }

    pub(super) fn req(r: &ReqRow) -> usize {
        let boxed = |there: bool, size: usize| if there { size } else { 0 };
        std::mem::size_of::<ReqRow>()
            + name(&r.method)
            + r.service_op.as_deref().map_or(0, name)
            + opt(r.request_id.as_deref())
            + opt(r.error.as_deref())
            + boxed(r.tls.is_some(), std::mem::size_of::<TlsRow>())
            + boxed(r.tcp.is_some(), std::mem::size_of::<TcpRow>())
            + boxed(r.detail.is_some(), std::mem::size_of::<ReqDetail>())
    }
}

/// A block of a run file.
#[derive(Clone, Debug)]
pub enum Block {
    Header(Box<RunHeader>),
    Segment(Box<Segment>),
    Interval(IntervalBlock),
    Report(Box<Report>),
    /// A kind this version does not know (skipped by [`read_run`]).
    Unknown(u8),
}

/// Streams a run to `<path>` (through `<path>.tmp`): the header first, segments and intervals
/// as the run produces them, the report last. It writes within the limits a reader accepts
/// ([`Limits`]): a block over its kind's limit, or one that takes the file past what it may
/// decode to, is an error here and not a file that cannot be opened.
pub struct RunFileWriter {
    path: PathBuf,
    tmp: PathBuf,
    enc: Option<zstd::stream::Encoder<'static, BufWriter<std::fs::File>>>,
    limits: Limits,
    /// Bytes written so far as a reader counts them (block heads and payloads).
    decoded: u64,
    ops: u64,
    reqs: u64,
}

impl RunFileWriter {
    /// Start a run file with its header.
    pub fn create(path: &Path, header: &RunHeader) -> Result<Self> {
        Self::create_within(path, header, Limits::default())
    }

    fn create_within(path: &Path, header: &RunHeader, limits: Limits) -> Result<Self> {
        if let Some(dir) = path.parent() {
            super::create_private_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let mut tmp_os = path.as_os_str().to_os_string();
        tmp_os.push(".tmp");
        let tmp = PathBuf::from(tmp_os);
        let file =
            super::private_file(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
        let enc = zstd::stream::Encoder::new(BufWriter::new(file), ZSTD_LEVEL)?;
        let mut w = Self {
            path: path.to_owned(),
            tmp,
            enc: Some(enc),
            limits,
            decoded: 0,
            ops: 0,
            reqs: 0,
        };
        w.frame(KIND_HEADER, &serde_json::to_vec(header)?)?;
        Ok(w)
    }

    /// The bytes segments and intervals may still take: what the file may decode to, less
    /// what is written and the room kept for the report.
    fn room(&self) -> u64 {
        self.limits
            .decoded_bytes
            .saturating_sub(self.decoded)
            .saturating_sub(5 + u64::from(self.limits.block_bytes))
    }

    fn frame(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        // Never write a block the reader refuses.
        let len = u32::try_from(payload.len())
            .ok()
            .filter(|len| *len <= self.limits.block(kind))
            .context("run file block too large")?;
        let decoded = self.decoded + 5 + u64::from(len);
        if decoded > self.limits.decoded_bytes {
            bail!(
                "run file too large to write: it would decompress to more than {}",
                crate::util::fmt_bytes(self.limits.decoded_bytes)
            );
        }
        let enc = self.enc.as_mut().context("run file already finished")?;
        enc.write_all(&[kind])?;
        enc.write_all(&len.to_le_bytes())?;
        enc.write_all(payload)?;
        self.decoded = decoded;
        Ok(())
    }

    /// Append rows, in segments of at most [`SEGMENT_ROWS`] rows and [`MAX_SEGMENT_BYTES`]:
    /// each a time slice when `ops` and `reqs` are sorted by start time.
    pub fn write_rows(&mut self, ops: &[OpRow], reqs: &[ReqRow]) -> Result<()> {
        let mut b = SegmentBuilder::default();
        let (mut oi, mut ri) = (0, 0);
        while oi < ops.len() || ri < reqs.len() {
            let op_next = match (ops.get(oi), reqs.get(ri)) {
                (Some(o), Some(r)) => o.t_us <= r.t_us,
                (o, _) => o.is_some(),
            };
            let row_json = if op_next {
                op_json(&ops[oi])
            } else {
                req_json(&reqs[ri])
            };
            if !b.takes(row_json, self.limits.segment_bytes) {
                self.write_segment(&b.finish())?;
            }
            if op_next {
                b.push_op(&ops[oi]);
                oi += 1;
            } else {
                b.push_req(&reqs[ri]);
                ri += 1;
            }
        }
        if !b.is_empty() {
            self.write_segment(&b.finish())?;
        }
        Ok(())
    }

    pub fn write_segment(&mut self, s: &Segment) -> Result<()> {
        self.frame(KIND_SEGMENT, &serde_json::to_vec(s)?)?;
        self.ops += s.ops_count() as u64;
        self.reqs += s.requests_count() as u64;
        Ok(())
    }

    /// Append one slice of the timeline. The report holds the timeline too
    /// (`report.timeline`): an interval that would take the room the report needs is left
    /// out, never the report.
    pub fn write_interval(&mut self, i: &IntervalBlock) -> Result<()> {
        let payload = serde_json::to_vec(i)?;
        if 5 + payload.len() as u64 > self.room() {
            return Ok(());
        }
        self.frame(KIND_INTERVAL, &payload)
    }

    /// Rows written so far.
    pub fn rows_written(&self) -> (u64, u64) {
        (self.ops, self.reqs)
    }

    /// Write the report, close the stream and move the file into place.
    pub fn finish(mut self, report: &Report) -> Result<PathBuf> {
        self.frame(KIND_REPORT, &serde_json::to_vec(report)?)?;
        let enc = self.enc.take().context("run file already finished")?;
        let mut w = enc.finish()?;
        w.flush()?;
        let file = w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all().ok();
        drop(file);
        std::fs::rename(&self.tmp, &self.path)
            .with_context(|| format!("cannot write {}", self.path.display()))?;
        Ok(self.path.clone())
    }
}

impl Drop for RunFileWriter {
    fn drop(&mut self) {
        if self.enc.is_some() {
            // Not finished: leave nothing half-written.
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Reads a run file block by block. The file is untrusted: a block's declared length is
/// checked against the limit of its kind and against what the file may still decode to before
/// anything is read, and its buffer grows only with the bytes that are really there.
pub struct RunFileReader<R: Read> {
    dec: zstd::stream::Decoder<'static, BufReader<R>>,
    limits: Limits,
    /// Bytes decoded so far (block heads and payloads, skipped blocks included).
    decoded: u64,
    /// The current block's payload (reused from block to block).
    payload: Vec<u8>,
}

impl RunFileReader<std::fs::File> {
    pub fn open(path: &Path) -> Result<Self> {
        let f =
            std::fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
        Self::new(f)
    }
}

impl<R: Read> RunFileReader<R> {
    pub fn new(r: R) -> Result<Self> {
        Self::with_limits(r, Limits::default())
    }

    fn with_limits(r: R, limits: Limits) -> Result<Self> {
        Ok(Self {
            dec: zstd::stream::Decoder::new(r).context("not a zstd stream")?,
            limits,
            decoded: 0,
            payload: Vec::new(),
        })
    }

    /// Count `bytes` more decoded bytes, unless the file would decode to more than it may.
    fn account(&mut self, bytes: u64) -> Result<()> {
        let total = self.decoded.saturating_add(bytes);
        if total > self.limits.decoded_bytes {
            bail!(
                "{TOO_LARGE}: it decompresses to more than {}",
                crate::util::fmt_bytes(self.limits.decoded_bytes)
            );
        }
        self.decoded = total;
        Ok(())
    }

    /// The next block (`None` at the end of the stream).
    pub fn next_block(&mut self) -> Result<Option<Block>> {
        let mut head = [0u8; 5];
        if !read_exact_or_eof(&mut self.dec, &mut head)? {
            return Ok(None);
        }
        let kind = head[0];
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
        let truncated = || {
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
                .context("corrupt run file: truncated block")
        };
        if len > self.limits.block(kind) {
            bail!("corrupt run file: a block of {len} bytes");
        }
        self.account(5 + u64::from(len))?;
        if !matches!(
            kind,
            KIND_HEADER | KIND_SEGMENT | KIND_INTERVAL | KIND_REPORT
        ) {
            // A kind this version does not know: skipped without buffering it.
            let skipped = std::io::copy(
                &mut (&mut self.dec).take(u64::from(len)),
                &mut std::io::sink(),
            )
            .context("corrupt run file")?;
            if skipped < u64::from(len) {
                return Err(truncated());
            }
            return Ok(Some(Block::Unknown(kind)));
        }
        // The length is only what the file says: the buffer grows as the bytes arrive.
        self.payload.clear();
        (&mut self.dec)
            .take(u64::from(len))
            .read_to_end(&mut self.payload)
            .context("corrupt run file")?;
        if self.payload.len() < len as usize {
            return Err(truncated());
        }
        let payload = &self.payload[..];
        let bad = |what: &str, e: serde_json::Error| {
            anyhow::anyhow!("corrupt run file: bad {what} block: {e}")
        };
        Ok(Some(match kind {
            KIND_HEADER => Block::Header(Box::new(
                serde_json::from_slice(payload).map_err(|e| bad("header", e))?,
            )),
            KIND_SEGMENT => Block::Segment(Box::new(
                serde_json::from_slice(payload).map_err(|e| bad("segment", e))?,
            )),
            KIND_INTERVAL => {
                Block::Interval(serde_json::from_slice(payload).map_err(|e| bad("interval", e))?)
            }
            // Straight into its type: a `Value` of the block first would take many times its
            // bytes.
            _ => {
                let report: Report =
                    serde_json::from_slice(payload).map_err(|e| bad("report", e))?;
                if report.schema != crate::model::REPORT_SCHEMA {
                    bail!(
                        "report schema `{}` is not supported; this oprobe reads `{}`",
                        report.schema,
                        crate::model::REPORT_SCHEMA
                    );
                }
                Block::Report(Box::new(report))
            }
        }))
    }
}

/// `read_exact`, but `Ok(false)` on a clean end of stream before the first byte.
fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) if n == 0 => return Ok(false),
            Ok(0) => bail!("corrupt run file: truncated block header"),
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("corrupt run file"),
        }
    }
    Ok(true)
}

/// Write a finished run as `oprobe.run/7` (rows in segments of [`SEGMENT_ROWS`], by time).
pub fn write_run(path: &Path, run: &Run) -> Result<PathBuf> {
    let header = RunHeader::new(run.report.run.id.clone(), run.report.spec.clone());
    let mut w = RunFileWriter::create(path, &header)?;
    for p in &run.report.timeline {
        w.write_interval(&IntervalBlock::of(p))?;
    }
    // Segments are time slices: rows sorted by start time (a run's rows are; others are
    // sorted here).
    if run.ops.is_sorted_by_key(|o| o.t_us) && run.reqs.is_sorted_by_key(|r| r.t_us) {
        w.write_rows(&run.ops, &run.reqs)?;
    } else {
        let mut o = run.ops.clone();
        let mut r = run.reqs.clone();
        o.sort_by_key(|o| (o.t_us, o.op_id));
        r.sort_by_key(|r| (r.t_us, r.op_id));
        w.write_rows(&o, &r)?;
    }
    w.finish(&run.report)
}

/// Rows the in-memory sample keeps while a run streams its rows to the run file (the file
/// holds them all, up to `trace.max_requests`): the report's row-based views use this sample.
pub const STREAM_MEMORY_ROWS: usize = 100_000;

/// Segments queued for the writer thread at most (back-pressure beyond).
const STREAM_QUEUE: usize = 4;

/// Failed and throttled operations are kept in full until their rows are this many times
/// `max_rows`; past that they are thinned as the other rows are past `max_rows`.
const FORCED_ROWS_FACTOR: u64 = 2;

/// Thinning past a cap: levels that each hold up to an eighth of the cap with twice the
/// previous stride (every 2nd, 4th, 8th … operation), so the file keeps covering the whole
/// run while growing only logarithmically with its length.
#[derive(Debug, Default)]
struct Thinning {
    /// The level (stride 2^level) and the rows kept at it.
    level: u32,
    level_rows: u64,
}

impl Thinning {
    /// Whether operation `op_id` (`rows` rows) is kept, and the operations it then stands
    /// for.
    fn pick(&mut self, op_id: u64, rows: u64, max_rows: u64) -> Option<f64> {
        let budget = (max_rows / 8).max(1);
        if self.level_rows + rows > budget {
            self.level += 1;
            self.level_rows = 0;
        }
        let stride = 1u64 << self.level.min(62);
        let pick = op_id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40 & (stride - 1) == 0;
        pick.then(|| {
            self.level_rows += rows;
            stride as f64
        })
    }
}

#[derive(Debug, Default)]
struct StreamState {
    seg: SegmentBuilder,
    rows_kept: u64,
    /// Rows of failed and throttled operations among them.
    forced_rows: u64,
    stats: crate::trace::SamplingStats,
    closed: bool,
    /// Past `max_rows`, and past [`FORCED_ROWS_FACTOR`] times it for the failed and throttled.
    thin: Thinning,
    forced_thin: Thinning,
    /// What the rows kept take of a reader's row budget ([`mem`]).
    row_bytes: u64,
    /// What the segments cut so far take of the bytes the file decodes to, at most.
    cut_json: u64,
    /// Rows were left out because the file had reached what a reader takes.
    full: bool,
}

/// A run file written while the run is in progress (design v5 §2, fixes M3): the recorder
/// hands it every row of the measurement window ([`crate::trace::RowSink`]); rows are packed
/// into columnar segments and a writer thread compresses and appends them, so memory stays
/// bounded by a few segments whatever the run's length.
///
/// **What is kept.** Up to `max_rows` rows, every row. Beyond that the rows are thinned
/// (every 2nd, 4th, … operation, an eighth of the cap per level), with all of an operation's
/// requests or none; failed and throttled operations are kept in full for longer (until
/// their rows are [`FORCED_ROWS_FACTOR`] times the cap), then thinned the same way. A
/// thinned row carries the operations it stands for (its weight).
///
/// **Never more than a reader takes.** The stream counts what its rows cost a reader (the
/// memory of the rows, the bytes the file decodes to) and stops keeping rows before either
/// passes the reader's limit: the file it writes can always be opened.
pub struct RunFileStream {
    path: PathBuf,
    max_rows: u64,
    limits: Limits,
    /// The bytes the segments may decode to (the header and the report have their own room).
    json_budget: u64,
    state: std::sync::Mutex<StreamState>,
    tx: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<Segment>>>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<Result<RunFileWriter>>>>,
}

impl std::fmt::Debug for RunFileStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunFileStream")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl RunFileStream {
    /// Start `path` (through `<path>.tmp`) with its header.
    pub fn create(
        path: &Path,
        header: &RunHeader,
        max_rows: usize,
    ) -> Result<std::sync::Arc<Self>> {
        Self::create_within(path, header, max_rows, Limits::default())
    }

    fn create_within(
        path: &Path,
        header: &RunHeader,
        max_rows: usize,
        limits: Limits,
    ) -> Result<std::sync::Arc<Self>> {
        let mut w = RunFileWriter::create_within(path, header, limits)?;
        let json_budget = w.room();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Segment>(STREAM_QUEUE);
        let thread = std::thread::Builder::new()
            .name("oprobe-runfile".into())
            .spawn(move || -> Result<RunFileWriter> {
                for seg in rx {
                    w.write_segment(&seg)?;
                }
                Ok(w)
            })?;
        Ok(std::sync::Arc::new(Self {
            path: path.to_owned(),
            max_rows: max_rows.max(1) as u64,
            limits,
            json_budget,
            state: Default::default(),
            tx: std::sync::Mutex::new(Some(tx)),
            thread: std::sync::Mutex::new(Some(thread)),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn send(&self, segs: Vec<Segment>) {
        if segs.is_empty() {
            return;
        }
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(tx) = tx {
            for seg in segs {
                // A failed writer reports its error at `finish`.
                let _ = tx.send(seg);
            }
        }
    }

    /// What the file holds: kept / seen operations and requests.
    pub fn sampling(&self) -> crate::trace::SamplingStats {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).stats
    }

    /// The report's `run.trace` and `sampling` describe the file's rows.
    pub fn describe(&self, report: &mut Report) {
        let s = self.sampling();
        report.sampling = crate::model::Sampling::from_stats(&s);
        report.run.trace = crate::model::TraceInfo {
            max_requests_count: self.max_rows,
            ops_seen_count: s.seen_ops,
            ops_kept_count: s.kept_ops,
            requests_seen_count: s.seen_requests,
            requests_kept_count: s.kept_requests,
            sample_ratio: if s.seen_requests == 0 {
                1.0
            } else {
                s.kept_requests as f64 / s.seen_requests as f64
            },
        };
        report.run.sampled = s.kept_ops < s.seen_ops || s.kept_requests < s.seen_requests;
        if self.state.lock().unwrap_or_else(|e| e.into_inner()).full {
            report.warnings.push(format!(
                "the run file is full: it holds the rows of {} of {} operations, the most a run file may hold; the aggregates count every operation",
                s.kept_ops, s.seen_ops
            ));
        }
    }

    /// Write the intervals and the report, close the stream and move the file into place.
    pub fn finish(&self, report: &Report) -> Result<PathBuf> {
        let rest = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.closed = true;
            (!st.seg.is_empty()).then(|| st.seg.finish())
        };
        self.send(rest.into_iter().collect());
        drop(self.tx.lock().unwrap_or_else(|e| e.into_inner()).take());
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .context("the run file was already finished")?;
        let mut w = thread
            .join()
            .map_err(|_| anyhow::anyhow!("the run file writer panicked"))??;
        for p in &report.timeline {
            w.write_interval(&IntervalBlock::of(p))?;
        }
        w.finish(report)
    }

    /// Whether `rows` still fit in a file a reader opens. A row that alone is more than a
    /// segment may hold does not: no segment could carry it.
    fn fits(&self, st: &StreamState, rows: &Cost) -> bool {
        let segment = self.limits.segment_bytes as usize;
        if json::empty().saturating_add(rows.json_max) > segment {
            return false;
        }
        // Every row at its bound, and an empty segment for each cut the rows may cause (by
        // rows or by bytes), the one being filled included.
        let cuts = 2 + rows.count / SEGMENT_ROWS + rows.json / segment;
        let json = (rows.json + cuts * json::empty()) as u64;
        st.row_bytes.saturating_add(rows.mem) <= self.limits.row_bytes
            && st
                .cut_json
                .saturating_add(st.seg.json_bytes() as u64)
                .saturating_add(json)
                <= self.json_budget
    }

    /// The weight operation `op` is kept with (`rows` rows), or `None` when it is thinned
    /// away.
    fn keep(&self, st: &mut StreamState, op: &OpRow, rows: u64) -> Option<f64> {
        if !op.ok || op.throttled {
            if st.forced_rows + rows <= FORCED_ROWS_FACTOR.saturating_mul(self.max_rows) {
                return Some(1.0);
            }
            return st.forced_thin.pick(op.op_id, rows, self.max_rows);
        }
        if st.rows_kept + rows <= self.max_rows {
            return Some(1.0);
        }
        st.thin.pick(op.op_id, rows, self.max_rows)
    }

    /// Start a new segment when the one being filled does not take a row of `row_json`
    /// bytes; the full one goes to `cut`.
    fn make_room(&self, st: &mut StreamState, row_json: usize, cut: &mut Vec<Segment>) {
        if !st.seg.takes(row_json, self.limits.segment_bytes) {
            st.cut_json += st.seg.json_bytes() as u64;
            cut.push(st.seg.finish());
        }
    }
}

/// What rows cost a reader: their count, the memory they take of its row budget ([`mem`])
/// and, at most, the bytes of their JSON ([`json`]: all of them, and the largest one's).
#[derive(Debug, Default)]
struct Cost {
    count: usize,
    mem: u64,
    json: usize,
    json_max: usize,
}

impl Cost {
    fn add(&mut self, mem: usize, json: usize) {
        self.count += 1;
        self.mem += mem as u64;
        self.json += json;
        self.json_max = self.json_max.max(json);
    }
}

/// What an operation row adds to a segment's JSON at most, its strings counted as new.
fn op_json(o: &OpRow) -> usize {
    json::op(o) + json::op_strings(o)
}

/// What a request row adds to a segment's JSON at most, its strings counted as new.
fn req_json(r: &ReqRow) -> usize {
    json::req(r) + json::req_strings(r)
}

impl crate::trace::RowSink for RunFileStream {
    fn on_op(&self, op: &OpRow, reqs: &[ReqRow]) {
        let mut cut = Vec::new();
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.closed {
                return;
            }
            st.stats.seen_ops += 1;
            st.stats.seen_requests += reqs.len() as u64;
            let rows = 1 + reqs.len() as u64;
            let mut cost = Cost::default();
            cost.add(mem::op(op), op_json(op));
            for r in reqs {
                cost.add(mem::req(r), req_json(r));
            }
            if !self.fits(&st, &cost) {
                st.full = true;
                return;
            }
            let Some(weight) = self.keep(&mut st, op, rows) else {
                return;
            };
            if !op.ok || op.throttled {
                st.stats.always_kept_ops += 1;
                st.forced_rows += rows;
            }
            st.stats.kept_ops += 1;
            st.stats.kept_requests += reqs.len() as u64;
            st.rows_kept += rows;
            st.row_bytes += cost.mem;
            self.make_room(&mut st, op_json(op), &mut cut);
            if (weight - 1.0).abs() > f64::EPSILON {
                let mut weighted = op.clone();
                weighted.weight = weight;
                st.seg.push_op(&weighted);
            } else {
                st.seg.push_op(op);
            }
            for r in reqs {
                self.make_room(&mut st, req_json(r), &mut cut);
                st.seg.push_req(r);
            }
        }
        self.send(cut);
    }

    fn on_unscoped(&self, req: &ReqRow) {
        let mut cut = Vec::new();
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.closed {
                return;
            }
            // Seen whether it is kept or not: a request left out makes the rows a sample.
            st.stats.seen_requests += 1;
            if st.rows_kept >= self.max_rows {
                return;
            }
            let mut cost = Cost::default();
            cost.add(mem::req(req), req_json(req));
            if !self.fits(&st, &cost) {
                st.full = true;
                return;
            }
            st.stats.kept_requests += 1;
            st.rows_kept += 1;
            st.row_bytes += cost.mem;
            self.make_room(&mut st, cost.json, &mut cut);
            st.seg.push_req(req);
        }
        self.send(cut);
    }
}

/// Read a whole `oprobe.run/7` file into a [`Run`] (rows sorted by start time).
pub fn read_run(r: impl Read) -> Result<Run> {
    read_run_within(r, Limits::default())
}

fn read_run_within(r: impl Read, limits: Limits) -> Result<Run> {
    let mut rd = RunFileReader::with_limits(r, limits)?;
    let mut budget = RowBudget::new(limits.row_bytes);
    let mut header = None;
    let mut report = None;
    let mut ops = Vec::new();
    let mut reqs = Vec::new();
    loop {
        let b = match rd.next_block() {
            Ok(Some(b)) => b,
            Ok(None) => break,
            // Nothing readable before the header: not a run file (another zstd file, random
            // bytes), unless the system failed the read or the file ends early (a truncated
            // run file is damaged).
            Err(e)
                if header.is_none()
                    && !e
                        .chain()
                        .filter_map(|c| c.downcast_ref::<std::io::Error>())
                        .any(|io| {
                            io.raw_os_error().is_some()
                                || io.kind() == std::io::ErrorKind::UnexpectedEof
                        }) =>
            {
                bail!("not an oprobe run file ({e:#})")
            }
            Err(e) => return Err(e),
        };
        match b {
            // A run file starts with its header.
            Block::Segment(_) | Block::Interval(_) | Block::Report(_) | Block::Unknown(_)
                if header.is_none() =>
            {
                bail!("not an oprobe run file (no header)")
            }
            Block::Header(h) => {
                if h.format != RUN_FORMAT {
                    bail!(
                        "run file format `{}` is not supported; this oprobe reads `{RUN_FORMAT}`",
                        h.format
                    );
                }
                header = Some(h);
            }
            Block::Segment(s) => {
                let (o, q) = s.decode(&mut budget)?;
                ops.extend(o);
                reqs.extend(q);
            }
            Block::Report(r) => report = Some(r),
            Block::Interval(_) | Block::Unknown(_) => {}
        }
    }
    if header.is_none() {
        bail!("not an oprobe run file (no header)");
    }
    let report = report.context("the run file has no report (the run did not finish)")?;
    ops.sort_by_key(|o| (o.t_us, o.op_id));
    reqs.sort_by_key(|r| (r.t_us, r.op_id));
    Ok(Run {
        report: *report,
        ops,
        reqs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Run {
        let mut run = crate::report::fixture::run();
        // Make sure several segments are written.
        let base_ops = run.ops.clone();
        let base_reqs = run.reqs.clone();
        for k in 1..40i64 {
            for o in &base_ops {
                let mut o = o.clone();
                o.op_id += k as u64 * 1_000_000;
                o.t_us += k * 10_000_000;
                run.ops.push(o);
            }
            for r in &base_reqs {
                let mut r = r.clone();
                r.op_id += k as u64 * 1_000_000;
                r.t_us += k * 10_000_000;
                run.reqs.push(r);
            }
        }
        run.ops.sort_by_key(|o| (o.t_us, o.op_id));
        run.reqs.sort_by_key(|r| (r.t_us, r.op_id));
        run
    }

    #[test]
    fn segments_round_trip_every_field() {
        let run = crate::report::fixture::run();
        let s = Segment::encode(&run.ops, &run.reqs);
        let (ops, reqs) = s.decode(&mut RowBudget::default()).unwrap();
        assert_eq!(ops, run.ops);
        assert_eq!(reqs, run.reqs);
    }

    #[test]
    fn a_run_file_streams_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.oprobe");
        let run = rows();
        write_run(&path, &run).unwrap();
        assert!(!path.with_extension("oprobe.tmp").exists());
        let back = read_run(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(back.ops.len(), run.ops.len());
        assert_eq!(back.reqs.len(), run.reqs.len());
        assert_eq!(back, run);
        // Block by block: header first, report last.
        let mut rd = RunFileReader::open(&path).unwrap();
        let mut kinds = Vec::new();
        while let Some(b) = rd.next_block().unwrap() {
            kinds.push(match b {
                Block::Header(_) => 'h',
                Block::Segment(_) => 's',
                Block::Interval(_) => 'i',
                Block::Report(_) => 'r',
                Block::Unknown(_) => '?',
            });
        }
        assert_eq!(kinds.first(), Some(&'h'));
        assert_eq!(kinds.last(), Some(&'r'));
        assert!(kinds.contains(&'s'));
    }

    #[test]
    fn an_unfinished_writer_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.oprobe");
        let run = crate::report::fixture::run();
        let header = RunHeader::new("x", run.report.spec.clone());
        {
            let mut w = RunFileWriter::create(&path, &header).unwrap();
            w.write_rows(&run.ops, &run.reqs).unwrap();
            assert_eq!(w.rows_written().0, run.ops.len() as u64);
        }
        assert!(!path.exists());
        assert!(!path.with_extension("oprobe.tmp").exists());
    }

    #[test]
    fn a_streamed_run_file_holds_every_row_and_thins_past_its_cap() {
        use crate::trace::RowSink;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.oprobe");
        let base = crate::report::fixture::run();
        let header = RunHeader::new("x", base.report.spec.clone());
        let op0 = base.ops[0].clone();
        let req0 = base
            .requests_of(op0.op_id)
            .into_iter()
            .next()
            .unwrap()
            .clone();
        // Rows of 2 (op + request); a cap of 20_000 rows holds 10_000 operations.
        let st = RunFileStream::create(&path, &header, 20_000).unwrap();
        for i in 0..30_000u64 {
            let mut o = op0.clone();
            o.op_id = i + 1;
            o.t_us = i as i64 * 100;
            o.ok = i % 1000 != 7;
            let mut r = req0.clone();
            r.op_id = o.op_id;
            r.t_us = o.t_us;
            st.on_op(&o, std::slice::from_ref(&r));
        }
        let s = st.sampling();
        assert_eq!(s.seen_ops, 30_000);
        assert!(s.kept_ops >= 10_000 && s.kept_ops < 20_000, "{s:?}");
        assert_eq!(s.kept_requests, s.kept_ops);
        let mut report = base.report.clone();
        st.describe(&mut report);
        assert!(report.run.sampled);
        assert_eq!(report.sampling.seen_ops_count, 30_000);
        let out = st.finish(&report).unwrap();
        assert_eq!(out, path);
        assert!(!path.with_extension("oprobe.tmp").exists());
        let back = read_run(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(back.ops.len() as u64, s.kept_ops);
        assert_eq!(back.reqs.len() as u64, s.kept_requests);
        // Every failed operation was kept, past the cap too.
        assert_eq!(back.ops.iter().filter(|o| !o.ok).count(), 30);
        // The first 10_000 operations are all there, with their requests.
        assert!((1..=10_000).all(|id| back.op(id).is_some()));
        assert_eq!(back.report.sampling, report.sampling);
        let mut rd = RunFileReader::open(&path).unwrap();
        let mut intervals = 0;
        while let Some(b) = rd.next_block().unwrap() {
            if let Block::Interval(i) = b {
                assert!(i.len_s > 0.0);
                intervals += 1;
            }
        }
        assert_eq!(intervals, report.timeline.len());
    }

    /// A run file's bytes: `blocks` as `(kind, declared length, body)`, zstd-compressed.
    fn stream(blocks: &[(u8, u32, &[u8])]) -> Vec<u8> {
        let mut raw = Vec::new();
        for (kind, len, body) in blocks {
            raw.push(*kind);
            raw.extend_from_slice(&len.to_le_bytes());
            raw.extend_from_slice(body);
        }
        zstd::encode_all(&raw[..], ZSTD_LEVEL).unwrap()
    }

    fn header_json() -> Vec<u8> {
        let run = crate::report::fixture::run();
        serde_json::to_vec(&RunHeader::new("x", run.report.spec)).unwrap()
    }

    /// The classified error of a refused file, as `show FILE` and the app report it.
    fn refused(bytes: &[u8]) -> crate::errors::ErrorBody {
        let e = crate::store::runs::decode_run(bytes).unwrap_err();
        let b = crate::errors::classify(&e, "s3");
        assert_eq!(
            b.code,
            crate::errors::ErrorCode::UsageInvalidArgument,
            "{e:#}"
        );
        assert!(b.hint.is_some(), "{e:#}");
        b
    }

    /// 9.1.0 allocated a block's declared length (up to 1 GiB, zeroed) before reading it: a
    /// 60-byte file took a gigabyte. The buffer now grows with the bytes that arrive.
    #[test]
    fn a_declared_length_is_not_allocated_before_its_bytes_arrive() {
        let header = header_json();
        let h = (KIND_HEADER, header.len() as u32, &header[..]);
        // The largest lengths the limits allow, with ten bytes behind them.
        for (kind, declared) in [
            (KIND_SEGMENT, MAX_SEGMENT_BYTES),
            (KIND_REPORT, MAX_BLOCK_BYTES),
            (KIND_INTERVAL, MAX_BLOCK_BYTES),
            // An unknown kind is skipped: nothing of it is buffered at all.
            (9, MAX_SEGMENT_BYTES),
        ] {
            let bytes = stream(&[h, (kind, declared, b"0123456789")]);
            let mut rd = RunFileReader::new(&bytes[..]).unwrap();
            assert!(matches!(rd.next_block(), Ok(Some(Block::Header(_)))));
            let e = rd.next_block().unwrap_err();
            assert!(format!("{e:#}").contains("truncated block"), "{e:#}");
            assert!(
                rd.payload.capacity() <= 64 << 10,
                "kind {kind}: {} bytes buffered for a 10-byte block",
                rd.payload.capacity()
            );
            let b = refused(&bytes);
            assert!(b.hint.unwrap().contains("damaged or truncated"));
        }
        // A length over its kind's limit is refused before anything is read.
        for (kind, declared) in [
            (KIND_SEGMENT, MAX_SEGMENT_BYTES + 1),
            (KIND_SEGMENT, u32::MAX),
            (KIND_HEADER, MAX_BLOCK_BYTES + 1),
            (KIND_REPORT, u32::MAX),
            (9, MAX_SEGMENT_BYTES + 1),
        ] {
            let bytes = stream(&[h, (kind, declared, b"0123456789")]);
            let mut rd = RunFileReader::new(&bytes[..]).unwrap();
            rd.next_block().unwrap();
            let e = rd.next_block().unwrap_err();
            assert!(
                format!("{e:#}").contains(&format!("a block of {declared} bytes")),
                "{e:#}"
            );
            assert_eq!(
                rd.payload.capacity(),
                header.len().max(rd.payload.capacity())
            );
            assert!(rd.payload.capacity() <= 64 << 10);
            refused(&bytes);
        }
        // The very first block: not a run file, whatever it declares.
        let b = refused(&stream(&[(KIND_HEADER, u32::MAX, b"{}")]));
        assert!(
            b.message.contains("not an oprobe run file"),
            "{}",
            b.message
        );
    }

    /// A zstd bomb: a few kilobytes that decompress to gigabytes of blocks. 9.1.0 decoded
    /// them all (and buffered each); the reader stops at what a file may decode to.
    #[test]
    fn a_zstd_bomb_is_refused_at_the_decoded_limit() {
        let header = header_json();
        let zeros = vec![0u8; 1 << 20];
        let limits = Limits {
            decoded_bytes: 8 << 20,
            ..Default::default()
        };
        // 1 GiB of skipped blocks, then as much again of blocks that are buffered.
        for kind in [9, KIND_INTERVAL] {
            let mut blocks = vec![(KIND_HEADER, header.len() as u32, &header[..])];
            blocks.extend((0..1024).map(|_| (kind, zeros.len() as u32, &zeros[..])));
            let bytes = stream(&blocks);
            assert!(bytes.len() < 200 << 10, "{} bytes", bytes.len());
            let mut rd = RunFileReader::with_limits(&bytes[..], limits).unwrap();
            let e = loop {
                match rd.next_block() {
                    Ok(Some(Block::Header(_) | Block::Unknown(_))) => {}
                    Ok(other) => panic!("{other:?}"),
                    Err(e) if kind == KIND_INTERVAL && rd.decoded < 2 << 20 => {
                        // A megabyte of zeros is not an interval: it was buffered within
                        // its limit, then refused as it is.
                        assert!(format!("{e:#}").contains("bad interval block"), "{e:#}");
                        assert!(rd.payload.capacity() <= 4 << 20);
                        break None;
                    }
                    Err(e) => break Some(e),
                }
            };
            assert!(rd.decoded <= limits.decoded_bytes, "{}", rd.decoded);
            if kind == 9 {
                let e = e.unwrap();
                assert!(format!("{e:#}").contains(TOO_LARGE), "{e:#}");
                assert_eq!(
                    rd.payload.capacity(),
                    header.len().max(rd.payload.capacity())
                );
                assert!(rd.payload.capacity() <= 64 << 10);
                let e = read_run_within(&bytes[..], limits).unwrap_err();
                assert!(format!("{e:#}").contains(TOO_LARGE), "{e:#}");
            }
        }
    }

    /// The format counts nothing: a stream of empty blocks without end must end by itself.
    #[test]
    fn a_stream_of_empty_blocks_ends_at_the_decoded_limit() {
        let header = header_json();
        let mut blocks = vec![(KIND_HEADER, header.len() as u32, &header[..])];
        blocks.extend((0..1_000_000).map(|_| (9u8, 0u32, &b""[..])));
        let bytes = stream(&blocks);
        let limits = Limits {
            decoded_bytes: 1 << 20,
            ..Default::default()
        };
        let mut rd = RunFileReader::with_limits(&bytes[..], limits).unwrap();
        let mut n = 0u64;
        let e = loop {
            match rd.next_block() {
                Ok(Some(_)) => n += 1,
                Ok(None) => panic!("the stream was read to its end"),
                Err(e) => break e,
            }
        };
        assert!(format!("{e:#}").contains(TOO_LARGE), "{e:#}");
        // Five bytes a block: the limit is what stops it.
        assert!(n <= (1 << 20) / 5 + 1, "{n}");
        assert!(rd.decoded <= limits.decoded_bytes);
    }

    /// A segment interns its strings: one long key named by every row is copied into each of
    /// them, so a megabyte of file decodes to any amount of memory. The rows are paid for out
    /// of a budget before they are built.
    #[test]
    fn rows_that_multiply_an_interned_string_are_refused_within_the_row_budget() {
        let base = crate::report::fixture::run();
        let n = 60_000;
        let ops: Vec<OpRow> = (0..n)
            .map(|i| OpRow {
                op_id: i,
                ..base.ops[0].clone()
            })
            .collect();
        let mut seg = Segment::encode(&ops, &[]);
        let key = seg.ops.key[0] as usize;
        seg.strings[key] = "k".repeat(1 << 20);
        // 60 000 rows of a 1 MiB key: 60 GiB of rows from a segment of a few MiB.
        let mut budget = RowBudget::default();
        let e = seg.decode(&mut budget).unwrap_err();
        assert!(format!("{e:#}").contains(TOO_LARGE), "{e:#}");
        assert!(budget.left < 1 << 20, "{}", budget.left);
        // As a file: refused, classified, with what to do.
        let header = header_json();
        let body = serde_json::to_vec(&seg).unwrap();
        let bytes = stream(&[
            (KIND_HEADER, header.len() as u32, &header[..]),
            (KIND_SEGMENT, body.len() as u32, &body[..]),
        ]);
        assert!(bytes.len() < 1 << 20, "{} bytes", bytes.len());
        let b = refused(&bytes);
        assert!(b.message.contains(TOO_LARGE), "{}", b.message);
        // No setting makes the file readable (9.2 said to lower `max_requests`).
        let hint = b.hint.unwrap();
        assert!(hint.contains("run the workload again"), "{hint}");
        assert!(!hint.contains("max_requests"), "{hint}");
        // A budget that holds the rows as they really are decodes them.
        seg.strings[key] = "k".into();
        let mut budget = RowBudget::new(n * (std::mem::size_of::<OpRow>() as u64 + 1));
        assert_eq!(seg.decode(&mut budget).unwrap().0.len() as u64, n);
        assert_eq!(budget.left, 0);
    }

    /// 9.1.0 leaked every distinct verb, method and service operation name of a file for the
    /// life of the process (`Box::leak`, the first 4 096 of them): the app opens many files.
    /// Names oprobe knows are borrowed; any other name is owned by its row and goes with it.
    #[test]
    fn names_of_a_file_are_owned_by_its_rows_not_leaked() {
        assert!(STATIC_NAMES.is_sorted(), "binary search needs them sorted");
        let base = crate::report::fixture::run();
        let n = 10_000usize;
        let ops: Vec<OpRow> = (0..n)
            .map(|i| OpRow {
                op_id: i as u64,
                verb: Cow::Owned(format!("verb-{i}")),
                ..base.ops[0].clone()
            })
            .collect();
        let reqs: Vec<ReqRow> = (0..n)
            .map(|i| ReqRow {
                op_id: i as u64,
                method: Cow::Owned(format!("METHOD-{i}")),
                service_op: Some(Cow::Owned(format!("Operation{i}"))),
                ..base.reqs[0].clone()
            })
            .collect();
        let seg = Segment::encode(&ops, &reqs);
        let mut budget = RowBudget::default();
        let (o, r) = seg.decode(&mut budget).unwrap();
        assert_eq!((o.len(), r.len()), (n, n));
        assert!(o.iter().all(|o| matches!(o.verb, Cow::Owned(_))));
        assert!(r.iter().all(|r| matches!(r.method, Cow::Owned(_))));
        assert!(
            r.iter()
                .all(|r| matches!(r.service_op, Some(Cow::Owned(_))))
        );
        assert_eq!((o, r), (ops, reqs));
        // What oprobe itself records is borrowed: no allocation per row for it.
        let run = crate::report::fixture::run();
        let (o, r) = Segment::encode(&run.ops, &run.reqs)
            .decode(&mut RowBudget::default())
            .unwrap();
        assert!(o.iter().all(|o| matches!(o.verb, Cow::Borrowed(_))));
        assert!(r.iter().all(|r| matches!(r.method, Cow::Borrowed(_))));
        for name in [
            "read.full",
            "write.multipart",
            "GET",
            "DELETE",
            "ListObjects",
        ] {
            assert!(matches!(static_name(name).0, Cow::Borrowed(_)), "{name}");
        }
    }

    /// A run file exactly at every limit reads back whole; one byte less of any of them
    /// refuses it.
    #[test]
    fn a_run_file_at_the_limits_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.oprobe");
        let run = rows();
        write_run(&path, &run).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        // What this file takes of each limit.
        let mut rd = RunFileReader::new(&bytes[..]).unwrap();
        let mut budget = RowBudget::default();
        let (mut segment, mut block, mut before) = (0u64, 0u64, 0u64);
        while let Some(b) = rd.next_block().unwrap() {
            let len = rd.decoded - before - 5;
            before = rd.decoded;
            match b {
                Block::Segment(s) => {
                    segment = segment.max(len);
                    s.decode(&mut budget).unwrap();
                }
                _ => block = block.max(len),
            }
        }
        let exact = Limits {
            segment_bytes: segment as u32,
            block_bytes: block as u32,
            decoded_bytes: rd.decoded,
            row_bytes: MAX_ROW_BYTES - budget.left,
        };
        assert!(exact.segment_bytes > 0 && exact.block_bytes > 0 && exact.row_bytes > 0);
        assert_eq!(read_run_within(&bytes[..], exact).unwrap(), run);
        for (what, limits) in [
            (
                "a block of",
                Limits {
                    segment_bytes: exact.segment_bytes - 1,
                    ..exact
                },
            ),
            (
                "a block of",
                Limits {
                    block_bytes: exact.block_bytes - 1,
                    ..exact
                },
            ),
            (
                TOO_LARGE,
                Limits {
                    decoded_bytes: exact.decoded_bytes - 1,
                    ..exact
                },
            ),
            (
                TOO_LARGE,
                Limits {
                    row_bytes: exact.row_bytes - 1,
                    ..exact
                },
            ),
        ] {
            let e = read_run_within(&bytes[..], limits).unwrap_err();
            assert!(format!("{e:#}").contains(what), "{limits:?}: {e:#}");
        }
        // The writer never writes a block the reader refuses.
        let header = RunHeader::new("x", run.report.spec.clone());
        let mut w = RunFileWriter::create(&dir.path().join("big.oprobe"), &header).unwrap();
        let mut big = Segment::encode(&run.ops[..1], &[]);
        big.strings.push("x".repeat(MAX_SEGMENT_BYTES as usize));
        let e = w.write_segment(&big).unwrap_err();
        assert!(format!("{e:#}").contains("block too large"), "{e:#}");
    }

    /// The default limits against what rows take (the fixture's: about 100 bytes of JSON and
    /// 270 of memory each; a lab run's are twice that): they leave room for runs kept at many
    /// times the default cap of a million rows.
    #[test]
    fn the_limits_are_generous_for_real_runs() {
        let run = rows();
        let rows_count = (run.ops.len() + run.reqs.len()) as u64;
        let seg = Segment::encode(&run.ops, &run.reqs);
        let json = serde_json::to_vec(&seg).unwrap().len() as u64;
        let mut budget = RowBudget::default();
        seg.decode(&mut budget).unwrap();
        let memory = MAX_ROW_BYTES - budget.left;
        let default_cap = crate::trace::DEFAULT_MAX_REQUESTS as u64;
        assert!(
            MAX_DECODED_BYTES / (json / rows_count) >= 10 * default_cap,
            "{} bytes of JSON a row",
            json / rows_count
        );
        assert!(
            MAX_ROW_BYTES / (memory / rows_count) >= 10 * default_cap,
            "{} bytes of memory a row",
            memory / rows_count
        );
        // A segment of SEGMENT_ROWS rows, 4 KiB a row; the fixture's take a fortieth of that.
        assert!(u64::from(MAX_SEGMENT_BYTES) >= SEGMENT_ROWS as u64 * 4 * 1024);
        assert!(json / rows_count * SEGMENT_ROWS as u64 * 8 <= u64::from(MAX_SEGMENT_BYTES));
    }

    /// Rows that fill every column, the sparse ones included.
    fn full_rows(n: u64) -> (Vec<OpRow>, Vec<ReqRow>) {
        let base = crate::report::fixture::run();
        let ops = (0..n)
            .map(|i| OpRow {
                op_id: i + 1,
                t_us: i as i64 * 10,
                ok: false,
                error_code: Some(FailureCode::StorageRangeNotSatisfied),
                error: Some(format!("failed \"{i}\"\n\u{1}")),
                await_tree: Some("root\n  child".into()),
                weight: 2.5,
                ..base.ops[0].clone()
            })
            .collect();
        let reqs = (0..n)
            .map(|i| ReqRow {
                op_id: i + 1,
                t_us: i as i64 * 10,
                service_op: Some(Cow::Borrowed("GetObject")),
                remote: Some("[2001:db8::1%7]:8443".parse().unwrap()),
                error_code: Some(FailureCode::StorageThrottled),
                error: Some("SlowDown".into()),
                retry_after_ms: Some(1_500),
                tls: Some(Box::new(TlsRow {
                    version: "TLSv1.3".into(),
                    cipher: Some("TLS13_AES_128_GCM_SHA256".into()),
                    ..Default::default()
                })),
                tcp: Some(Box::new(TcpRow {
                    rtt_us: 1_200,
                    ..Default::default()
                })),
                detail: Some(Box::new(ReqDetail {
                    error_code: Some("SlowDown".into()),
                    ..Default::default()
                })),
                ..base.reqs[0].clone()
            })
            .collect();
        (ops, reqs)
    }

    thread_local! {
        /// Values [`Counted`] built on this thread.
        static BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// A column entry that counts how many of it a decoder builds.
    #[derive(Debug)]
    struct Counted;

    impl<'de> Deserialize<'de> for Counted {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
            u8::deserialize(d)?;
            BUILT.with(|n| n.set(n.get() + 1));
            Ok(Counted)
        }
    }

    /// K1-6. 9.2 built a segment whole before it charged the row budget: `[0,{}],` is seven
    /// bytes of JSON and some three hundred of struct, so a 256 MiB block allocated about
    /// 12 GiB before it was refused. A column is capped while it is deserialised: the decoder
    /// builds one entry more than the cap and stops, however many the file writes.
    #[test]
    fn a_column_is_capped_while_it_is_deserialised() {
        let entries = 10 * MAX_SEGMENT_ROWS;
        let json = format!("[{}]", vec!["0"; entries].join(","));
        BUILT.with(|n| n.set(0));
        let mut de = serde_json::Deserializer::from_str(&json);
        let e = column::<_, Counted>(&mut de).unwrap_err();
        assert!(e.to_string().contains("entries in one column"), "{e}");
        assert_eq!(BUILT.with(|n| n.get()), MAX_SEGMENT_ROWS + 1);
        // A column at the cap is read whole.
        let json = format!("[{}]", vec!["0"; MAX_SEGMENT_ROWS].join(","));
        let mut de = serde_json::Deserializer::from_str(&json);
        assert_eq!(
            column::<_, Counted>(&mut de).unwrap().len(),
            MAX_SEGMENT_ROWS
        );

        // Every column of a segment has the cap (a column added without it fails here), and
        // so has the string table.
        let (ops, reqs) = full_rows(2);
        let seg = serde_json::to_value(Segment::encode(&ops, &reqs)).unwrap();
        let mut columns = 0;
        for group in ["ops", "reqs"] {
            for (name, col) in seg[group].as_object().unwrap() {
                let first = col
                    .get(0)
                    .unwrap_or_else(|| panic!("{group}.{name} is empty"));
                for (n, refused) in [(MAX_SEGMENT_ROWS, false), (MAX_SEGMENT_ROWS + 1, true)] {
                    let mut big = seg.clone();
                    big[group][name] = serde_json::Value::Array(vec![first.clone(); n]);
                    let r = serde_json::from_value::<Segment>(big);
                    assert_eq!(r.is_err(), refused, "{group}.{name} with {n} entries");
                    if let Err(e) = r {
                        assert!(e.to_string().contains("entries in one column"), "{e}");
                    }
                }
                columns += 1;
            }
        }
        assert_eq!(columns, 15 + 21, "every column of both kinds was filled");
        let mut big = seg.clone();
        big["strings"] = serde_json::Value::Array(vec!["s".into(); MAX_SEGMENT_STRINGS + 1]);
        let e = serde_json::from_value::<Segment>(big).unwrap_err();
        assert!(e.to_string().contains("entries in one column"), "{e}");
    }

    /// The crafted file of K1-6, as `show FILE` and the app meet it: a small file whose one
    /// segment repeats `[0,{}],` is refused as damaged, having built no more than the cap.
    #[test]
    fn a_crafted_segment_is_refused_before_it_expands() {
        let (ops, reqs) = full_rows(1);
        let mut seg = serde_json::to_value(Segment::encode(&ops, &reqs)).unwrap();
        let entry = serde_json::json!([0, {}]);
        assert!(serde_json::from_value::<(u32, ReqDetail)>(entry.clone()).is_ok());
        seg["reqs"]["detail"] = serde_json::Value::Array(vec![entry; 8 * MAX_SEGMENT_ROWS]);
        let body = serde_json::to_vec(&seg).unwrap();
        let header = header_json();
        let bytes = stream(&[
            (KIND_HEADER, header.len() as u32, &header[..]),
            (KIND_SEGMENT, body.len() as u32, &body[..]),
        ]);
        assert!(bytes.len() < 64 << 10, "{} bytes", bytes.len());
        let b = refused(&bytes);
        assert!(b.message.contains("entries in one column"), "{}", b.message);
        assert!(b.hint.unwrap().contains("damaged or truncated"));
    }

    /// The report block is parsed into its type; one of another report version says so.
    #[test]
    fn a_report_block_of_another_version_is_refused() {
        let run = crate::report::fixture::run();
        let header = header_json();
        let mut report = serde_json::to_value(&run.report).unwrap();
        let good = serde_json::to_vec(&report).unwrap();
        let bytes = stream(&[
            (KIND_HEADER, header.len() as u32, &header[..]),
            (KIND_REPORT, good.len() as u32, &good[..]),
        ]);
        assert_eq!(read_run(&bytes[..]).unwrap().report, run.report);
        report["schema"] = "oprobe.report/6".into();
        let old = serde_json::to_vec(&report).unwrap();
        let bytes = stream(&[
            (KIND_HEADER, header.len() as u32, &header[..]),
            (KIND_REPORT, old.len() as u32, &old[..]),
        ]);
        let b = refused(&bytes);
        assert!(
            b.message.contains("`oprobe.report/6` is not supported"),
            "{}",
            b.message
        );
    }

    /// A segment's times are its base plus a `u32`: a base no run has is a damaged file, not
    /// an overflow (a panic in a debug build of 9.2, a wrapped time in a release build).
    #[test]
    fn a_rows_time_out_of_range_is_a_damaged_file() {
        let run = crate::report::fixture::run();
        let mut seg = Segment::encode(&run.ops, &run.reqs);
        seg.base_us = i64::MAX - 1;
        let e = seg.decode(&mut RowBudget::default()).unwrap_err();
        assert!(format!("{e:#}").contains("corrupt run file"), "{e:#}");
        let header = header_json();
        let body = serde_json::to_vec(&seg).unwrap();
        let b = refused(&stream(&[
            (KIND_HEADER, header.len() as u32, &header[..]),
            (KIND_SEGMENT, body.len() as u32, &body[..]),
        ]));
        assert!(b.hint.unwrap().contains("damaged or truncated"));
    }

    /// One operation of every kind the engine runs. The `match` has no wildcard: a new kind
    /// does not compile until it is listed, and then its verb is checked below.
    fn every_kind() -> Vec<crate::config::OpKind> {
        use crate::config::OpKind as K;
        use crate::util::ByteSize;
        let size = ByteSize(1);
        let kinds = vec![
            K::Read { size: None },
            K::Read { size: Some(size) },
            K::ParquetScan {
                columns: 1,
                column_size: size,
                footer_size: size,
                suffix: false,
            },
            K::ParquetScan {
                columns: 1,
                column_size: size,
                footer_size: size,
                suffix: true,
            },
            K::Write { size },
            K::MultipartWrite { size },
            K::Stat,
            K::List { recursive: false },
            K::ListPaged { limit: 1 },
            K::Delete { batch: 1 },
            K::Copy,
            K::PresignedRead,
            K::ConditionalWrite { size },
            K::MetadataWrite { size, entries: 1 },
        ];
        let listed: std::collections::BTreeSet<u8> = kinds
            .iter()
            .map(|k| match k {
                K::Read { .. } => 0,
                K::ParquetScan { .. } => 1,
                K::Write { .. } => 2,
                K::MultipartWrite { .. } => 3,
                K::Stat => 4,
                K::List { .. } => 5,
                K::ListPaged { .. } => 6,
                K::Delete { .. } => 7,
                K::Copy => 8,
                K::PresignedRead => 9,
                K::ConditionalWrite { .. } => 10,
                K::MetadataWrite { .. } => 11,
            })
            .collect();
        assert_eq!(
            listed.len(),
            12,
            "a kind of the match above has no operation here"
        );
        kinds
    }

    /// `STATIC_NAMES` is a copy of the verbs the engine records (`engine::ops::verb`), the
    /// HTTP methods and (through `Api::ALL`) the API labels: every one of them is known, so a
    /// row never owns a copy of a name oprobe itself wrote.
    #[test]
    fn every_verb_method_and_api_label_is_a_static_name() {
        assert!(STATIC_NAMES.is_sorted(), "binary search needs them sorted");
        let verbs: std::collections::BTreeSet<&str> =
            every_kind().iter().map(crate::engine::ops::verb).collect();
        assert_eq!(verbs.len(), 14, "{verbs:?}");
        let methods = ["DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT"];
        let labels = crate::trace::Api::ALL.iter().map(|a| a.label());
        for name in verbs.iter().copied().chain(methods).chain(labels) {
            assert_eq!(known_name(name), Some(name));
            assert!(matches!(static_name(name), (Cow::Borrowed(_), 0)), "{name}");
        }
        // Nothing else is in the table: a name the engine no longer records would stay.
        let known: std::collections::BTreeSet<&str> = verbs.into_iter().chain(methods).collect();
        assert_eq!(known, STATIC_NAMES.into_iter().collect());
    }

    /// What a builder says its segment serialises to is never less than what it does, for
    /// ordinary rows and for the largest a row can be, and what [`mem`] says the rows cost a
    /// reader is exactly what the reader's budget pays.
    #[test]
    fn a_builder_bounds_its_segments_json_and_counts_its_rows_as_a_reader_does() {
        let run = rows();
        let (full_ops, full_reqs) = full_rows(50);
        let extreme_op = OpRow {
            op_id: u64::MAX,
            verb: Cow::Owned("\u{1}\u{2}\"\\".repeat(40)),
            key: "\u{0}".repeat(300),
            t_us: i64::MIN,
            latency_us: u64::MAX,
            bytes: u64::MAX,
            attempts: u32::MAX,
            requests: u32::MAX,
            breakdown: OpBreakdownUs {
                queue_us: u64::MAX,
                cache_us: u64::MAX,
                backend_us: u64::MAX,
                retry_us: u64::MAX,
                client_us: u64::MAX,
            },
            weight: -1.234_567_890_123_456_7e-300,
            ..full_ops[0].clone()
        };
        let extreme_reqs: Vec<ReqRow> = crate::trace::Api::ALL
            .iter()
            .zip(
                [WireProtocol::H1, WireProtocol::H2, WireProtocol::Inproc]
                    .iter()
                    .cycle(),
            )
            .map(|(api, protocol)| ReqRow {
                t_us: i64::MAX,
                op_id: u64::MAX,
                attempt: u32::MAX,
                api: *api,
                method: Cow::Owned("\u{7}".repeat(20)),
                status: u16::MAX,
                protocol: *protocol,
                conn_id: u64::MAX,
                remote: Some(
                    "[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff%4294967295]:65535"
                        .parse()
                        .unwrap(),
                ),
                queue_us: u64::MAX,
                dns_us: u64::MAX,
                tcp_us: u64::MAX,
                proxy_us: u64::MAX,
                tls_us: u64::MAX,
                send_us: u64::MAX,
                ttfb_us: u64::MAX,
                body_us: u64::MAX,
                stall_us: u64::MAX,
                total_us: u64::MAX,
                bytes_up: u64::MAX,
                bytes_down: u64::MAX,
                retry_after_ms: Some(u64::MAX),
                request_id: Some("\u{1f}".repeat(64)),
                ..full_reqs[0].clone()
            })
            .collect();
        for (ops, reqs) in [
            (&run.ops[..], &run.reqs[..]),
            (&full_ops[..], &full_reqs[..]),
            (std::slice::from_ref(&extreme_op), &extreme_reqs[..]),
            (&[][..], &extreme_reqs[..1]),
        ] {
            let mut b = SegmentBuilder::default();
            let mut row_bounds = 0;
            for o in ops {
                row_bounds += json::op(o) + json::op_strings(o);
                b.push_op(o);
            }
            for r in reqs {
                row_bounds += json::req(r) + json::req_strings(r);
                b.push_req(r);
            }
            let bound = b.json_bytes();
            let seg = b.finish();
            let real = serde_json::to_vec(&seg).unwrap().len() + 5;
            assert!(real <= bound, "{real} bytes of JSON, bound {bound}");
            // A row with every string new is bounded by itself.
            assert!(bound <= json::empty() + row_bounds);
            let mut budget = RowBudget::default();
            let (o, r) = seg.decode(&mut budget).unwrap();
            let paid = MAX_ROW_BYTES - budget.left;
            let counted: usize =
                o.iter().map(mem::op).sum::<usize>() + r.iter().map(mem::req).sum::<usize>();
            assert_eq!(paid, counted as u64);
        }
    }

    /// The blocks of a run file: `(kind, payload bytes, operations, requests)` of each.
    fn blocks(bytes: &[u8], limits: Limits) -> Vec<(char, u64, usize, usize)> {
        let mut rd = RunFileReader::with_limits(bytes, limits).unwrap();
        let (mut out, mut before) = (Vec::new(), 0);
        while let Some(b) = rd.next_block().unwrap() {
            let len = rd.decoded - before - 5;
            before = rd.decoded;
            out.push(match b {
                Block::Header(_) => ('h', len, 0, 0),
                Block::Segment(s) => ('s', len, s.ops_count(), s.requests_count()),
                Block::Interval(_) => ('i', len, 0, 0),
                Block::Report(_) => ('r', len, 0, 0),
                Block::Unknown(_) => ('?', len, 0, 0),
            });
        }
        out
    }

    /// The writer cuts a segment where the reader stops taking one: at [`SEGMENT_ROWS`] rows
    /// (9.2 wrote that many of each kind) and before its JSON passes the segment limit (9.2
    /// failed the save, "run file block too large", once rows with long keys or errors took a
    /// segment past it).
    #[test]
    fn a_writer_cuts_segments_at_the_rows_and_bytes_a_reader_takes() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ops, mut reqs) = full_rows(3 * SEGMENT_ROWS as u64 / 2);
        let header = RunHeader::new("x", crate::report::fixture::run().report.spec);
        let report = crate::report::fixture::report();
        // By rows.
        let path = dir.path().join("rows.oprobe");
        let mut w = RunFileWriter::create(&path, &header).unwrap();
        w.write_rows(&ops, &reqs).unwrap();
        w.finish(&report).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let segs: Vec<_> = blocks(&bytes, Limits::default())
            .into_iter()
            .filter(|b| b.0 == 's')
            .collect();
        assert_eq!(segs.len(), 3, "{segs:?}");
        assert!(segs.iter().all(|s| s.2 + s.3 <= SEGMENT_ROWS), "{segs:?}");
        let back = read_run(&bytes[..]).unwrap();
        assert_eq!((back.ops.len(), back.reqs.len()), (ops.len(), reqs.len()));
        // By bytes: rows with 4 KiB keys and errors under a 256 KiB segment limit.
        ops.truncate(400);
        reqs.truncate(400);
        for o in &mut ops {
            o.key = format!("{}/{}", "k".repeat(4096), o.op_id);
        }
        for r in &mut reqs {
            r.error = Some(format!("{} {}", "e".repeat(4096), r.op_id));
        }
        let limits = Limits {
            segment_bytes: 256 << 10,
            ..Default::default()
        };
        let path = dir.path().join("bytes.oprobe");
        let mut w = RunFileWriter::create_within(&path, &header, limits).unwrap();
        w.write_rows(&ops, &reqs).unwrap();
        w.finish(&report).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let segs: Vec<_> = blocks(&bytes, limits)
            .into_iter()
            .filter(|b| b.0 == 's')
            .collect();
        assert!(segs.len() > 10, "{segs:?}");
        assert!(segs.iter().all(|s| s.1 <= 256 << 10), "{segs:?}");
        let back = read_run_within(&bytes[..], limits).unwrap();
        assert_eq!(back.ops, ops);
        assert_eq!(back.reqs, reqs);
    }

    /// A failing operation as a long run against a broken target records it.
    fn failing(i: u64, base: &Run) -> (OpRow, ReqRow) {
        let error = format!("Unexpected (temporary) at write => connection refused #{i}");
        let op = OpRow {
            op_id: i + 1,
            t_us: i as i64 * 100,
            ok: false,
            error_code: Some(FailureCode::NetUnreachable),
            error: Some(error.clone()),
            ..base.ops[0].clone()
        };
        let req = ReqRow {
            op_id: op.op_id,
            t_us: op.t_us,
            status: 0,
            error_code: Some(FailureCode::NetUnreachable),
            error: Some(error),
            ..base.reqs[0].clone()
        };
        (op, req)
    }

    /// K1-7. 9.2 kept every failed and throttled operation whatever the cap: a long run
    /// against a failing target saved a file past what the reader takes, and then could not
    /// open it. They are kept in full for twice the cap, then thinned as the other rows are,
    /// each kept row carrying the operations it stands for.
    #[test]
    fn failed_operations_are_thinned_past_twice_the_cap() {
        use crate::trace::RowSink;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.oprobe");
        let base = crate::report::fixture::run();
        let header = RunHeader::new("x", base.report.spec.clone());
        // Rows of 2 (op + request); a cap of 1 000 rows.
        let st = RunFileStream::create(&path, &header, 1_000).unwrap();
        let n = 200_000u64;
        for i in 0..n {
            let (o, r) = failing(i, &base);
            st.on_op(&o, std::slice::from_ref(&r));
        }
        let s = st.sampling();
        assert_eq!(s.seen_ops, n);
        // Twice the cap in full (1 000 operations), then an eighth of it a level.
        assert!(s.kept_ops >= 1_000 && s.kept_ops < 2_500, "{s:?}");
        assert_eq!(s.always_kept_ops, s.kept_ops);
        let mut report = base.report.clone();
        st.describe(&mut report);
        assert!(report.run.sampled);
        assert!(report.warnings.is_empty(), "thinned, not full");
        st.finish(&report).unwrap();
        let back = read_run(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(back.ops.len() as u64, s.kept_ops);
        assert!((1..=1_000).all(|id| back.op(id).is_some_and(|o| o.weight == 1.0)));
        // The weights say what the kept rows stand for: every operation seen, give or take
        // the one level being filled.
        let stood_for: f64 = back.ops.iter().map(|o| o.weight).sum();
        assert!(
            (stood_for - n as f64).abs() < n as f64 * 0.2,
            "{stood_for} of {n}"
        );
        assert!(back.ops.iter().any(|o| o.weight > 64.0));
        assert_eq!(
            back.op_weights().len(),
            back.ops.iter().filter(|o| o.weight != 1.0).count()
        );
    }

    /// K1-7. The largest file a stream writes is one the reader opens: the stream stops
    /// keeping rows before they cost a reader more than it takes, by the memory of the rows
    /// and by the bytes the file decodes to (a cap raised far enough made 9.2 write a file
    /// it then refused).
    #[test]
    fn the_largest_file_a_stream_writes_is_read_back() {
        use crate::trace::RowSink;
        let dir = tempfile::tempdir().unwrap();
        let base = crate::report::fixture::run();
        let header = RunHeader::new("x", base.report.spec.clone());
        let roomy = Limits {
            segment_bytes: 64 << 10,
            block_bytes: 256 << 10,
            decoded_bytes: 64 << 20,
            row_bytes: 64 << 20,
        };
        for (name, limits) in [
            (
                "rows",
                Limits {
                    row_bytes: 1 << 20,
                    ..roomy
                },
            ),
            (
                "decoded",
                Limits {
                    decoded_bytes: 1 << 20,
                    ..roomy
                },
            ),
        ] {
            let path = dir.path().join(format!("{name}.oprobe"));
            // A cap nothing reaches: only the reader's limits stop the stream.
            let st = RunFileStream::create_within(&path, &header, usize::MAX, limits).unwrap();
            let n = 20_000u64;
            for i in 0..n {
                let (o, r) = failing(i, &base);
                st.on_op(&o, std::slice::from_ref(&r));
                if i % 97 == 0 {
                    st.on_unscoped(&r);
                }
            }
            let s = st.sampling();
            assert_eq!(s.seen_ops, n, "{name}");
            assert!(s.kept_ops > 500 && s.kept_ops < n, "{name}: {s:?}");
            assert!(s.kept_requests >= s.kept_ops && s.kept_requests < s.seen_requests);
            let counted = st.state.lock().unwrap().row_bytes;
            let mut report = base.report.clone();
            st.describe(&mut report);
            assert!(report.run.sampled, "{name}");
            // The report says why rows are missing.
            let warned = report.warnings.last().unwrap();
            assert!(
                warned.contains("the run file is full") && warned.contains("of 20000 operations"),
                "{warned}"
            );
            st.finish(&report).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let back = read_run_within(&bytes[..], limits)
                .unwrap_or_else(|e| panic!("{name}: the stream's own file is refused: {e:#}"));
            assert_eq!(back.ops.len() as u64, s.kept_ops, "{name}");
            assert_eq!(back.reqs.len() as u64, s.kept_requests, "{name}");
            // Whole operations: each one kept has its request.
            assert!(
                back.ops
                    .iter()
                    .all(|o| !back.requests_of(o.op_id).is_empty())
            );
            // The reader's own accounting: its row budget paid what the stream counted, and
            // the file decoded to no more than it may.
            let mut rd = RunFileReader::with_limits(&bytes[..], limits).unwrap();
            let mut budget = RowBudget::new(limits.row_bytes);
            let mut intervals = 0;
            while let Some(b) = rd.next_block().unwrap() {
                match b {
                    Block::Segment(s) => {
                        assert!(s.ops_count() + s.requests_count() <= SEGMENT_ROWS);
                        s.decode(&mut budget).unwrap();
                    }
                    Block::Interval(_) => intervals += 1,
                    _ => {}
                }
            }
            assert_eq!(limits.row_bytes - budget.left, counted, "{name}");
            assert!(counted <= limits.row_bytes && rd.decoded <= limits.decoded_bytes);
            assert_eq!(intervals, report.timeline.len(), "{name}");
            if name == "rows" {
                // Full to within one operation of the limit.
                assert!(limits.row_bytes - counted < 4 << 10, "{counted}");
            }
        }
        // A file with no room left for its intervals leaves them out, never the report.
        let path = dir.path().join("tight.oprobe");
        let tight = Limits {
            decoded_bytes: 5
                + serde_json::to_vec(&header).unwrap().len() as u64
                + 5
                + u64::from(roomy.block_bytes)
                + 100,
            ..roomy
        };
        let st = RunFileStream::create_within(&path, &header, usize::MAX, tight).unwrap();
        let (o, r) = failing(0, &base);
        st.on_op(&o, std::slice::from_ref(&r));
        assert_eq!(st.sampling().kept_ops, 0, "no segment fits");
        st.finish(&base.report).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let kinds: String = blocks(&bytes, tight).iter().map(|b| b.0).collect();
        assert_eq!(kinds, "hr");
        assert_eq!(
            read_run_within(&bytes[..], tight).unwrap().report,
            base.report
        );
    }

    /// Requests outside any operation fill a segment as the others do: 9.2 only cut a
    /// segment when an operation ended, so they grew the one being filled without bound.
    #[test]
    fn unscoped_requests_cut_segments_too() {
        use crate::trace::RowSink;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u.oprobe");
        let base = crate::report::fixture::run();
        let header = RunHeader::new("x", base.report.spec.clone());
        let st = RunFileStream::create(&path, &header, 10 * SEGMENT_ROWS).unwrap();
        let n = 2 * SEGMENT_ROWS + 10;
        for i in 0..n {
            let mut r = base.reqs[0].clone();
            r.op_id = 0;
            r.t_us = i as i64;
            st.on_unscoped(&r);
        }
        st.finish(&base.report).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let segs: Vec<usize> = blocks(&bytes, Limits::default())
            .iter()
            .filter(|b| b.0 == 's')
            .map(|b| b.3)
            .collect();
        assert_eq!(segs, [SEGMENT_ROWS, SEGMENT_ROWS, 10]);
        assert_eq!(read_run(&bytes[..]).unwrap().reqs.len(), n);
    }
}
