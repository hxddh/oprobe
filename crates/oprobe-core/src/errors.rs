//! Classified errors (design §8): a stable dotted [`ErrorCode`], an [`ErrorBody`] for machine
//! output, and the exit-code mapping every command uses. They live here, not in `oprobe-trace`:
//! the tracing library carries only the small [`oprobe_trace::rows::FailureCode`] vocabulary of
//! its rows, which converts into an [`ErrorCode`].
//!
//! Raise a classified error with [`OprobeError`] (or the [`fail!`](crate::fail) macro) where the
//! error happens; user input is parsed by [`crate::input`], whose errors are typed. Any other
//! `anyhow::Error` is classified at the CLI boundary by [`classify`] from typed sources only
//! (OpenDAL error kinds, dial/TLS/timeout errors, I/O kinds); there is no message matching, so
//! an untyped error is `internal` — a bug to fix where it is raised.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Exit codes (design v6 §8).
pub mod exit {
    pub const OK: u8 = 0;
    /// Runtime failure: auth, network, storage, internal.
    pub const RUNTIME: u8 = 1;
    /// Usage or configuration error.
    pub const USAGE: u8 = 2;
    /// The change is worse on the objective (`prove` refuted it).
    pub const GATE: u8 = 3;
    /// A budget stopped the run.
    pub const BUDGET: u8 = 4;
    /// A safety rail refused the command.
    pub const SAFETY: u8 = 5;
    /// Interrupted (Ctrl-C).
    pub const INTERRUPTED: u8 = 130;
}

macro_rules! codes {
    ($($variant:ident => $s:literal, $exit:expr, $retry:expr;)*) => {
        /// Stable error code. Serialized as its dotted string (e.g. `net.timeout`).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
        pub enum ErrorCode {
            $( #[serde(rename = $s)] $variant, )*
        }

        impl ErrorCode {
            /// Every code, in contract order.
            pub const ALL: &'static [ErrorCode] = &[$(ErrorCode::$variant),*];

            pub fn as_str(self) -> &'static str {
                match self { $(ErrorCode::$variant => $s,)* }
            }

            /// Process exit code for a command failing with this error.
            pub fn exit_code(self) -> u8 {
                match self { $(ErrorCode::$variant => $exit,)* }
            }

            /// Whether retrying the same command later may succeed.
            pub fn retryable(self) -> bool {
                match self { $(ErrorCode::$variant => $retry,)* }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some(ErrorCode::$variant),)* _ => None }
            }
        }
    };
}

