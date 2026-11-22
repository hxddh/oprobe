//! An OpenDAL service whose operations are executed by another client (design v5 §7).
//!
//! The workload engine drives an OpenDAL [`opendal::Operator`]; this service turns each of its
//! calls into one call of a [`Backend`] (aws-sdk-s3, or an external adapter over the oprobe
//! client protocol). So every workload, the op scopes, the recorder, budgets, findings and the
//! report stay exactly as with the OpenDAL client: only what issues the requests changes.

use std::fmt::Debug;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use opendal::raw::oio;
use opendal::raw::*;
use opendal::{
    Buffer, Capability, Error, ErrorKind, Metadata, MetadataBuilder, OperationContext, Result,
};

/// A byte range of a [`ClientOp::Get`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GetRange {
    Whole,
    /// `offset..offset+length` (`length: None`: to the end).
    From {
        offset: u64,
        length: Option<u64>,
    },
    /// The last N bytes.
    Suffix(u64),
}

/// One storage operation, in the vocabulary every client shares. Keys are full object keys
/// (the target's root included).
#[derive(Clone, Debug)]
pub enum ClientOp {
    Get {
        key: String,
        range: GetRange,
    },
    Put {
        key: String,
        data: Bytes,
        if_match: Option<String>,
        if_none_match: Option<String>,
        content_type: Option<String>,
        metadata: Vec<(String, String)>,
    },
    Head {
        key: String,
    },
    Delete {
        keys: Vec<String>,
    },
    List {
        prefix: String,
        recursive: bool,
        limit: Option<usize>,
    },
    Copy {
        source: String,
        key: String,
    },
}

/// A listed key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListEntry {
    pub key: String,
    pub size: u64,
    pub dir: bool,
}

/// What a client returns.
#[derive(Clone, Debug, Default)]
pub struct ClientReply {
    /// The body of a `get`; `None`: the client reported only its size (`bytes`).
    pub data: Option<Buffer>,
    /// Payload bytes transferred.
    pub bytes: u64,
    /// The object's total size (`get`, `head`).
    pub size: Option<u64>,
    pub etag: Option<String>,
    pub entries: Vec<ListEntry>,
    /// `delete`: keys that were not deleted, with the reason.
    pub failed_keys: Vec<(String, String)>,
}

/// A load generator: executes [`ClientOp`]s and records their requests (into the current
/// [`crate::trace::OpScope`]) itself.
pub trait Backend: Send + Sync + Debug + 'static {
    fn call(&self, op: ClientOp) -> BoxFuture<'static, Result<ClientReply>>;
}

/// The OpenDAL error of a failed client call.
pub fn error_for(status: u16, code: &str, message: &str) -> Error {
    let kind = match (status, code) {
        (404, _) | (_, "NoSuchKey" | "NotFound" | "NoSuchBucket") => ErrorKind::NotFound,
        (412, _) | (_, "PreconditionFailed" | "ConditionalRequestConflict") => {
            ErrorKind::ConditionNotMatch
        }
        (403, _) | (_, "AccessDenied") => ErrorKind::PermissionDenied,
        (429 | 503, _) | (_, "SlowDown" | "Throttling" | "TooManyRequests") => {
            ErrorKind::RateLimited
        }
        _ => ErrorKind::Unexpected,
    };
    let what = match (code.is_empty(), message.is_empty()) {
        (true, true) => format!("the client failed (status {status})"),
        (false, true) => code.to_owned(),
        (true, false) => message.to_owned(),
        (false, false) => format!("{code}: {message}"),
    };
    let mut e = Error::new(kind, what).with_context("status", status.to_string());
    if kind == ErrorKind::RateLimited || status == 0 || status >= 500 {
        e = e.set_temporary();
    }
    e
}

/// The OpenDAL service over a [`Backend`].
#[derive(Clone, Debug)]
pub struct ClientService {
    backend: Arc<dyn Backend>,
    /// Normalized root without the leading `/` (`bench/`, or empty).
    root: String,
    info: ServiceInfo,
    capability: Capability,
}

impl ClientService {
    /// `name` labels the service (`aws_sdk`, `exec`); `root` is the target's root.
    pub fn new(
        backend: Arc<dyn Backend>,
        name: &'static str,
        root: &str,
        capability: Capability,
    ) -> Self {
        let r = normalize_root(root);
        let root = r.trim_start_matches('/').to_owned();
        Self {
            backend,
            info: ServiceInfo::new(name, r, name),
            root,
            capability,
        }
    }

