//! The `.oprobe` file a traced operator writes ([`crate::Handle::save`],
//! [`crate::Snapshot::save`]): format `oprobe.trace/9`, which `oprobe show FILE` and the
//! workbench open like any other profile.
//!
//! It is one JSON document, so it can be read without oprobe:
//!
//! ```text
//! {
//!   "format": "oprobe.trace/9",
//!   "oprobe_trace_version": "9.0.0",
//!   "created_at": "2026-11-25T10:00:00Z",
//!   "service": { "scheme": "s3", "name": "my-bucket", "root": "/data/" },   // optional
//!   "snapshot": { … the aggregates: per API and stage, connections, bytes, timeline … },
//!   "rows": { "ops": [ … ], "reqs": [ … ], "info": { … }, "sampling": { … } }   // optional
//! }
//! ```
//!
//! The format is stable across 9.x: fields are only ever added. `oprobe.trace/9` holds the
//! application's operations (`rows.ops`), each request linked to the operation that issued it.

use std::io::{self, BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Snapshot;
use crate::log::TraceRows;
use crate::rows::{OpRow, ReqRow, TraceInfo};
use crate::sample::SamplingStats;

/// The `format` of a file written by `oprobe-trace`.
pub const TRACE_FORMAT: &str = "oprobe.trace/9";

/// The formats [`TraceFile`] reads.
pub const TRACE_FORMATS_READ: &[&str] = &[TRACE_FORMAT];

fn default_true() -> bool {
    true
}

/// The OpenDAL service the traced operator talks to, as the layer saw it (the first operator
/// the layer was applied to). No credentials or secret options: the scheme, the service's name
/// (the bucket or container), the root and the client's transport posture.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct ServiceRecord {
    /// The OpenDAL scheme (`s3`, `gcs`, `memory`, ...).
    pub scheme: String,
    /// The service's name: the bucket or container (empty when the service has none).
    #[serde(default)]
    pub name: String,
    /// The operator's root (`/` by default).
    #[serde(default)]
    pub root: String,
    /// Configured HTTP version of the traced client (`auto` / `h1` / `h2`).
    #[serde(default)]
    pub http_version: crate::HttpVersion,
    /// Whether connection pooling (`keep_alive`) was enabled on the traced client.
    #[serde(default = "default_true")]
    pub keep_alive: bool,
    /// Configured in-flight HTTP request limit on the traced client, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inflight_count: Option<usize>,
}

impl Default for ServiceRecord {
    fn default() -> Self {
        Self {
            scheme: String::new(),
            name: String::new(),
            root: String::new(),
            http_version: crate::HttpVersion::Auto,
            keep_alive: true,
            max_inflight_count: None,
        }
    }
}

#[derive(Serialize)]
struct FileRef<'a> {
    format: &'static str,
    oprobe_trace_version: &'static str,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    service: Option<&'a ServiceRecord>,
    snapshot: &'a Snapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<RowsRef<'a>>,
}

#[derive(Serialize)]
struct RowsRef<'a> {
    ops: &'a [OpRow],
    reqs: &'a [ReqRow],
    info: &'a TraceInfo,
    sampling: &'a SamplingStats,
}

