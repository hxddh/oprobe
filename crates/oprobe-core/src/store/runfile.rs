//! The run file `oprobe.run/5` (design v5 §2): a zstd stream of framed blocks, written while
//! the run is in progress and read back block by block. The process never encodes the whole
//! file in memory.
//!
//! ```text
//! zstd( frame* )         frame = kind:u8  len:u32 (LE)  payload[len] (JSON)
//! kind 1  header         RunHeader: format, run id, spec, client, versions
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
//! [`RunFileWriter`] writes to `<path>.tmp` and renames on [`RunFileWriter::finish`]; a run that
//! crashes leaves no half-written `.oprobe` behind.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::errors::ErrorCode;
use crate::model::{
    ClientInfo, OpBreakdownUs, OpRow, RUN_FORMAT, Report, ReqRow, Run, RunSpec, TcpRow, TlsRow,
    WireProtocol,
};

const ZSTD_LEVEL: i32 = 3;
/// Rows per segment when a finished run is written in one go ([`write_run`]).
pub const SEGMENT_ROWS: usize = 16_384;

const KIND_HEADER: u8 = 1;
const KIND_SEGMENT: u8 = 2;
const KIND_INTERVAL: u8 = 3;
const KIND_REPORT: u8 = 4;
/// Refuse frames larger than this (a corrupt length).
const MAX_FRAME: u32 = 1 << 30;

/// The first block: what the run is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunHeader {
    /// Always [`RUN_FORMAT`].
    pub format: String,
    pub run_id: String,
    pub oprobe_version: String,
    pub created_at: String,
    pub spec: RunSpec,
    pub client: ClientInfo,
}

impl RunHeader {
    pub fn new(run_id: impl Into<String>, spec: RunSpec, client: ClientInfo) -> Self {
        Self {
            format: RUN_FORMAT.to_owned(),
            run_id: run_id.into(),
            oprobe_version: env!("CARGO_PKG_VERSION").to_owned(),
            created_at: crate::util::now_rfc3339(),
            spec,
            client,
        }
    }
}

/// One slice of the run's timeline (per-second; stream MEAS adds sketches).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IntervalBlock {
    /// Start of the slice, seconds from the window start.
    pub t_s: f64,
    pub len_s: f64,
    pub ops_count: u64,
    pub errors_count: u64,
    pub throttled_count: u64,
    pub transferred_bytes: u64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    /// Requests in flight at the end of the slice (when measured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inflight_count: Option<u64>,
}

/// Interned strings of one segment.
#[derive(Debug, Default)]
struct Interner {
    table: Vec<String>,
    index: HashMap<String, u32>,
}

impl Interner {
    fn id(&mut self, s: &str) -> u32 {
        if let Some(i) = self.index.get(s) {
            return *i;
        }
        let i = self.table.len() as u32;
        self.table.push(s.to_owned());
        self.index.insert(s.to_owned(), i);
        i
    }

    fn opt(&mut self, s: Option<&str>) -> Option<u32> {
        s.map(|s| self.id(s))
    }
}

fn u32_sat(v: u64) -> u32 {
    v.min(u64::from(u32::MAX)) as u32
}

/// Columnar operation rows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
struct OpColumns {
    op_id: Vec<u64>,
    verb: Vec<u32>,
    key: Vec<u32>,
    t_us: Vec<u32>,
    latency_us: Vec<u32>,
    bytes: Vec<u64>,
    ok: Vec<bool>,
    error_code: Vec<Option<ErrorCode>>,
    error: Vec<Option<u32>>,
    throttled: Vec<bool>,
    attempts: Vec<u32>,
    /// queue, cache, backend, retry, client (µs) per row.
    breakdown: Vec<[u32; 5]>,
    requests: Vec<u32>,
    await_tree: Vec<Option<u32>>,
}

