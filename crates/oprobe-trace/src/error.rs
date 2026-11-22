//! Classified errors: a stable dotted [`ErrorCode`] (the vocabulary of `error_code` in trace
//! rows and of every oprobe error), an [`ErrorBody`] for machine output, [`OprobeError`] and
//! the exit-code mapping. `oprobe-core` re-exports them and adds classification of any error.

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
