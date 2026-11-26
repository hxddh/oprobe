//! The `.oprobe` file a traced operator writes ([`crate::Handle::save`],
//! [`crate::Snapshot::save`]): format `oprobe.trace/9`, which `oprobe show FILE` and the
//! workbench open like any other profile.
//!
//! It is one JSON document, so it can be read without oprobe:
//!
//! ```text
//! {
//!   "format": "oprobe.trace/9",
//!   "oprobe_trace_version": "9.1.0",
//!   "created_at": "2026-11-25T10:00:00Z",
//!   "service": { "scheme": "s3", "name": "my-bucket", "root": "/data/",
//!                "http_version": "auto", "keep_alive": true },              // optional
//!   "snapshot": { … the aggregates: per API and stage, connections, bytes, timeline … },
//!   "rows": { "ops": [ … ], "reqs": [ … ], "info": { … }, "sampling": { … } }   // optional
//! }
//! ```
//!
//! The format is stable across 9.x: fields are only ever added. `oprobe.trace/9` holds the
//! application's operations (`rows.ops`), each request linked to the operation that issued it.
//!
//! A layer that kept the operator's own HTTP transport (`Config::keep_http_transport`) writes
//! `"http_transport": "installed"` in `service` instead of `http_version` and `keep_alive`: the
//! client is the application's, and its connections were not observed (9.3).

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

/// [`ServiceRecord::http_transport`] of a layer that kept the transport installed on the
/// operator and observed it from outside (`Config::keep_http_transport`).
pub const HTTP_TRANSPORT_INSTALLED: &str = "installed";

/// The OpenDAL service the traced operator talks to, as the layer saw it (the first operator
/// the layer was applied to). No credentials or secret options: the scheme, the service's name
/// (the bucket or container), the root and the client's transport posture.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Configured HTTP version of the instrumented client (`auto` / `h1` / `h2`). Absent when
    /// that client was not in use ([`Self::http_transport`]), and in files that do not say:
    /// the client's default (`auto`) then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_version: Option<crate::HttpVersion>,
    /// Whether connection pooling (`keep_alive`) was enabled on the instrumented client.
    /// Absent like `http_version`: the client's default (on) then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<bool>,
    /// Configured in-flight HTTP request limit of the layer, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inflight_count: Option<usize>,
    /// Whose HTTP client carried the requests: absent for oprobe's instrumented client;
    /// [`HTTP_TRANSPORT_INSTALLED`] for the transport installed on the operator, observed from
    /// outside. Its connections were not observed then (reuse and dials are unknown, not
    /// zero: [`crate::ConnStats::observed`]), and a request's time to first byte includes its
    /// dial and the sending of its body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_transport: Option<String>,
}

impl ServiceRecord {
    /// Whether the requests went through the transport installed on the operator
    /// (`Config::keep_http_transport`) instead of the instrumented client.
    pub fn keeps_http_transport(&self) -> bool {
        self.http_transport.as_deref() == Some(HTTP_TRANSPORT_INSTALLED)
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

    /// The `service` of every 9.x file reads: 9.0 to 9.2 always wrote the instrumented
    /// client's `http_version` and `keep_alive`; 9.3 leaves them out, and says
    /// `http_transport`, when the operator's own transport carried the requests. A 9.2 reader
    /// takes its defaults for what is left out.
    #[test]
    fn the_service_of_every_9x_file_reads() {
        let read = |json: &str| serde_json::from_str::<ServiceRecord>(json).unwrap();
        let old = read(
            r#"{"scheme":"s3","name":"b","root":"/","http_version":"h2","keep_alive":false,"max_inflight_count":8}"#,
        );
        assert_eq!(old.http_version, Some(crate::HttpVersion::H2));
        assert_eq!((old.keep_alive, old.max_inflight_count), (Some(false), Some(8)));
        assert!(!old.keeps_http_transport());
        let bare = read(r#"{"scheme":"memory"}"#);
        assert_eq!((bare.http_version, bare.keep_alive), (None, None));

        let kept = ServiceRecord {
            scheme: "s3".into(),
            name: "b".into(),
            root: "/".into(),
            max_inflight_count: Some(8),
            http_transport: Some(HTTP_TRANSPORT_INSTALLED.into()),
            ..Default::default()
        };
        let written = serde_json::to_string(&kept).unwrap();
        assert_eq!(
            written,
            r#"{"scheme":"s3","name":"b","root":"/","max_inflight_count":8,"http_transport":"installed"}"#
        );
        assert!(read(&written).keeps_http_transport());
        assert_eq!(read(&written), kept);
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
