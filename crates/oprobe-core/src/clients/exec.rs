//! `--client exec:CMD`: an external adapter speaking the oprobe client protocol
//! ([`super::protocol`], `docs/clients.md`).
//!
//! One adapter process serves the whole session: operations are multiplexed over its stdin
//! by `id`, results come back on stdout in any order. Each result becomes request records in
//! the operation's scope (one per attempt or request the adapter reports), timed by the
//! adapter's own clock, so the protocol's cost is not counted as the SDK's.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use futures::future::BoxFuture;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use super::protocol::*;
use super::service::{Backend, ClientOp, ClientReply, GetRange, ListEntry, error_for};
use crate::config::{Knobs, TargetSpec};
use crate::errors::{ErrorCode, OprobeError};
use crate::trace::{Api, OpScope, Protocol, RequestRecord, RequestTiming};

/// How long an adapter may take to answer `hello` (SDK imports can be slow).
const HELLO_TIMEOUT: Duration = Duration::from_secs(60);
/// stderr lines kept to explain a dead adapter.
const STDERR_TAIL: usize = 20;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<WireResult>>>>;

/// A running adapter.
#[derive(Debug)]
pub struct ExecBackend {
    command: String,
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    pub hello: HelloReply,
    /// Why the adapter is gone (its exit and the end of its stderr).
    dead: Arc<Mutex<Option<String>>>,
    child: Mutex<Option<Child>>,
}

/// The protocol's view of `target`, and the environment that carries its credentials.
pub fn protocol_target(target: &TargetSpec) -> (ProtocolTarget, Vec<(String, String)>) {
    let opts = target.resolved_options().unwrap_or_default();
    let get = |k: &str| opts.get(k).filter(|v| !v.is_empty()).cloned();
    let endpoint = get("endpoint");
    let path_style = match get("enable_virtual_host_style").as_deref() {
        Some("true") => false,
        // Custom endpoints (S3-compatible services, the lab) are path-style by default, as in
        // OpenDAL; AWS virtual-hosted.
        _ => endpoint.is_some(),
    };
    let mut env = Vec::new();
    for (opt, var) in [
        ("access_key_id", "AWS_ACCESS_KEY_ID"),
        ("secret_access_key", "AWS_SECRET_ACCESS_KEY"),
        ("session_token", "AWS_SESSION_TOKEN"),
        ("region", "AWS_REGION"),
    ] {
        if let Some(v) = get(opt) {
            env.push((var.to_owned(), v));
        }
    }
    (
        ProtocolTarget {
            scheme: target.scheme.clone(),
            bucket: get("bucket"),
            endpoint,
            region: get("region"),
            path_style,
            insecure: target.insecure,
            ca_cert_file: target.ca_cert.as_ref().map(|p| p.display().to_string()),
        },
        env,
    )
}

fn adapter_error(command: &str, msg: impl std::fmt::Display) -> anyhow::Error {
    OprobeError::new(
        ErrorCode::UsageInvalidArgument,
        format!("the client adapter `{command}` {msg}"),
    )
    .with_hint("check the adapter command and run it by hand; see docs/clients.md")
    .into()
}

impl ExecBackend {
    /// Spawn `command` and exchange `hello`.
    pub async fn spawn(command: &str, target: &TargetSpec, knobs: &Knobs) -> Result<Self> {
        let argv = split_command(command).map_err(|e| adapter_error(command, e))?;
        let (pt, env) = protocol_target(target);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .envs(env)
            .env("OPROBE_CLIENT_PROTOCOL", PROTOCOL)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| adapter_error(command, format!("cannot start: {e}")))?;
        let mut stdin = child.stdin.take().context("adapter stdin")?;
        let stdout = child.stdout.take().context("adapter stdout")?;
        let stderr = child.stderr.take().context("adapter stderr")?;