/// Columnar request rows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
struct ReqColumns {
    t_us: Vec<u32>,
    op_id: Vec<u64>,
    attempt: Vec<u32>,
    api: Vec<u32>,
    method: Vec<u32>,
    status: Vec<u16>,
    protocol: Vec<WireProtocol>,
    reused: Vec<bool>,
    conn_id: Vec<u64>,
    remote: Vec<Option<u32>>,
    /// queue, dns, tcp, proxy, tls, send, ttfb, body, stall, total (µs) per row.
    stages: Vec<[u32; 10]>,
    bytes_up: Vec<u64>,
    bytes_down: Vec<u64>,
    request_id: Vec<Option<u32>>,
    retry_after_ms: Vec<Option<u64>>,
    error_code: Vec<Option<ErrorCode>>,
    error: Vec<Option<u32>>,
    /// Sparse: (row index, value).
    tls: Vec<(u32, TlsRow)>,
    tcp: Vec<(u32, TcpRow)>,
}

/// One block of rows: a time slice of the run (columnar, interned strings).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Segment {
    /// Times are µs after this base (µs from the window start; may be negative).
    base_us: i64,
    strings: Vec<String>,
    ops: OpColumns,
    reqs: ReqColumns,
}

impl Segment {
    /// Encode rows (any order) into one segment.
    pub fn encode(ops: &[OpRow], reqs: &[ReqRow]) -> Self {
        let base_us = ops
            .iter()
            .map(|o| o.t_us)
            .chain(reqs.iter().map(|r| r.t_us))
            .min()
            .unwrap_or(0);
        let rel = |t: i64| u32_sat((t - base_us).max(0) as u64);
        let mut s = Interner::default();
        let mut oc = OpColumns::default();
        for o in ops {
            oc.op_id.push(o.op_id);
            oc.verb.push(s.id(&o.verb));
            oc.key.push(s.id(&o.key));
            oc.t_us.push(rel(o.t_us));
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
        }
        let mut rc = ReqColumns::default();
        for (i, r) in reqs.iter().enumerate() {
            rc.t_us.push(rel(r.t_us));
            rc.op_id.push(r.op_id);
            rc.attempt.push(r.attempt);
            rc.api.push(s.id(&r.api));
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
        }
        Segment {
            base_us,
            strings: s.table,
            ops: oc,
            reqs: rc,
        }
    }

