//! The oprobe client protocol, `oprobe.client/1` (`--client exec:CMD`, design v5 §7).
//!
//! oprobe spawns the adapter and talks NDJSON over its stdin and stdout: one JSON object per
//! line. The first line each way is a `hello`; then oprobe sends operations (each with an `id`)
//! and the adapter answers each with one result line carrying the same `id`, in any order
//! (adapters may run operations concurrently). `{"type":"shutdown"}` (or EOF on stdin) ends the
//! session. The adapter writes diagnostics to stderr, never to stdout.
//!
//! The full reference, with examples, is `docs/clients.md`; these types are the contract.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The protocol name and major version (`hello.protocol`).
pub const PROTOCOL: &str = "oprobe.client/1";

/// What oprobe sends first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HelloRequest {
    /// Always `hello`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `oprobe.client/1`.
    pub protocol: String,
    pub target: ProtocolTarget,
    /// Operations oprobe keeps in flight at most (size the adapter's pool to it).
    pub concurrency_count: usize,
    /// Retries the client should make after a failed attempt (`knobs.retries`).
    pub retries_count: usize,
    /// Per-operation timeout (`knobs.timeout`).
    pub timeout_ms: u64,
}

/// Where the adapter sends its requests. Credentials are not part of the protocol: they are
/// in the adapter's environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
/// `AWS_SESSION_TOKEN`, `AWS_REGION`), set from the target when it has them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProtocolTarget {
    /// OpenDAL scheme of the target (`s3` for every reference adapter).
    pub scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// Custom endpoint (S3-compatible services, the lab); absent: the provider's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Path-style addressing (`https://endpoint/bucket/key`) instead of virtual hosts.
    #[serde(default)]
    pub path_style: bool,
    /// Skip TLS certificate verification (self-signed development endpoints).
    #[serde(default)]
    pub insecure: bool,
    /// Extra PEM CA bundle to trust (a file path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_file: Option<String>,
}

/// The adapter's answer to `hello`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HelloReply {
    /// `hello`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The protocol the adapter speaks (`oprobe.client/1`); the major version must match.
    pub protocol: String,
    /// The client library, e.g. `boto3`, `awscrt`, `aws-sdk-go-v2`.
    pub client: String,
    /// Its version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Request stages the adapter measures (`queue`, `connect`, `send`, `ttfb`, `body`):
    /// findings that need other stages are suppressed.
    #[serde(default)]
    pub phases: Vec<String>,
    /// Operations it implements (default: all of `get put head delete list copy`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<String>,
    /// Optional features: `if_match`, `if_none_match`, `metadata`, `batch_delete`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

impl HelloReply {
    /// Whether the adapter implements `op` (an empty list means every operation).
    pub fn has_op(&self, op: &str) -> bool {
        self.ops.is_empty() || self.ops.iter().any(|o| o == op)
    }

    pub fn has_feature(&self, f: &str) -> bool {
        self.features.iter().any(|x| x == f)
    }
}

/// The operations of the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpName {
    /// GET an object or a byte range of it.
    Get,
    /// PUT an object (`data_b64`, `size` bytes).
    Put,
    /// HEAD an object.
    Head,
    /// Delete `keys` (one request per key, or a batch delete when the adapter can).
    Delete,
    /// List under `prefix` (all pages; `recursive`: no delimiter; `limit`: page size).
    List,
    /// Server-side copy `source` → `key`.
    Copy,
}

/// A byte range of a `get`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WireRange {
    /// First byte (absent with `suffix`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Bytes from `offset` (absent: to the end).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    /// The last N bytes (`Range: bytes=-N`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<u64>,
}

/// One operation, oprobe → adapter. Keys are full object keys (the target's root included).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WireOp {
    pub id: u64,
    pub op: OpName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// `delete`: the keys to delete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// `copy`: the source key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// `get`: the byte range (absent: the whole object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<WireRange>,
    /// `put`: payload size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// `put`: the payload, base64 (standard alphabet, padded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_match: Option<String>,
    /// `*`: create only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_none_match: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// User metadata (`x-amz-meta-*`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// `list`: the prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursive: Option<bool>,
    /// `list`: page size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