        let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        {
            let tail = tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                    if t.len() == STDERR_TAIL {
                        t.pop_front();
                    }
                    t.push_back(l);
                }
            });
        }
        let tail_text = {
            let tail = tail.clone();
            move || {
                let t = tail.lock().unwrap_or_else(|e| e.into_inner());
                if t.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; its stderr ends with: {}",
                        t.iter().cloned().collect::<Vec<_>>().join(" | ")
                    )
                }
            }
        };

        let hello = HelloRequest {
            kind: "hello".into(),
            protocol: PROTOCOL.into(),
            target: pt,
            concurrency_count: knobs.concurrency.max(1),
            retries_count: knobs.retries,
            timeout_ms: knobs.timeout.get().as_millis() as u64,
        };
        let line = serde_json::to_string(&hello)? + "\n";
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| adapter_error(command, format!("closed its stdin: {e}")))?;
        stdin.flush().await.ok();
        let mut lines = BufReader::new(stdout).lines();
        let first = tokio::time::timeout(HELLO_TIMEOUT, lines.next_line())
            .await
            .map_err(|_| {
                adapter_error(
                    command,
                    format!(
                        "did not answer `hello` within {}s{}",
                        HELLO_TIMEOUT.as_secs(),
                        tail_text()
                    ),
                )
            })?
            .map_err(|e| adapter_error(command, e))?;
        // Give stderr a moment so a crash explains itself.
        let Some(first) = first else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            return Err(adapter_error(
                command,
                format!("exited before answering `hello`{}", tail_text()),
            ));
        };
        let reply: HelloReply = serde_json::from_str(&first).map_err(|e| {
            adapter_error(
                command,
                format!("answered `hello` with invalid JSON ({e}): {first}"),
            )
        })?;
        // The major version must match (`oprobe.client/1`, `oprobe.client/1.2`).
        let major = reply
            .protocol
            .strip_prefix("oprobe.client/")
            .and_then(|v| v.split('.').next());
        if major != Some("1") {
            return Err(adapter_error(
                command,
                format!("speaks `{}`, oprobe speaks `{PROTOCOL}`", reply.protocol),
            ));
        }

        // Writer: one line per message, in order.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(l) = rx.recv().await {
                if stdin.write_all(l.as_bytes()).await.is_err() {
                    break;
                }
                // Batch whatever is already queued before flushing.
                while let Ok(more) = rx.try_recv() {
                    if stdin.write_all(more.as_bytes()).await.is_err() {
                        return;
                    }
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });
        // Reader: route each result to its caller.
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(Mutex::new(None::<String>));
        {
            let pending = pending.clone();
            let dead = dead.clone();
            let command = command.to_owned();
            tokio::spawn(async move {
                let why = loop {
                    match lines.next_line().await {
                        Ok(Some(l)) if l.trim().is_empty() => continue,
                        Ok(Some(l)) => match serde_json::from_str::<WireResult>(&l) {
                            Ok(r) => {
                                let tx = pending
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .remove(&r.id);
                                if let Some(tx) = tx {
                                    let _ = tx.send(r);
                                }
                            }
                            Err(e) => break format!("wrote an invalid result line ({e}): {l}"),
                        },
                        Ok(None) => break "exited".to_owned(),
                        Err(e) => break format!("cannot be read: {e}"),
                    }
                };
                tokio::time::sleep(Duration::from_millis(100)).await;
                *dead.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!(
                    "the client adapter `{command}` {why}{}",
                    tail_text()
                ));
                // Fail everything still waiting (their senders drop).
                pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
            });
        }
        Ok(Self {
            command: command.to_owned(),
            tx,
            pending,
            next_id: AtomicU64::new(1),
            hello: reply,
            dead,
            child: Mutex::new(Some(child)),
        })
    }

    /// The adapter's capability, as an OpenDAL service.
    pub fn capability(&self) -> opendal::Capability {
        let h = &self.hello;
        opendal::Capability {
            stat: h.has_op("head"),
            read: h.has_op("get"),
            read_with_suffix: h.has_op("get"),
            write: h.has_op("put"),
            write_can_empty: h.has_op("put"),
            write_with_content_type: h.has_op("put"),
            write_with_if_match: h.has_feature("if_match"),
            write_with_if_none_match: h.has_feature("if_none_match"),
            write_with_if_not_exists: h.has_feature("if_none_match"),
            write_with_user_metadata: h.has_feature("metadata"),
            delete: h.has_op("delete"),
            delete_max_size: Some(if h.has_feature("batch_delete") {
                1000
            } else {
                1
            }),
            list: h.has_op("list"),
            list_with_recursive: h.has_op("list"),
            list_with_limit: h.has_op("list"),
            copy: h.has_op("copy"),
            shared: true,
            ..Default::default()
        }
    }

    fn gone(&self) -> opendal::Error {
        let why = self
            .dead
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| format!("the client adapter `{}` stopped", self.command));
        opendal::Error::new(opendal::ErrorKind::Unexpected, why)
    }

    /// Ask the adapter to exit and wait briefly for it.
    pub async fn shutdown(&self) {
        let _ = self.tx.send("{\"type\":\"shutdown\"}\n".into());
        let child = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut c) = child
            && tokio::time::timeout(Duration::from_secs(2), c.wait())
                .await
                .is_err()
        {
            let _ = c.kill().await;
        }
    }
}