    /// The operator the workload drives: OpenDAL's standard layers over this service.
    pub fn into_operator(self) -> Result<opendal::Operator> {
        opendal::Operator::new(ClientServiceBuilder(Some(self)))
    }

    fn key(&self, path: &str) -> String {
        let p = path.trim_start_matches('/');
        format!("{}{p}", self.root)
    }

    fn rel(&self, key: &str) -> String {
        let k = key.strip_prefix(self.root.as_str()).unwrap_or(key);
        if k.is_empty() { "/".into() } else { k.into() }
    }
}

/// [`opendal::Operator::new`] wants a builder.
#[derive(Default)]
struct ClientServiceBuilder(Option<ClientService>);

impl opendal::Builder for ClientServiceBuilder {
    type Config = ();

    fn build(self) -> Result<impl Service> {
        self.0
            .ok_or_else(|| Error::new(ErrorKind::ConfigInvalid, "no client service to build"))
    }
}

impl Service for ClientService {
    type Reader = oio::StreamReader<ClientReader>;
    type Writer = ClientWriter;
    type Lister = ClientLister;
    type Deleter = oio::BatchDeleter<ClientDeleter>;
    type Copier = oio::OneShotCopier;
    type Composer = ();

    fn info(&self) -> ServiceInfo {
        self.info.clone()
    }

    fn capability(&self) -> Capability {
        self.capability
    }

    async fn create_dir(
        &self,
        _: &OperationContext,
        _: &str,
        _: OpCreateDir,
    ) -> Result<RpCreateDir> {
        Ok(RpCreateDir::default())
    }

    async fn stat(&self, _: &OperationContext, path: &str, _: OpStat) -> Result<RpStat> {
        if path.ends_with('/') {
            return Ok(RpStat::new(MetadataBuilder::dir().build()));
        }
        let r = self
            .backend
            .call(ClientOp::Head {
                key: self.key(path),
            })
            .await?;
        Ok(RpStat::new(file_meta(r.size.unwrap_or(r.bytes), r.etag)))
    }

    fn read(&self, _: &OperationContext, path: &str, _: OpRead) -> Result<Self::Reader> {
        Ok(oio::StreamReader::new(ClientReader {
            backend: self.backend.clone(),
            key: self.key(path),
        }))
    }

    fn write(&self, _: &OperationContext, path: &str, args: OpWrite) -> Result<Self::Writer> {
        Ok(ClientWriter {
            backend: self.backend.clone(),
            key: self.key(path),
            args,
            buf: Vec::new(),
        })
    }

    fn delete(&self, _: &OperationContext) -> Result<Self::Deleter> {
        Ok(oio::BatchDeleter::new(
            ClientDeleter { svc: self.clone() },
            self.capability.delete_max_size,
        ))
    }

    fn list(&self, _: &OperationContext, path: &str, args: OpList) -> Result<Self::Lister> {
        Ok(ClientLister {
            svc: self.clone(),
            prefix: self.key(path),
            recursive: args.recursive(),
            limit: args.limit(),
            entries: None,
        })
    }

    fn copy(&self, _: &OperationContext, from: &str, to: &str, _: OpCopy) -> Result<Self::Copier> {
        let backend = self.backend.clone();
        let (source, key) = (self.key(from), self.key(to));
        Ok(oio::OneShotCopier::new(async move {
            let r = backend.call(ClientOp::Copy { source, key }).await?;
            Ok(file_meta(r.bytes, r.etag))
        }))
    }

    async fn rename(
        &self,
        _: &OperationContext,
        _: &str,
        _: &str,
        _: OpRename,
    ) -> Result<RpRename> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "rename is not supported by this client",
        ))
    }

    async fn presign(&self, _: &OperationContext, _: &str, _: OpPresign) -> Result<RpPresign> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "presign is not supported by this client",
        ))
    }
}

fn file_meta(len: u64, etag: Option<String>) -> Metadata {
    let mut b = MetadataBuilder::file(len);
    if let Some(e) = etag {
        b.etag(e);
    }
    b.build()
}

/// Zeroes standing in for a body the client only reported the size of.
fn zeroes(n: u64) -> Buffer {
    static ZERO: std::sync::OnceLock<Bytes> = std::sync::OnceLock::new();
    let z = ZERO.get_or_init(|| Bytes::from(vec![0u8; 1 << 20]));
    let mut left = n as usize;
    let mut parts = Vec::with_capacity(left / z.len() + 1);
    while left > 0 {
        let k = left.min(z.len());
        parts.push(z.slice(..k));
        left -= k;
    }
    Buffer::from(parts)
}