impl WireOp {
    pub fn new(id: u64, op: OpName) -> Self {
        Self {
            id,
            op,
            key: None,
            keys: Vec::new(),
            source: None,
            range: None,
            size: None,
            data_b64: None,
            if_match: None,
            if_none_match: None,
            content_type: None,
            metadata: BTreeMap::new(),
            prefix: None,
            recursive: None,
            limit: None,
        }
    }
}

/// Stage durations of one request, as far as the client can see them (µs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WirePhases {
    /// Waiting for a connection or a pool slot.
    #[serde(default)]
    pub queue_us: u64,
    #[serde(default)]
    pub dns_us: u64,
    #[serde(default)]
    pub tcp_us: u64,
    #[serde(default)]
    pub tls_us: u64,
    /// DNS + TCP + TLS when the client cannot split them.
    #[serde(default)]
    pub connect_us: u64,
    #[serde(default)]
    pub send_us: u64,
    #[serde(default)]
    pub ttfb_us: u64,
    #[serde(default)]
    pub body_us: u64,
}

/// One attempt of a retried operation (optional detail of a result).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WireAttempt {
    /// HTTP status; 0: no response (network error).
    #[serde(default)]
    pub status: u16,
    pub t_start_us: u64,
    pub t_end_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phases: Option<WirePhases>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A listed key.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WireEntry {
    pub key: String,
    #[serde(default)]
    pub size: u64,
    /// A common prefix (non-recursive listings).
    #[serde(default)]
    pub dir: bool,
}

/// Why an operation failed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WireError {
    /// The service's error code (`NoSuchKey`, `SlowDown`, `PreconditionFailed`, …) or the
    /// client's exception name.
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

/// One result, adapter → oprobe.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WireResult {
    pub id: u64,
    pub ok: bool,
    /// HTTP status of the final attempt; 0: no response.
    #[serde(default)]
    pub status: u16,
    /// Payload bytes transferred (downloaded for `get`, uploaded for `put`).
    #[serde(default)]
    pub bytes: u64,
    /// The object's total size (`get`, `head`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Start and end of the operation inside the client, on any monotonic clock (µs). oprobe
    /// uses only their difference: the time the SDK took, without the protocol's own cost.
    #[serde(default)]
    pub t_start_us: u64,
    #[serde(default)]
    pub t_end_us: u64,
    /// Stages of the final attempt, when the client can observe them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phases: Option<WirePhases>,
    /// Attempts made (1 + retries).
    #[serde(default = "one")]
    pub attempts: u32,
    /// HTTP requests the operation issued (list pages, batch deletes); default 1.
    #[serde(default = "one")]
    pub requests: u32,
    /// Every attempt, when the client reports them (status, timing).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempt_log: Vec<WireAttempt>,
    /// `list`: the keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<WireEntry>,
    /// `delete`: keys that could not be deleted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_keys: Vec<String>,
    /// `http1` or `http2`, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

fn one() -> u32 {
    1
}

impl WireResult {
    /// Microseconds the operation took inside the client.
    pub fn total_us(&self) -> u64 {
        self.t_end_us.saturating_sub(self.t_start_us)
    }
}

/// Split a command line into words: whitespace separates, single and double quotes group,
/// a backslash escapes the next character (outside single quotes). No shell is involved.
pub fn split_command(cmd: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c) => cur.push(c),
                            None => return Err("trailing backslash".into()),
                        },
                        Some(c) => cur.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(c) => cur.push(c),
                    None => return Err("trailing backslash".into()),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    if words.is_empty() {
        return Err("empty command".into());
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_commands_without_a_shell() {
        assert_eq!(
            split_command("python3 -u 'my adapter.py' --x=\"a b\" c\\ d").unwrap(),
            vec!["python3", "-u", "my adapter.py", "--x=a b", "c d"]
        );
        assert!(split_command("  ").is_err());
        assert!(split_command("a 'b").is_err());
    }

    #[test]
    fn results_default_to_one_attempt_and_request() {
        let r: WireResult = serde_json::from_str(r#"{"id":3,"ok":true,"status":200}"#).unwrap();
        assert_eq!((r.attempts, r.requests), (1, 1));
        let op = WireOp {
            key: Some("k".into()),
            ..WireOp::new(1, OpName::Get)
        };
        assert_eq!(
            serde_json::to_string(&op).unwrap(),
            r#"{"id":1,"op":"get","key":"k"}"#
        );
    }
}