impl Drop for ExecBackend {
    fn drop(&mut self) {
        let _ = self.tx.send("{\"type\":\"shutdown\"}\n".into());
    }
}

/// The wire form of `op`, and the API its requests are recorded as.
fn wire(id: u64, op: &ClientOp) -> (WireOp, Api, u64) {
    let b64 = base64::engine::general_purpose::STANDARD;
    match op {
        ClientOp::Get { key, range } => (
            WireOp {
                key: Some(key.clone()),
                range: match *range {
                    GetRange::Whole => None,
                    GetRange::From { offset, length } => Some(WireRange {
                        offset: Some(offset),
                        length,
                        suffix: None,
                    }),
                    GetRange::Suffix(n) => Some(WireRange {
                        offset: None,
                        length: None,
                        suffix: Some(n),
                    }),
                },
                ..WireOp::new(id, OpName::Get)
            },
            Api::GetObject,
            0,
        ),
        ClientOp::Put {
            key,
            data,
            if_match,
            if_none_match,
            content_type,
            metadata,
        } => (
            WireOp {
                key: Some(key.clone()),
                size: Some(data.len() as u64),
                data_b64: Some(b64.encode(data)),
                if_match: if_match.clone(),
                if_none_match: if_none_match.clone(),
                content_type: content_type.clone(),
                metadata: metadata.iter().cloned().collect(),
                ..WireOp::new(id, OpName::Put)
            },
            Api::PutObject,
            data.len() as u64,
        ),
        ClientOp::Head { key } => (
            WireOp {
                key: Some(key.clone()),
                ..WireOp::new(id, OpName::Head)
            },
            Api::HeadObject,
            0,
        ),
        ClientOp::Delete { keys } => (
            WireOp {
                keys: keys.clone(),
                ..WireOp::new(id, OpName::Delete)
            },
            if keys.len() > 1 {
                Api::DeleteObjects
            } else {
                Api::DeleteObject
            },
            0,
        ),
        ClientOp::List {
            prefix,
            recursive,
            limit,
        } => (
            WireOp {
                prefix: Some(prefix.clone()),
                recursive: Some(*recursive),
                limit: limit.map(|l| l as u64),
                ..WireOp::new(id, OpName::List)
            },
            Api::ListObjects,
            0,
        ),
        ClientOp::Copy { source, key } => (
            WireOp {
                key: Some(key.clone()),
                source: Some(source.clone()),
                ..WireOp::new(id, OpName::Copy)
            },
            Api::CopyObject,
            0,
        ),
    }
}

fn timing(total_us: u64, phases: Option<&WirePhases>) -> RequestTiming {
    match phases {
        None => RequestTiming {
            ttfb_us: total_us,
            total_us,
            ..Default::default()
        },
        Some(p) => {
            let (dns, tcp, tls) = if p.dns_us + p.tcp_us + p.tls_us > 0 {
                (p.dns_us, p.tcp_us, p.tls_us)
            } else {
                (0, p.connect_us, 0)
            };
            let sum = p.queue_us + dns + tcp + tls + p.send_us + p.ttfb_us + p.body_us;
            RequestTiming {
                queue_us: p.queue_us,
                dns_us: dns,
                tcp_us: tcp,
                proxy_us: 0,
                tls_us: tls,
                send_us: p.send_us,
                // Whatever the client did not attribute counts as waiting for the server.
                ttfb_us: p.ttfb_us + total_us.saturating_sub(sum),
                body_us: p.body_us,
                stall_us: 0,
                total_us: total_us.max(sum),
            }
        }
    }
}