codes! {
    UsageInvalidArgument => "usage.invalid_argument", exit::USAGE, false;
    UsageUnknownTarget => "usage.unknown_target", exit::USAGE, false;
    UsageUnknownRun => "usage.unknown_run", exit::USAGE, false;
    UsageUnknownBaseline => "usage.unknown_baseline", exit::USAGE, false;
    ConfigInvalid => "config.invalid", exit::USAGE, false;
    ConfigUnknownOption => "config.unknown_option", exit::USAGE, false;
    AuthNoCredentials => "auth.no_credentials", exit::RUNTIME, false;
    AuthDenied => "auth.denied", exit::RUNTIME, false;
    NetUnreachable => "net.unreachable", exit::RUNTIME, true;
    NetDns => "net.dns", exit::RUNTIME, true;
    NetTls => "net.tls", exit::RUNTIME, false;
    NetTimeout => "net.timeout", exit::RUNTIME, true;
    NetRateLimited => "net.rate_limited", exit::RUNTIME, true;
    NetHttp => "net.http", exit::RUNTIME, false;
    StorageNotFound => "storage.not_found", exit::RUNTIME, false;
    StorageThrottled => "storage.throttled", exit::RUNTIME, true;
    StorageUnsupported => "storage.unsupported", exit::RUNTIME, false;
    SafetyBudgetExceeded => "safety.budget_exceeded", exit::BUDGET, false;
    SafetyReadOnly => "safety.read_only", exit::SAFETY, false;
    SafetyRefused => "safety.refused", exit::SAFETY, false;
    RunSlaFailed => "run.sla_failed", exit::GATE, false;
    RunRegression => "run.regression", exit::GATE, false;
    RunInterrupted => "run.interrupted", exit::INTERRUPTED, true;
    Internal => "internal", exit::RUNTIME, false;
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ErrorCode {
    /// The code of an operation that failed with error class `class` (`engine::ops::error_class`).
    pub fn from_class(class: &str) -> Self {
        match class {
            "rate_limited" => Self::StorageThrottled,
            "permission_denied" => Self::AuthDenied,
            "not_found" => Self::StorageNotFound,
            "config_invalid" => Self::ConfigInvalid,
            "unsupported" => Self::StorageUnsupported,
            "timeout" | "tail_cut" => Self::NetTimeout,
            "connect" | "network" => Self::NetUnreachable,
            _ => Self::Internal,
        }
    }

    /// The code of a backend request answered with HTTP `status` (0: no response).
    pub fn from_status(status: u16) -> Option<Self> {
        match status {
            0 => Some(Self::NetUnreachable),
            429 | 503 => Some(Self::StorageThrottled),
            401 | 403 => Some(Self::AuthDenied),
            404 => Some(Self::StorageNotFound),
            400..=499 => Some(Self::Internal),
            500..=599 => Some(Self::Internal),
            _ => None,
        }
    }

    /// The code of a non-storage HTTP API (GitHub releases, an OTLP collector) answering
    /// `status`: rate limits (429, or 403 with an exhausted quota) are `net.rate_limited`,
    /// anything else `net.http`. Never `storage.*` or `auth.*`: the storage target's
    /// credentials are not involved (design v4 §8).
    pub fn from_api_status(status: u16, rate_limited: bool) -> Self {
        match status {
            0 => Self::NetUnreachable,
            429 => Self::NetRateLimited,
            403 if rate_limited => Self::NetRateLimited,
            _ => Self::NetHttp,
        }
    }

    /// Documentation anchor for the code.
    /// The troubleshooting section of this code, as this version shipped it (pinned to the
    /// version tag, like the glossary).
    pub fn docs(self) -> String {
        crate::util::docs_url(&format!(
            "docs/troubleshooting.md#{}",
            self.as_str().replace('.', "-")
        ))
    }
}

/// Machine form of an error (`{"type":"error","error":{…}}` in json/ndjson output).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    /// The whole error chain as the storage library reported it, when `message` leads with
    /// the provider's error code or states the cause plainly instead (machine output only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
            retryable: code.retryable(),
            docs: Some(code.docs()),
            detail: None,
        }
    }

    /// The same error filed under `code`: its `retryable` flag and `docs` link follow the code
    /// (a failure caused by Ctrl-C becomes `run.interrupted`, not an `internal` one).
    pub fn with_code(mut self, code: ErrorCode) -> Self {
        self.code = code;
        self.retryable = code.retryable();
        self.docs = Some(code.docs());
        self
    }

    /// Lead with the provider's error code and message when the chain carries one
    /// (`credentials rejected: InvalidAccessKeyId: The AWS Access Key Id …`); the raw chain
    /// moves to `detail`.
    fn lead_with_provider(mut self) -> Self {
        if let Some(lead) = provider_lead(&self.message, self.code) {
            self.detail = Some(std::mem::replace(&mut self.message, lead));
        }
        self
    }

    pub fn exit_code(&self) -> u8 {
        self.code.exit_code()
    }
}

/// A classified error. Return it (as `anyhow::Error`) from any command; [`classify`] keeps its
/// code, message and hint.
#[derive(Debug)]
pub struct OprobeError {
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
    /// The storage library's own report, when `message` states the cause plainly (machine
    /// output only: [`ErrorBody::detail`]).
    pub detail: Option<String>,
}

impl OprobeError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
            detail: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl fmt::Display for OprobeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OprobeError {}

impl From<oprobe_trace::rows::FailureCode> for ErrorCode {
    fn from(c: oprobe_trace::rows::FailureCode) -> Self {
        ErrorCode::parse(c.as_str()).unwrap_or(ErrorCode::Internal)
    }
}

/// `return Err(fail!(ErrorCode::UsageUnknownRun, "no run `{}`", r))`: a classified
/// `anyhow::Error`.
#[macro_export]
macro_rules! fail {
    ($code:expr, $($arg:tt)*) => {
        ::anyhow::Error::new($crate::errors::OprobeError::new($code, format!($($arg)*)))
    };
}

/// Attach a classification to any fallible result.
pub trait Classify<T> {
    fn code(self, code: ErrorCode) -> anyhow::Result<T>;
}

