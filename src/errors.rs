//! Classified errors (design §8): a stable dotted [`ErrorCode`], an [`ErrorBody`] for machine
//! output, and the exit-code mapping every command uses.
//!
//! Raise a classified error with [`OprobeError`] (or the [`fail!`](crate::fail) macro); any other
//! `anyhow::Error` is classified at the CLI boundary by [`classify`] (OpenDAL error kinds, typed
//! transport errors, then known messages), with a hint from `analysis::hints`.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Exit codes (design §8).
pub mod exit {
    pub const OK: u8 = 0;
    /// Runtime failure: auth, network, storage, internal.
    pub const RUNTIME: u8 = 1;
    /// Usage or configuration error.
    pub const USAGE: u8 = 2;
    /// SLA failed or regression.
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
    ConfigInvalid => "config.invalid", exit::USAGE, false;
    ConfigUnknownOption => "config.unknown_option", exit::USAGE, false;
    AuthNoCredentials => "auth.no_credentials", exit::RUNTIME, false;
    AuthDenied => "auth.denied", exit::RUNTIME, false;
    NetUnreachable => "net.unreachable", exit::RUNTIME, true;
    NetDns => "net.dns", exit::RUNTIME, true;
    NetTls => "net.tls", exit::RUNTIME, false;
    NetTimeout => "net.timeout", exit::RUNTIME, true;
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

    /// Documentation anchor for the code.
    pub fn docs(self) -> String {
        format!(
            "https://github.com/hxddh/oprobe/blob/main/docs/troubleshooting.md#{}",
            self.as_str().replace('.', "-")
        )
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
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
            retryable: code.retryable(),
            docs: Some(code.docs()),
        }
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
}

impl OprobeError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for OprobeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OprobeError {}

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

/// Classify an error chain: an [`OprobeError`] anywhere in the chain wins, then typed sources
/// (OpenDAL error kinds, dial/TLS/timeout errors), then known messages. `scheme` selects
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
            let mut body = ErrorBody::new(e.code, chain_message(err));
            body.hint = e
                .hint
                .as_deref()
                .map(crate::util::strip_url_credentials)
                .or_else(hint_text);
            return body;
        }
    }
    let code = typed_code(err).unwrap_or_else(|| message_code(&chain));
    let mut body = ErrorBody::new(code, chain_message(err));
    body.hint = hint_text();
    body
}

/// The whole chain on one line (`outer: inner: root`), with URL credentials (`user:pass@`)
/// removed: OpenDAL contexts quote endpoints as configured.
fn chain_message(err: &anyhow::Error) -> String {
    crate::util::strip_url_credentials(&format!("{err:#}"))
}

fn typed_code(err: &anyhow::Error) -> Option<ErrorCode> {
    for cause in err.chain() {
        if let Some(oe) = cause.downcast_ref::<opendal::Error>() {
            use opendal::ErrorKind as K;
            return Some(match oe.kind() {
                K::NotFound => ErrorCode::StorageNotFound,
                K::PermissionDenied => ErrorCode::AuthDenied,
                K::RateLimited => ErrorCode::StorageThrottled,
                K::Unsupported => ErrorCode::StorageUnsupported,
                K::ConfigInvalid => {
                    if oe.to_string().contains("credential") {
                        ErrorCode::AuthNoCredentials
                    } else {
                        ErrorCode::ConfigInvalid
                    }
                }
                _ => {
                    let class = crate::engine::ops::error_class_of(oe, None);
                    match ErrorCode::from_class(&class) {
                        ErrorCode::Internal => message_code(&format!("{err:#}")),
                        c => c,
                    }
                }
            });
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
                std::io::ErrorKind::ConnectionRefused => return Some(ErrorCode::NetUnreachable),
                std::io::ErrorKind::PermissionDenied => return Some(ErrorCode::AuthDenied),
                _ => {}
            }
        }
        if cause.downcast_ref::<toml::de::Error>().is_some() {
            return Some(ErrorCode::ConfigInvalid);
        }
    }
    None
}

/// Last resort: messages oprobe itself produces for usage and configuration errors.
fn message_code(chain: &str) -> ErrorCode {
    let l = chain.to_ascii_lowercase();
    let has = |s: &str| l.contains(s);
    if has("unknown option") || has("not a valid option") || has("did you mean") {
        ErrorCode::ConfigUnknownOption
    } else if has("no target given") || has("invalid storage url") || has("unsupported scheme") {
        ErrorCode::UsageUnknownTarget
    } else if has("invalid --") || has("must be at least") || has("must be between") {
        ErrorCode::UsageInvalidArgument
    } else if has("profile") && (has("parse") || has("invalid")) || has("invalid sla") {
        ErrorCode::ConfigInvalid
    } else if has("dns error")
        || has("failed to lookup address")
        || has("name or service not known")
    {
        ErrorCode::NetDns
    } else if has("certificate") || has("tls handshake") || has("invalid peer certificate") {
        ErrorCode::NetTls
    } else if has("timed out") || has("timeout") {
        ErrorCode::NetTimeout
    } else if has("connection refused") || has("unreachable") || has("connect error") {
        ErrorCode::NetUnreachable
    } else if has("credential") || has("no valid credential") {
        ErrorCode::AuthNoCredentials
    } else if has("access denied") || has("forbidden") || has("permission denied") {
        ErrorCode::AuthDenied
    } else {
        ErrorCode::Internal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn every_code_has_its_docs_anchor() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/troubleshooting.md"),
        )
        .unwrap();
        for c in ErrorCode::ALL {
            let url = c.docs();
            let (page, anchor) = url.rsplit_once('#').unwrap();
            assert!(page.ends_with("docs/troubleshooting.md"), "{url}");
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
        assert_eq!(ErrorCode::ALL.len(), 21);
    }

    #[test]
    fn classified_errors_survive_context() {
        let e = anyhow::Error::new(
            OprobeError::new(ErrorCode::UsageUnknownRun, "no run `x`")
                .with_hint("oprobe runs list"),
        )
        .context("cannot show the run");
        let b = classify(&e, "s3");
        assert_eq!(b.code, ErrorCode::UsageUnknownRun);
        assert_eq!(b.hint.as_deref(), Some("oprobe runs list"));
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
        let e = anyhow::anyhow!("no target given");
        assert_eq!(classify(&e, "s3").code, ErrorCode::UsageUnknownTarget);
        let e = anyhow::anyhow!("something odd");
        assert_eq!(classify(&e, "s3").code, ErrorCode::Internal);
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