    /// Operation and request rows of this segment.
    pub fn decode(&self) -> Result<(Vec<OpRow>, Vec<ReqRow>)> {
        let st = |i: u32| -> Result<&str> {
            self.strings
                .get(i as usize)
                .map(String::as_str)
                .with_context(|| format!("run file segment: no string #{i}"))
        };
        let sto = |i: Option<u32>| -> Result<Option<String>> {
            i.map(|i| st(i).map(str::to_owned)).transpose()
        };
        let t = |v: u32| self.base_us + i64::from(v);
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
        let mut ops = Vec::with_capacity(n);
        for i in 0..n {
            let b = o.breakdown[i].map(u64::from);
            ops.push(OpRow {
                op_id: o.op_id[i],
                verb: intern_static(st(o.verb[i])?),
                key: st(o.key[i])?.to_owned(),
                t_us: t(o.t_us[i]),
                latency_us: u64::from(o.latency_us[i]),
                bytes: o.bytes[i],
                ok: o.ok[i],
                error_code: o.error_code[i],
                error: sto(o.error[i])?,
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
                await_tree: sto(o.await_tree[i])?,
            });
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
        let mut reqs = Vec::with_capacity(m);
        for i in 0..m {
            let s = r.stages[i].map(u64::from);
            reqs.push(ReqRow {
                t_us: t(r.t_us[i]),
                op_id: r.op_id[i],
                attempt: r.attempt[i],
                api: intern_static(st(r.api[i])?),
                method: intern_static(st(r.method[i])?),
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
                request_id: sto(r.request_id[i])?,
                retry_after_ms: r.retry_after_ms[i],
                error_code: r.error_code[i],
                error: sto(r.error[i])?,
                tls: None,
                tcp: None,
            });
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
        Ok((ops, reqs))
    }

    pub fn ops_count(&self) -> usize {
        self.ops.op_id.len()
    }

    pub fn requests_count(&self) -> usize {
        self.reqs.t_us.len()
    }
}

/// API, method and verb names are a small static vocabulary: no allocation per row for them.
fn intern_static(s: &str) -> Cow<'static, str> {
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let names = NAMES.get_or_init(Default::default);
    let mut m = names.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = m.get(s) {
        return Cow::Borrowed(v);
    }
    if m.len() >= 4096 {
        // Not a vocabulary any more: stop leaking.
        return Cow::Owned(s.to_owned());
    }
    let v: &'static str = Box::leak(s.to_owned().into_boxed_str());
    m.insert(s.to_owned(), v);
    Cow::Borrowed(v)
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
/// as the run produces them, the report last.
pub struct RunFileWriter {
    path: PathBuf,
    tmp: PathBuf,
    enc: Option<zstd::stream::Encoder<'static, BufWriter<std::fs::File>>>,
    ops: u64,
    reqs: u64,
}

impl RunFileWriter {
    /// Start a run file with its header.
    pub fn create(path: &Path, header: &RunHeader) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let tmp = path.with_extension("oprobe.tmp");
        let file = std::fs::File::create(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        let enc = zstd::stream::Encoder::new(BufWriter::new(file), ZSTD_LEVEL)?;
        let mut w = Self {
            path: path.to_owned(),
            tmp,
            enc: Some(enc),
            ops: 0,
            reqs: 0,
        };
        w.frame(KIND_HEADER, header)?;
        Ok(w)
    }

    fn frame<T: Serialize>(&mut self, kind: u8, v: &T) -> Result<()> {
        let payload = serde_json::to_vec(v)?;
        let len = u32::try_from(payload.len()).context("run file block too large")?;
        let enc = self.enc.as_mut().context("run file already finished")?;
        enc.write_all(&[kind])?;
        enc.write_all(&len.to_le_bytes())?;
        enc.write_all(&payload)?;
        Ok(())
    }

    /// Append rows (one segment; call it once per time slice).
    pub fn write_rows(&mut self, ops: &[OpRow], reqs: &[ReqRow]) -> Result<()> {
        if ops.is_empty() && reqs.is_empty() {
            return Ok(());
        }
        self.write_segment(&Segment::encode(ops, reqs))
    }

    pub fn write_segment(&mut self, s: &Segment) -> Result<()> {
        self.ops += s.ops_count() as u64;
        self.reqs += s.requests_count() as u64;
        self.frame(KIND_SEGMENT, s)
    }

    pub fn write_interval(&mut self, i: &IntervalBlock) -> Result<()> {
        self.frame(KIND_INTERVAL, i)
    }

    /// Rows written so far.
    pub fn rows_written(&self) -> (u64, u64) {
        (self.ops, self.reqs)
    }

    /// Write the report, close the stream and move the file into place.
    pub fn finish(mut self, report: &Report) -> Result<PathBuf> {
        self.frame(KIND_REPORT, report)?;
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

/// Reads a run file block by block.
pub struct RunFileReader<R: Read> {
    dec: zstd::stream::Decoder<'static, BufReader<R>>,
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
        Ok(Self {
            dec: zstd::stream::Decoder::new(r).context("not a zstd stream")?,
        })
    }

    /// The next block (`None` at the end of the stream).
    pub fn next_block(&mut self) -> Result<Option<Block>> {
        let mut head = [0u8; 5];
        if !read_exact_or_eof(&mut self.dec, &mut head)? {
            return Ok(None);
        }
        let kind = head[0];
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
        if len > MAX_FRAME {
            bail!("corrupt run file: a block of {len} bytes");
        }
        let mut payload = vec![0u8; len as usize];
        self.dec
            .read_exact(&mut payload)
            .context("corrupt run file: truncated block")?;
        let bad = |what: &str, e: serde_json::Error| {
            anyhow::anyhow!("corrupt run file: bad {what} block: {e}")
        };
        Ok(Some(match kind {
            KIND_HEADER => Block::Header(Box::new(
                serde_json::from_slice(&payload).map_err(|e| bad("header", e))?,
            )),
            KIND_SEGMENT => Block::Segment(Box::new(
                serde_json::from_slice(&payload).map_err(|e| bad("segment", e))?,
            )),
            KIND_INTERVAL => {
                Block::Interval(serde_json::from_slice(&payload).map_err(|e| bad("interval", e))?)
            }
            KIND_REPORT => Block::Report(Box::new(Report::from_json_value(
                serde_json::from_slice(&payload).map_err(|e| bad("report", e))?,
            )?)),
            other => Block::Unknown(other),
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

/// Write a finished run as `oprobe.run/5` (rows in segments of [`SEGMENT_ROWS`], by time).
pub fn write_run(path: &Path, run: &Run) -> Result<PathBuf> {
    let header = RunHeader::new(
        run.report.run.id.clone(),
        run.report.spec.clone(),
        run.report.client.clone(),
    );
    let mut w = RunFileWriter::create(path, &header)?;
    for p in &run.report.timeline {
        w.write_interval(&IntervalBlock {
            t_s: p.t_s,
            len_s: 1.0,
            ops_count: p.ops_per_s.round().max(0.0) as u64,
            errors_count: p.errors,
            throttled_count: p.throttled,
            transferred_bytes: (p.mib_per_s * 1024.0 * 1024.0).round().max(0.0) as u64,
            p50_ms: p.p50_ms,
            p99_ms: p.p99_ms,
            inflight_count: None,
        })?;
    }
    // Segments are time slices: rows sorted by start time (a run's rows are; others are
    // sorted here).
    let sorted_ops;
    let sorted_reqs;
    let (ops, reqs) =
        if run.ops.is_sorted_by_key(|o| o.t_us) && run.reqs.is_sorted_by_key(|r| r.t_us) {
            (&run.ops[..], &run.reqs[..])
        } else {
            let mut o = run.ops.clone();
            let mut r = run.reqs.clone();
            o.sort_by_key(|o| (o.t_us, o.op_id));
            r.sort_by_key(|r| (r.t_us, r.op_id));
            sorted_ops = o;
            sorted_reqs = r;
            (&sorted_ops[..], &sorted_reqs[..])
        };
    let (mut oi, mut ri) = (0, 0);
    while oi < ops.len() || ri < reqs.len() {
        // The slice ends where either kind reaches SEGMENT_ROWS rows.
        let cut = ops
            .get(oi + SEGMENT_ROWS)
            .map_or(i64::MAX, |o| o.t_us)
            .min(reqs.get(ri + SEGMENT_ROWS).map_or(i64::MAX, |r| r.t_us));
        let mut oe = oi + ops[oi..].partition_point(|o| o.t_us < cut);
        let mut re = ri + reqs[ri..].partition_point(|r| r.t_us < cut);
        if oe == oi && re == ri {
            // More than SEGMENT_ROWS rows at one instant: a fixed slice.
            oe = (oi + SEGMENT_ROWS).min(ops.len());
            re = (ri + SEGMENT_ROWS).min(reqs.len());
        }
        w.write_rows(&ops[oi..oe], &reqs[ri..re])?;
        oi = oe;
        ri = re;
    }
    w.finish(&run.report)
}

/// Read a whole `oprobe.run/5` file into a [`Run`] (rows sorted by start time).
pub fn read_run(r: impl Read) -> Result<Run> {
    let mut rd = RunFileReader::new(r)?;
    let mut header = None;
    let mut report = None;
    let mut ops = Vec::new();
    let mut reqs = Vec::new();
    while let Some(b) = rd.next_block()? {
        match b {
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
                let (o, q) = s.decode()?;
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
        let (ops, reqs) = s.decode().unwrap();
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
        let header = RunHeader::new("x", run.report.spec.clone(), ClientInfo::opendal());
        {
            let mut w = RunFileWriter::create(&path, &header).unwrap();
            w.write_rows(&run.ops, &run.reqs).unwrap();
            assert_eq!(w.rows_written().0, run.ops.len() as u64);
        }
        assert!(!path.exists());
        assert!(!path.with_extension("oprobe.tmp").exists());
    }
}