impl<T, E: Into<anyhow::Error>> Classify<T> for Result<T, E> {
    fn code(self, code: ErrorCode) -> anyhow::Result<T> {
        self.map_err(|e| {
            let e: anyhow::Error = e.into();
            if e.downcast_ref::<OprobeError>().is_some() {
                return e;
            }
            let msg = format!("{e:#}");
            anyhow::Error::new(OprobeError::new(code, msg))
        })
    }
}

/// The value of XML element `tag` in `s` (`<Code>SlowDown</Code>`).
fn xml_value<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let i = s.find(&open)? + open.len();
    let j = s[i..].find("</")? + i;
    Some(s[i..j].trim()).filter(|v| !v.is_empty())
}

/// The value of a debug-printed field `name: "…"` in `s` (OpenDAL's `S3Error { code: "…" }`).
fn debug_field<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}: \"");
    let i = s.find(&key)? + key.len();
    let j = s[i..].find('"')? + i;
    Some(s[i..j].trim()).filter(|v| !v.is_empty())
}

/// The provider's error code and message in an error chain (S3 and Azure XML bodies, OpenDAL's
/// debug-printed service errors), as a short lead: what happened, then the code and message.
/// `None` when the chain carries no provider code.
pub fn provider_lead(chain: &str, code: ErrorCode) -> Option<String> {
    let (what, rest) = provider_parts(chain, code)?;
    Some(format!("{what}: {rest}"))
}

/// [`provider_lead`] in two parts: what happened (`credentials rejected`) and the provider's
/// code and message (`InvalidAccessKeyId: The AWS Access Key Id …`).
pub fn provider_parts(chain: &str, code: ErrorCode) -> Option<(&'static str, String)> {
    let (pcode, pmsg) = match (xml_value(chain, "Code"), debug_field(chain, "code")) {
        (Some(c), _) => (c, xml_value(chain, "Message")),
        (None, Some(c)) => (c, debug_field(chain, "message")),
        _ => return None,
    };
    if !pcode.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
        return None;
    }
    let what = match pcode {
        "InvalidAccessKeyId"
        | "SignatureDoesNotMatch"
        | "InvalidToken"
        | "ExpiredToken"
        | "InvalidClientTokenId"
        | "AuthenticationFailed"
        | "InvalidSecurity" => "credentials rejected",
        "AccessDenied" | "AuthorizationPermissionMismatch" | "AuthorizationFailure" => {
            "access denied"
        }
        "NoSuchBucket" | "ContainerNotFound" => "bucket not found",
        "SlowDown" | "ServerBusy" | "TooManyRequests" | "RequestLimitExceeded" => "throttled",
        _ => match code {
            ErrorCode::AuthDenied => "access denied",
            ErrorCode::AuthNoCredentials => "no credentials",
            ErrorCode::StorageNotFound => "not found",
            ErrorCode::StorageThrottled => "throttled",
            _ => "the service refused the request",
        },
    };
    Some((
        what,
        match pmsg {
            Some(m) => format!("{pcode}: {m}"),
            None => pcode.to_owned(),
        },
    ))
}

/// Classify an error chain: an [`OprobeError`] anywhere in the chain wins, then typed sources
/// (OpenDAL error kinds, dial/TLS/timeout errors), else `internal`. `scheme` selects
/// provider-specific hints.
pub fn classify(err: &anyhow::Error, scheme: &str) -> ErrorBody {
    let chain = format!("{err:#}");
    let hint_text = || {
        crate::analysis::hints::hint_for(&chain, scheme).map(|h| {
            if h.steps.is_empty() {
                h.title
            } else {
                format!("{}: {}", h.title, h.steps.join("; "))
            }
        })
    };
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<OprobeError>() {
            let mut body = ErrorBody::new(e.code, chain_message(err)).lead_with_provider();
            if body.detail.is_none() {
                body.detail = e.detail.as_deref().map(crate::util::strip_url_credentials);
            }
            body.hint = e
                .hint
                .as_deref()
                .map(crate::util::strip_url_credentials)
                .or_else(hint_text);
            return body;
        }
        if let Some(e) = cause.downcast_ref::<crate::net::ConfigError>() {
            let mut body = ErrorBody::new(ErrorCode::UsageInvalidArgument, chain_message(err));
            body.hint = e.hint().map(str::to_owned).or_else(hint_text);
            return body;
        }
    }
    let code = typed_code(err).unwrap_or(ErrorCode::Internal);
    let mut body = ErrorBody::new(code, chain_message(err)).lead_with_provider();
    body.hint = hint_text();
    body
}