/// The request records of one result: one per reported attempt, else one per request
/// (`requests`, the time split evenly), the last with the final status and phases.
pub fn records(
    api: Api,
    r: &WireResult,
    bytes_up: u64,
    start_offset_us: u64,
) -> (Vec<RequestRecord>, u64) {
    let protocol = match r.http_version.as_deref() {
        Some("http2" | "h2" | "2") => Protocol::Http2,
        _ => Protocol::Http1,
    };
    let err_text = r
        .error
        .as_ref()
        .map(|e| format!("{} {}", e.code, e.message).trim().to_owned());
    let final_status = if r.status == 0 && r.ok { 200 } else { r.status };
    // The service's error code of a failed final attempt (`SlowDown`, `NoSuchKey`, …), as the
    // fidelity transport reads it from the error body.
    let final_detail = r
        .error
        .as_ref()
        .filter(|e| !r.ok && !e.code.is_empty())
        .map(|e| {
            Box::new(crate::trace::ReqDetail {
                error_code: Some(e.code.clone()),
                ..Default::default()
            })
        });
    let rec =
        |status: u16, offset: u64, t: RequestTiming, up: u64, down: u64, err: Option<String>| {
            RequestRecord {
                api,
                method: api.label().to_owned(),
                status,
                protocol,
                reused: t.connect_us() == 0,
                conn_id: 0,
                start_offset_us: offset,
                timing: t,
                bytes_up: up,
                bytes_down: down,
                request_id: None,
                retry_after_ms: None,
                error: err,
                remote: None,
                service_op: None,
                tls: None,
                tcp: None,
                detail: None,
            }
        };
    let with_detail = |mut v: Vec<RequestRecord>| {
        if let Some(last) = v.last_mut() {
            last.detail = final_detail.clone();
        }
        v
    };
    let down = if bytes_up > 0 { 0 } else { r.bytes };
    if !r.attempt_log.is_empty() {
        let t0 = r.attempt_log[0].t_start_us;
        let n = r.attempt_log.len();
        let out = r
            .attempt_log
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let last = i + 1 == n;
                let total = a.t_end_us.saturating_sub(a.t_start_us);
                let phases = a
                    .phases
                    .as_ref()
                    .or(if last { r.phases.as_ref() } else { None });
                let ok = (200..400).contains(&a.status);
                rec(
                    a.status,
                    start_offset_us + a.t_start_us.saturating_sub(t0),
                    timing(total, phases),
                    if last || !ok { bytes_up } else { 0 },
                    if last { down } else { 0 },
                    a.error
                        .clone()
                        .or_else(|| (last && !r.ok).then(|| err_text.clone()).flatten()),
                )
            })
            .collect();
        return (with_detail(out), 0);
    }
    let n = r.requests.max(1) as u64;
    let total = r.total_us();
    let each = total / n;
    let out = (0..n)
        .map(|i| {
            let last = i + 1 == n;
            let t = if last {
                timing(total - each * (n - 1), r.phases.as_ref())
            } else {
                timing(each, None)
            };
            rec(
                if last { final_status } else { 200 },
                start_offset_us + each * i,
                t,
                if last { bytes_up } else { 0 },
                if last { down } else { 0 },
                if last && !r.ok {
                    err_text.clone()
                } else {
                    None
                },
            )
        })
        .collect();
    // Retries the adapter counted without detail.
    (with_detail(out), r.attempts.saturating_sub(1) as u64)
}