pub struct ClientReader {
    backend: Arc<dyn Backend>,
    key: String,
}

impl oio::StreamRead for ClientReader {
    async fn open(
        &self,
        range: opendal::BytesRange,
    ) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
        let r = if range.is_suffix() {
            GetRange::Suffix(range.size().unwrap_or(0))
        } else if range.is_full() {
            GetRange::Whole
        } else {
            GetRange::From {
                offset: range.offset(),
                length: range.size(),
            }
        };
        let reply = self
            .backend
            .call(ClientOp::Get {
                key: self.key.clone(),
                range: r,
            })
            .await?;
        let data = reply.data.unwrap_or_else(|| zeroes(reply.bytes));
        let total = reply.size.unwrap_or(data.len() as u64);
        Ok((
            RpRead::new(file_meta(total, None)),
            Box::new(data) as Box<dyn oio::ReadStreamDyn>,
        ))
    }
}

pub struct ClientWriter {
    backend: Arc<dyn Backend>,
    key: String,
    args: OpWrite,
    buf: Vec<Buffer>,
}

impl oio::Write for ClientWriter {
    async fn write(&mut self, bs: Buffer) -> Result<()> {
        self.buf.push(bs);
        Ok(())
    }

    async fn close(&mut self) -> Result<Metadata> {
        let parts = std::mem::take(&mut self.buf);
        let data = if parts.len() == 1 {
            parts.into_iter().next().unwrap_or_default().to_bytes()
        } else {
            let mut v = Vec::with_capacity(parts.iter().map(|b| b.len()).sum());
            for p in parts {
                for chunk in p {
                    v.extend_from_slice(&chunk);
                }
            }
            Bytes::from(v)
        };
        let len = data.len() as u64;
        let a = &self.args;
        let r = self
            .backend
            .call(ClientOp::Put {
                key: self.key.clone(),
                data,
                if_match: a.if_match().map(str::to_owned),
                if_none_match: if a.if_not_exists() {
                    Some("*".into())
                } else {
                    a.if_none_match().map(str::to_owned)
                },
                content_type: a.content_type().map(str::to_owned),
                metadata: a
                    .user_metadata()
                    .map(|m| {
                        m.into_iter()
                            .map(|(k, v)| (k.to_owned(), v.to_owned()))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .await?;
        Ok(file_meta(len, r.etag))
    }

    async fn abort(&mut self) -> Result<()> {
        self.buf.clear();
        Ok(())
    }
}

pub struct ClientDeleter {
    svc: ClientService,
}

impl oio::BatchDelete for ClientDeleter {
    async fn delete_once(&self, path: String, _: OpDelete) -> Result<()> {
        let r = self
            .svc
            .backend
            .call(ClientOp::Delete {
                keys: vec![self.svc.key(&path)],
            })
            .await?;
        match r.failed_keys.into_iter().next() {
            Some((_, why)) => Err(Error::new(ErrorKind::Unexpected, why)),
            None => Ok(()),
        }
    }

    async fn delete_batch(&self, batch: Vec<(String, OpDelete)>) -> Result<oio::BatchDeleteResult> {
        let keys = batch.iter().map(|(p, _)| self.svc.key(p)).collect();
        let r = self.svc.backend.call(ClientOp::Delete { keys }).await?;
        let mut out = oio::BatchDeleteResult::default();
        for (path, args) in batch {
            let key = self.svc.key(&path);
            match r.failed_keys.iter().find(|(k, _)| *k == key) {
                Some((_, why)) => {
                    out.failed
                        .push((path, args, Error::new(ErrorKind::Unexpected, why.clone())))
                }
                None => out.succeeded.push((path, args)),
            }
        }
        Ok(out)
    }
}

pub struct ClientLister {
    svc: ClientService,
    prefix: String,
    recursive: bool,
    limit: Option<usize>,
    entries: Option<std::vec::IntoIter<ListEntry>>,
}

impl oio::List for ClientLister {
    async fn next(&mut self) -> Result<Option<oio::Entry>> {
        if self.entries.is_none() {
            let r = self
                .svc
                .backend
                .call(ClientOp::List {
                    prefix: self.prefix.clone(),
                    recursive: self.recursive,
                    limit: self.limit,
                })
                .await?;
            self.entries = Some(r.entries.into_iter());
        }
        let Some(e) = self.entries.as_mut().and_then(|i| i.next()) else {
            return Ok(None);
        };
        let path = self.svc.rel(&e.key);
        let meta = if e.dir || e.key.ends_with('/') {
            MetadataBuilder::dir().build()
        } else {
            file_meta(e.size, None)
        };
        Ok(Some(oio::Entry::new(&path, meta)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// An in-memory backend: the service and its operator, end to end.
    #[derive(Debug, Default)]
    struct Mem(Mutex<BTreeMap<String, Bytes>>);

    #[derive(Debug, Default)]
    struct MemBackend(Arc<Mem>);

    impl Backend for MemBackend {
        fn call(&self, op: ClientOp) -> BoxFuture<'static, Result<ClientReply>> {
            let m = self.0.clone();
            Box::pin(async move {
                let mut map = m.0.lock().unwrap();
                Ok(match op {
                    ClientOp::Get { key, range } => {
                        let v = map
                            .get(&key)
                            .ok_or_else(|| error_for(404, "NoSuchKey", ""))?;
                        let n = v.len() as u64;
                        let (a, b) = match range {
                            GetRange::Whole => (0, n),
                            GetRange::From { offset, length } => {
                                (offset, length.map_or(n, |l| (offset + l).min(n)))
                            }
                            GetRange::Suffix(s) => (n.saturating_sub(s), n),
                        };
                        let d = v.slice(a as usize..b as usize);
                        ClientReply {
                            bytes: d.len() as u64,
                            data: Some(Buffer::from(d)),
                            size: Some(n),
                            ..Default::default()
                        }
                    }
                    ClientOp::Put {
                        key,
                        data,
                        if_none_match,
                        ..
                    } => {
                        if if_none_match.as_deref() == Some("*") && map.contains_key(&key) {
                            return Err(error_for(412, "PreconditionFailed", ""));
                        }
                        let n = data.len() as u64;
                        map.insert(key, data);
                        ClientReply {
                            bytes: n,
                            etag: Some(format!("\"{n}\"")),
                            ..Default::default()
                        }
                    }
                    ClientOp::Head { key } => ClientReply {
                        size: Some(
                            map.get(&key).ok_or_else(|| error_for(404, "", ""))?.len() as u64
                        ),
                        ..Default::default()
                    },
                    ClientOp::Delete { keys } => {
                        for k in keys {
                            map.remove(&k);
                        }
                        ClientReply::default()
                    }
                    ClientOp::List { prefix, .. } => ClientReply {
                        entries: map
                            .iter()
                            .filter(|(k, _)| k.starts_with(&prefix))
                            .map(|(k, v)| ListEntry {
                                key: k.clone(),
                                size: v.len() as u64,
                                dir: false,
                            })
                            .collect(),
                        ..Default::default()
                    },
                    ClientOp::Copy { source, key } => {
                        let v = map
                            .get(&source)
                            .cloned()
                            .ok_or_else(|| error_for(404, "", ""))?;
                        map.insert(key, v);
                        ClientReply::default()
                    }
                })
            })
        }
    }

    #[tokio::test]
    async fn the_operator_drives_the_backend() {
        let mem = Arc::new(Mem::default());
        let cap = Capability {
            read: true,
            read_with_suffix: true,
            write: true,
            write_can_empty: true,
            write_with_if_none_match: true,
            stat: true,
            delete: true,
            delete_max_size: Some(100),
            list: true,
            list_with_recursive: true,
            copy: true,
            ..Default::default()
        };
        let op = ClientService::new(Arc::new(MemBackend(mem.clone())), "test", "/bench", cap)
            .into_operator()
            .unwrap();
        op.write("a/x.bin", vec![1u8; 10]).await.unwrap();
        assert!(mem.0.lock().unwrap().contains_key("bench/a/x.bin"));
        assert_eq!(op.stat("a/x.bin").await.unwrap().content_length(), 10);
        assert_eq!(op.read_with("a/x.bin").range(2..6).await.unwrap().len(), 4);
        op.copy("a/x.bin", "a/y.bin").await.unwrap();
        let listed = op.list_with("a/").recursive(true).await.unwrap();
        assert!(listed.iter().any(|e| e.path() == "a/y.bin"), "{listed:?}");
        op.delete_iter(vec!["a/x.bin".to_owned(), "a/y.bin".to_owned()])
            .await
            .unwrap();
        assert!(mem.0.lock().unwrap().is_empty());
        let e = op.stat("a/x.bin").await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::NotFound);
    }
}