/// The whole chain on one line (`outer: inner: root`), with URL credentials removed
/// (`user:pass@`, the signature and token of a presigned or SAS URL): OpenDAL contexts quote
/// endpoints as configured, and HTTP clients quote the URL of a failed request.
fn chain_message(err: &anyhow::Error) -> String {
    crate::util::strip_url_credentials(&format!("{err:#}"))
}

/// The code of an OpenDAL error, from its kind (and, for `Unexpected`, the failure class
/// `engine::ops::error_class_of` derives from its status and source). Credential failures
/// (`failed to load signing credential`, `no valid credential found`: OpenDAL raises these as
/// `ConfigInvalid` or `Unexpected` depending on the service) are `auth.no_credentials`.
/// `None` when the error says nothing more specific than "unexpected".
pub fn opendal_code(oe: &opendal::Error) -> Option<ErrorCode> {
    use opendal::ErrorKind as K;
    // The message, its context (`called: reqsign::Sign`) and its source.
    let credential = oe.to_string().to_ascii_lowercase().contains("credential");
    Some(match oe.kind() {
        K::NotFound => ErrorCode::StorageNotFound,
        K::PermissionDenied => ErrorCode::AuthDenied,
        K::RateLimited => ErrorCode::StorageThrottled,
        K::Unsupported => ErrorCode::StorageUnsupported,
        K::ConfigInvalid if credential => ErrorCode::AuthNoCredentials,
        K::ConfigInvalid => ErrorCode::ConfigInvalid,
        K::Unexpected if credential => ErrorCode::AuthNoCredentials,
        _ => match ErrorCode::from_class(&crate::engine::ops::error_class_of(oe, None)) {
            ErrorCode::Internal => return None,
            c => c,
        },
    })
}

