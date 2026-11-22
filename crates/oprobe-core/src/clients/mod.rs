//! What generates the load (design v5 §7). Owned by stream SCALE.
//!
//! The foundation only defines the choice a [`crate::app::RunRequest`] carries; OpenDAL (the
//! white-box default) is the only client implemented. `aws-sdk` and `exec:CMD` (the oprobe
//! client protocol, `docs/clients.md`) are refused with a usage error until SCALE fills them in.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::errors::{ErrorCode, OprobeError};

/// The load generator of a run (`--client`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ClientChoice {
    /// OpenDAL through oprobe's instrumented transport (every phase observed).
    #[default]
    Opendal,
    /// aws-sdk-s3 (Rust) over oprobe's instrumented hyper connector.
    AwsSdk,
    /// An external adapter speaking the oprobe client protocol (NDJSON over stdin/stdout).
    Exec { command: String },
}

impl ClientChoice {
    /// Parse `--client`: `opendal`, `aws-sdk` or `exec:CMD`.
    pub fn parse(v: &str) -> Result<Self, OprobeError> {
        let t = v.trim();
        match t {
            "opendal" => Ok(Self::Opendal),
            "aws-sdk" | "aws_sdk" => Ok(Self::AwsSdk),
            _ => match t.strip_prefix("exec:") {
                Some(cmd) if !cmd.trim().is_empty() => Ok(Self::Exec {
                    command: cmd.trim().to_owned(),
                }),
                _ => Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("unknown client `{t}`"),
                )
                .with_hint("use opendal, aws-sdk or exec:COMMAND")),
            },
        }
    }

    /// The `report.client.kind` label.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Opendal => "opendal",
            Self::AwsSdk => "aws_sdk",
            Self::Exec { .. } => "exec",
        }
    }

    /// Refuse clients this build cannot drive yet.
    pub fn check_supported(&self) -> Result<(), OprobeError> {
        match self {
            Self::Opendal => Ok(()),
            other => Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!(
                    "the `{}` client is not available in this build",
                    other.kind()
                ),
            )
            .with_hint("run with the default OpenDAL client")),
        }
    }

    /// What `report.client` says about this choice.
    pub fn info(&self) -> crate::model::ClientInfo {
        crate::model::ClientInfo {
            kind: self.kind().to_owned(),
            version: match self {
                Self::Opendal => Some(crate::model::OPENDAL_VERSION.to_owned()),
                _ => None,
            },
            phases_available: match self {
                Self::Opendal => crate::model::ClientInfo::OPENDAL_PHASES
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
                _ => Vec::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_refuses_unimplemented_clients() {
        assert_eq!(
            ClientChoice::parse("opendal").unwrap(),
            ClientChoice::Opendal
        );
        assert_eq!(
            ClientChoice::parse("exec: python a.py").unwrap(),
            ClientChoice::Exec {
                command: "python a.py".into()
            }
        );
        assert!(ClientChoice::parse("exec:").is_err());
        assert!(ClientChoice::Opendal.check_supported().is_ok());
        assert!(ClientChoice::AwsSdk.check_supported().is_err());
        assert_eq!(ClientChoice::Opendal.info().kind, "opendal");
    }
}