impl Backend for Arc<ExecBackend> {
    fn call(&self, op: ClientOp) -> BoxFuture<'static, opendal::Result<ClientReply>> {
        let this = self.clone();
        Box::pin(async move {
            let id = this.next_id.fetch_add(1, Ordering::Relaxed);
            let (w, api, up) = wire(id, &op);
            let (tx, rx) = oneshot::channel();
            this.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, tx);
            let scope = OpScope::current();
            let started = Instant::now();
            let line = serde_json::to_string(&w)
                .map_err(|e| opendal::Error::new(opendal::ErrorKind::Unexpected, e.to_string()))?
                + "\n";
            if this.tx.send(line).is_err() {
                this.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                return Err(this.gone());
            }
            let r = match rx.await {
                Ok(r) => r,
                Err(_) => return Err(this.gone()),
            };
            if let Some(s) = &scope {
                let offset = started.saturating_duration_since(s.started).as_micros() as u64;
                let (recs, retries) = records(api, &r, up, offset);
                for rec in recs {
                    s.push_request(rec);
                }
                if retries > 0 {
                    s.with(|d| {
                        if !d.closed {
                            d.retries += retries;
                        }
                    });
                }
            }
            if !r.ok {
                let (code, msg) = r
                    .error
                    .as_ref()
                    .map(|e| (e.code.as_str(), e.message.as_str()))
                    .unwrap_or(("", ""));
                return Err(error_for(r.status, code, msg));
            }
            Ok(ClientReply {
                data: None,
                bytes: r.bytes,
                size: r.size,
                etag: r.etag,
                entries: r
                    .entries
                    .into_iter()
                    .map(|e| ListEntry {
                        key: e.key,
                        size: e.size,
                        dir: e.dir,
                    })
                    .collect(),
                failed_keys: r
                    .failed_keys
                    .into_iter()
                    .map(|k| (k, "not deleted".to_owned()))
                    .collect(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_become_request_records() {
        let r = WireResult {
            id: 1,
            ok: true,
            status: 200,
            bytes: 100,
            t_start_us: 1_000,
            t_end_us: 3_000,
            phases: Some(WirePhases {
                connect_us: 500,
                ttfb_us: 1_000,
                body_us: 200,
                ..Default::default()
            }),
            attempts: 2,
            requests: 1,
            ..Default::default()
        };
        let (recs, retries) = records(Api::GetObject, &r, 0, 10);
        assert_eq!(retries, 1);
        assert_eq!(recs.len(), 1);
        let t = recs[0].timing;
        assert_eq!(t.total_us, 2_000);
        assert_eq!(t.tcp_us, 500);
        assert_eq!(t.ttfb_us, 1_000 + 300);
        assert_eq!(recs[0].bytes_down, 100);
        assert!(!recs[0].reused);

        let r = WireResult {
            ok: true,
            status: 200,
            attempt_log: vec![
                WireAttempt {
                    status: 503,
                    t_start_us: 0,
                    t_end_us: 100,
                    ..Default::default()
                },
                WireAttempt {
                    status: 200,
                    t_start_us: 300,
                    t_end_us: 500,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let (recs, retries) = records(Api::PutObject, &r, 64, 0);
        assert_eq!((recs.len(), retries), (2, 0));
        assert!(recs[0].is_throttle());
        assert_eq!(recs[1].start_offset_us, 300);
        assert_eq!(recs[1].bytes_up, 64);
    }

    #[test]
    fn a_failed_result_carries_the_service_error_code() {
        let r = WireResult {
            ok: false,
            status: 503,
            error: Some(WireError {
                code: "SlowDown".into(),
                message: "Please reduce your request rate.".into(),
            }),
            ..Default::default()
        };
        let (recs, _) = records(Api::GetObject, &r, 0, 0);
        let d = recs.last().and_then(|x| x.detail.as_ref()).expect("detail");
        assert_eq!(d.error_code.as_deref(), Some("SlowDown"));
        // A success carries none.
        let ok = WireResult {
            ok: true,
            status: 200,
            ..Default::default()
        };
        assert!(records(Api::GetObject, &ok, 0, 0).0[0].detail.is_none());
    }

    #[test]
    fn targets_map_to_the_protocol_and_the_environment() {
        let mut t = TargetSpec::from_url("s3://b/p").unwrap();
        t.options
            .insert("endpoint".into(), "http://127.0.0.1:9000".into());
        t.options.insert("access_key_id".into(), "AK".into());
        let (pt, env) = protocol_target(&t);
        assert_eq!(pt.bucket.as_deref(), Some("b"));
        assert!(pt.path_style);
        assert!(env.contains(&("AWS_ACCESS_KEY_ID".into(), "AK".into())));
    }
}