/// Typed sources in the chain: OpenDAL kinds, dial stages, reqwest timeouts/connect errors, I/O
/// kinds, TOML parse errors. An OpenDAL error that says only "unexpected" lets a more specific
/// cause further down the chain decide.
fn typed_code(err: &anyhow::Error) -> Option<ErrorCode> {
    for cause in err.chain() {
        if let Some(oe) = cause.downcast_ref::<opendal::Error>()
            && let Some(c) = opendal_code(oe)
        {
            return Some(c);
        }
        if let Some(d) = cause.downcast_ref::<crate::net::dial::DialError>() {
            return Some(match d.stage {
                "dns" => ErrorCode::NetDns,
                "tls" => ErrorCode::NetTls,
                _ => ErrorCode::NetUnreachable,
            });
        }
        if let Some(r) = cause.downcast_ref::<reqwest::Error>() {
            if r.is_timeout() {
                return Some(ErrorCode::NetTimeout);
            }
            if r.is_connect() {
                return Some(ErrorCode::NetUnreachable);
            }
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::TimedOut => return Some(ErrorCode::NetTimeout),
                std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::HostUnreachable
                | std::io::ErrorKind::NetworkUnreachable => return Some(ErrorCode::NetUnreachable),
                std::io::ErrorKind::PermissionDenied => return Some(ErrorCode::AuthDenied),
                _ => {}
            }
        }
        if cause.downcast_ref::<toml::de::Error>().is_some() {
            return Some(ErrorCode::ConfigInvalid);
        }
        if cause
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
        {
            return Some(ErrorCode::NetTimeout);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An internal failure caused by Ctrl-C, filed as `run.interrupted`, links that code's
    /// docs and says it can be retried (9.0 kept `#internal` and `retryable: false`).
    #[test]
    fn a_recoded_error_takes_the_docs_and_retryability_of_its_code() {
        let b = classify(&anyhow::anyhow!("the runtime stopped"), "s3");
        assert_eq!(b.code, ErrorCode::Internal);
        assert!(!b.retryable);
        let b = b.with_code(ErrorCode::RunInterrupted);
        assert_eq!(b.code, ErrorCode::RunInterrupted);
        assert!(b.retryable);
        assert_eq!(b.docs, Some(ErrorCode::RunInterrupted.docs()));
        assert_ne!(ErrorCode::Internal.docs(), ErrorCode::RunInterrupted.docs());
    }

    #[test]
    fn credentials_in_urls_never_reach_error_messages() {
        let e = anyhow::anyhow!("connect to http://admin:s3cret@minio.local:9000/bucket failed")
            .context("cannot open the target");
        let b = classify(&e, "s3");
        assert!(!b.message.contains("s3cret"), "{}", b.message);
        assert!(
            b.message.contains("http://minio.local:9000/bucket"),
            "{}",
            b.message
        );
        let e: anyhow::Error = OprobeError::new(ErrorCode::NetUnreachable, "x")
            .with_hint("check https://u:p@proxy.example.com")
            .into();
        assert!(!classify(&e, "s3").hint.unwrap().contains("u:p@"));
    }

    /// A presigned or SAS URL is a bearer credential: HTTP clients and OpenDAL quote the whole
    /// URL in their errors, and neither the message nor the machine `detail` may carry its
    /// signature or token (9.1 removed only `user:password@`).
    #[test]
    fn presigned_signatures_never_reach_error_output() {
        let source = opendal::Error::new(opendal::ErrorKind::Unexpected, "send http request")
            .with_operation("reqwest::send")
            .with_context(
                "url",
                "https://b.s3.amazonaws.com/k?X-Amz-Credential=AKIAEXAMPLE&X-Amz-Expires=900&X-Amz-Security-Token=STSTOKEN&X-Amz-Signature=deadbeefcafe",
            )
            .set_source(anyhow::anyhow!(
                "error sending request for url (https://acct.blob.core.windows.net/c/k?sv=2024&se=2026-12-01&sig=SASSECRET)"
            ));
        let e = anyhow::Error::new(source).context("cannot read the object");
        let b = classify(&e, "s3");
        let json = serde_json::to_string(&b).unwrap();
        for text in [b.message.as_str(), json.as_str(), &format!("{b:?}")] {
            for secret in ["deadbeefcafe", "STSTOKEN", "AKIAEXAMPLE", "SASSECRET"] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
        // What is not a credential stays, so the failing request is still identifiable.
        assert!(
            b.message
                .contains("https://b.s3.amazonaws.com/k?X-Amz-Expires=900"),
            "{}",
            b.message
        );
        assert!(json.contains("sv=2024&se=2026-12-01"), "{json}");
    }

    #[test]
    fn every_code_has_its_docs_anchor() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
                .join("docs/troubleshooting.md"),
        )
        .unwrap();
        for c in ErrorCode::ALL {
            let url = c.docs();
            let (page, anchor) = url.rsplit_once('#').unwrap();
            assert!(page.ends_with("docs/troubleshooting.md"), "{url}");
            // Pinned to this version's tag, like the glossary (not `main`, which moves on).
            assert!(
                url.contains(&format!("/blob/v{}/", env!("CARGO_PKG_VERSION"))),
                "{url}"
            );
            assert!(
                doc.contains(&format!("<a id=\"{anchor}\"></a>")),
                "docs/troubleshooting.md has no anchor `{anchor}` for {c}"
            );
            assert!(
                doc.contains(&format!("### `{c}` (exit {}", c.exit_code())),
                "docs/troubleshooting.md: no section for {c} with exit {}",
                c.exit_code()
            );
        }
    }

    #[test]
    fn codes_round_trip_and_map_to_exit_codes() {
        for c in ErrorCode::ALL {
            assert_eq!(ErrorCode::parse(c.as_str()), Some(*c));
            let json = serde_json::to_string(c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.as_str()));
            let prefix = c.as_str().split('.').next().unwrap();
            let want = match prefix {
                "usage" | "config" => 2,
                "auth" | "net" | "storage" | "internal" => 1,
                "safety" if *c == ErrorCode::SafetyBudgetExceeded => 4,
                "safety" => 5,
                "run" if *c == ErrorCode::RunInterrupted => 130,
                "run" => 3,
                _ => unreachable!(),
            };
            assert_eq!(c.exit_code(), want, "{c}");
        }
        assert_eq!(ErrorCode::ALL.len(), 24);
    }

    #[test]
    fn classified_errors_survive_context() {
        let e = anyhow::Error::new(
            OprobeError::new(ErrorCode::UsageUnknownRun, "no run `x`")
                .with_hint("oprobe show --list"),
        )
        .context("cannot show the run");
        let b = classify(&e, "s3");
        assert_eq!(b.code, ErrorCode::UsageUnknownRun);
        assert_eq!(b.hint.as_deref(), Some("oprobe show --list"));
        assert!(b.message.contains("no run `x`"));
        assert_eq!(b.exit_code(), 2);
    }

    #[test]
    fn opendal_kinds_are_classified() {
        let e = anyhow::Error::new(opendal::Error::new(
            opendal::ErrorKind::PermissionDenied,
            "AccessDenied",
        ));
        assert_eq!(classify(&e, "s3").code, ErrorCode::AuthDenied);
        let e = anyhow::Error::new(opendal::Error::new(opendal::ErrorKind::NotFound, "nope"));
        assert_eq!(classify(&e, "s3").code, ErrorCode::StorageNotFound);
        // Messages are never matched: only typed errors carry a code.
        let e = anyhow::anyhow!("no target given: connection refused, credential");
        assert_eq!(classify(&e, "s3").code, ErrorCode::Internal);
        let e = anyhow::Error::new(opendal::Error::new(
            opendal::ErrorKind::Unexpected,
            "failed to load signing credential",
        ))
        .context("preflight");
        assert_eq!(classify(&e, "gcs").code, ErrorCode::AuthNoCredentials);
        let e = anyhow::Error::new(opendal::Error::new(
            opendal::ErrorKind::ConfigInvalid,
            "no valid credential found",
        ));
        assert_eq!(classify(&e, "s3").code, ErrorCode::AuthNoCredentials);
        let e = anyhow::anyhow!("something odd");
        assert_eq!(classify(&e, "s3").code, ErrorCode::Internal);
    }

    /// A wrong S3 key: the provider's code and message lead, the raw chain stays in `detail`.
    #[test]
    fn errors_lead_with_the_provider_code() {
        let dump = "preflight: cannot list the target root: PermissionDenied (persistent) at list, context: { uri: http://127.0.0.1:9000/b?list-type=2, response: Parts { status: 403, version: HTTP/1.1, headers: {\"x-amz-request-id\": \"17A\"} }, service: s3, path: / } => S3Error { code: \"InvalidAccessKeyId\", message: \"The AWS Access Key Id you provided does not exist in our records.\", resource: \"\", request_id: \"17A\" }";
        let e = anyhow::Error::new(OprobeError::new(ErrorCode::AuthDenied, dump));
        let b = classify(&e, "s3");
        assert_eq!(
            b.message,
            "credentials rejected: InvalidAccessKeyId: The AWS Access Key Id you provided does not exist in our records."
        );
        assert_eq!(b.detail.as_deref(), Some(dump));
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["detail"], dump);
        // An XML body (Azure, S3-compatible gateways).
        let xml = "unexpected: <?xml version=\"1.0\"?><Error><Code>SlowDown</Code><Message>Please reduce your request rate.</Message></Error>";
        assert_eq!(
            provider_lead(xml, ErrorCode::StorageThrottled).as_deref(),
            Some("throttled: SlowDown: Please reduce your request rate.")
        );
        // No provider code: the message is the chain, and there is no detail.
        let e = anyhow::Error::new(OprobeError::new(
            ErrorCode::NetUnreachable,
            "cannot reach x",
        ));
        let b = classify(&e, "s3");
        assert_eq!(b.message, "cannot reach x");
        assert!(b.detail.is_none());
    }

    #[test]
    fn api_statuses_are_network_codes() {
        assert_eq!(
            ErrorCode::from_api_status(403, true),
            ErrorCode::NetRateLimited
        );
        assert_eq!(
            ErrorCode::from_api_status(429, false),
            ErrorCode::NetRateLimited
        );
        assert_eq!(ErrorCode::from_api_status(403, false), ErrorCode::NetHttp);
        assert_eq!(ErrorCode::from_api_status(404, false), ErrorCode::NetHttp);
        assert!(ErrorCode::NetRateLimited.retryable());
        assert_eq!(ErrorCode::NetHttp.exit_code(), 1);
    }

    #[test]
    fn error_body_json_shape() {
        let b = ErrorBody::new(ErrorCode::SafetyReadOnly, "refused");
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["code"], "safety.read_only");
        assert_eq!(v["retryable"], false);
        assert!(v.get("hint").is_none());
    }
}