/// Write `snapshot` (and, when given, the sampled rows) to `path`, atomically: the file is
/// written next to `path` and renamed into place.
pub(crate) fn write(
    path: &Path,
    service: Option<&ServiceRecord>,
    snapshot: &Snapshot,
    rows: Option<&TraceRows>,
) -> io::Result<()> {
    let doc = FileRef {
        format: TRACE_FORMAT,
        oprobe_trace_version: env!("CARGO_PKG_VERSION"),
        created_at: crate::util::now_rfc3339(),
        service,
        snapshot,
        rows: rows.map(|r| RowsRef {
            ops: &r.ops,
            reqs: &r.reqs,
            info: &r.info,
            sampling: &r.sampling,
        }),
    };
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let result = (|| {
        let mut w = BufWriter::new(std::fs::File::create(&tmp)?);
        serde_json::to_writer(&mut w, &doc).map_err(io::Error::other)?;
        w.write_all(b"\n")?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()
    })();
    match result {
        Ok(()) => std::fs::rename(&tmp, path),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// A file written by `oprobe-trace`, read back.
#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(not(feature = "__internal"), allow(dead_code))]
pub struct TraceFile {
    /// One of [`TRACE_FORMATS_READ`].
    pub format: String,
    pub oprobe_trace_version: String,
    pub created_at: String,
    /// The service the traced operator talks to (absent in a bare [`Snapshot::save`]).
    #[serde(default)]
    pub service: Option<ServiceRecord>,
    pub snapshot: Snapshot,
    #[serde(default)]
    rows: Option<OwnedRows>,
}

#[derive(Clone, Debug, Deserialize)]
struct OwnedRows {
    ops: Vec<OpRow>,
    reqs: Vec<ReqRow>,
    info: TraceInfo,
    sampling: SamplingStats,
}

#[cfg_attr(not(feature = "__internal"), allow(dead_code))]
impl TraceFile {
    /// Whether `bytes` (the start of a file) look like a trace file: a JSON object rather than
    /// the zstd stream of an `oprobe.run` file.
    pub fn sniff(bytes: &[u8]) -> bool {
        bytes
            .iter()
            .find(|b| !b.is_ascii_whitespace())
            .is_some_and(|b| *b == b'{')
    }

    /// Read a trace file. Fails on another format (an `oprobe.run` file, or a later major).
    /// A 6.x file reads as it was written: requests, no operation rows.
    pub fn read(path: &Path) -> io::Result<TraceFile> {
        let f = std::fs::File::open(path)?;
        let file: TraceFile = serde_json::from_reader(io::BufReader::new(f))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        file.check_format(&path.display().to_string())
    }

    /// Parse a trace file already in memory (`what` names it in errors).
    pub fn from_slice(bytes: &[u8], what: &str) -> io::Result<TraceFile> {
        let file: TraceFile = serde_json::from_slice(bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        file.check_format(what)
    }

    fn check_format(self, what: &str) -> io::Result<TraceFile> {
        if !Self::reads(&self.format) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("`{what}` is format `{}`, not `{TRACE_FORMAT}`", self.format),
            ));
        }
        Ok(self)
    }

    /// Whether this version reads trace files of `format` ([`TRACE_FORMATS_READ`]).
    pub fn reads(format: &str) -> bool {
        TRACE_FORMATS_READ.contains(&format)
    }

    /// The sampled rows the file holds (empty when it was written from a bare snapshot).
    pub fn rows(&self) -> TraceRows {
        match &self.rows {
            Some(r) => TraceRows {
                ops: r.ops.clone(),
                reqs: r.reqs.clone(),
                info: r.info.clone(),
                sampling: r.sampling,
            },
            None => TraceRows::default(),
        }
    }

    /// Move the sampled rows out of the file (empty when it holds none, or they were taken).
    pub fn take_rows(&mut self) -> TraceRows {
        match self.rows.take() {
            Some(r) => TraceRows {
                ops: r.ops,
                reqs: r.reqs,
                info: r.info,
                sampling: r.sampling,
            },
            None => TraceRows::default(),
        }
    }

    /// Whether the file holds rows (written by [`crate::Handle::save`]).
    pub fn has_rows(&self) -> bool {
        self.rows.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(format: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "format": format,
            "oprobe_trace_version": "6.3.0",
            "created_at": "2026-11-22T10:00:00Z",
            "snapshot": Snapshot::default(),
            "rows": { "ops": [], "reqs": [], "info": TraceInfo::default(),
                      "sampling": SamplingStats::default() },
        }))
        .unwrap()
    }

    #[test]
    fn this_major_reads_and_rejects_others() {
        assert_eq!(TRACE_FORMAT, "oprobe.trace/9");
        let t = TraceFile::from_slice(&doc("oprobe.trace/9"), "x").unwrap();
        assert_eq!(t.format, "oprobe.trace/9");
        assert!(t.has_rows());
        for other in ["oprobe.trace/8", "oprobe.trace/7", "oprobe.trace/6", "oprobe.run/7"] {
            let e = TraceFile::from_slice(&doc(other), "x").unwrap_err();
            assert!(e.to_string().contains(other), "{e}");
        }
    }
}
