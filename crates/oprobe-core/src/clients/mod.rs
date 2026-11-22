//! What generates the load (design v5 §7). Owned by stream SCALE.
//!
//! - `opendal` (default): OpenDAL through oprobe's instrumented transport, every phase observed.
//! - `aws-sdk`: aws-sdk-s3 (Rust) over the same instrumented transport (cargo feature
//!   `aws-sdk`; [`aws`]).
//! - `exec:CMD`: an external adapter speaking the oprobe client protocol ([`protocol`],
//!   `docs/clients.md`); reference adapters for boto3, awscrt and aws-sdk-go-v2 live in
//!   `adapters/`.
//!
//! The non-default clients run behind an OpenDAL service ([`service::ClientService`]), so the
//! workloads, op scopes, recorder, budgets and the report are the ones every run uses. The
//! choice is runtime-only: it is never part of a stored spec (a replayed run uses the default
//! client unless `--client` is given again), so a run file can never start a command.

#[cfg(feature = "aws-sdk")]
pub mod aws;
pub mod exec;
pub mod protocol;
pub mod service;

use std::sync::Arc;

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::{Knobs, Profile, TargetSpec};
use crate::engine::stack::ClientStack;
use crate::errors::{ErrorCode, OprobeError};
use crate::model::ClientInfo;
use crate::trace::Recorder;

/// The load generator of a run (`--client`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ClientChoice {
    /// OpenDAL through oprobe's instrumented transport (every phase observed).
    #[default]
    Opendal,
    /// aws-sdk-s3 (Rust) over oprobe's instrumented transport.
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

    pub fn is_default(&self) -> bool {
        *self == Self::Opendal
    }

    /// Refuse clients this build cannot drive.
    pub fn check_supported(&self) -> Result<(), OprobeError> {
        match self {
            Self::Opendal | Self::Exec { .. } => Ok(()),
            Self::AwsSdk if cfg!(feature = "aws-sdk") => Ok(()),
            Self::AwsSdk => Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                "this oprobe was built without the `aws-sdk` client",
            )
            .with_hint(
                "install a build with it (`cargo install oprobe --features aws-sdk`), or use \
                 the default OpenDAL client",
            )),
        }
    }

    /// What `report.client` says about this choice before a driver ran (an adapter's name,
    /// version and phases are known once it answered `hello`: [`Driver::info`]).
    pub fn info(&self) -> ClientInfo {
        match self {
            Self::Opendal => ClientInfo::opendal(),
            Self::AwsSdk => ClientInfo {
                kind: self.kind().to_owned(),
                version: aws_version(),
                phases_available: AWS_SDK_PHASES.iter().map(|s| (*s).to_owned()).collect(),
            },
            Self::Exec { .. } => ClientInfo {
                kind: self.kind().to_owned(),
                version: None,
                phases_available: Vec::new(),
            },
        }
    }
}

/// Stages the aws-sdk client observes: oprobe's transport sees every request the SDK sends;
/// the SDK reads bodies at once (no consumer stall) and its retry backoff is internal.
pub const AWS_SDK_PHASES: &[&str] = &["queue", "connect", "send", "ttfb", "body"];

/// Every stage name a client can report.
pub const KNOWN_PHASES: &[&str] = &[
    "queue", "connect", "send", "ttfb", "body", "stall", "backoff",
];

fn aws_version() -> Option<String> {
    #[cfg(feature = "aws-sdk")]
    {
        Some(format!("aws-sdk-s3 {}", aws::VERSION))
    }
    #[cfg(not(feature = "aws-sdk"))]
    {
        None
    }
}

#[derive(Debug)]
enum Imp {
    Exec(Arc<exec::ExecBackend>),
    #[cfg(feature = "aws-sdk")]
    Aws,
}

/// A started non-default client: what [`crate::engine::Session`] drives instead of OpenDAL's
/// own HTTP stack.
#[derive(Debug)]
pub struct Driver {
    choice: ClientChoice,
    imp: Imp,
}

impl Driver {
    /// Start the client of `choice` for the (prepared) `target`: spawns an `exec` adapter and
    /// exchanges `hello`. `None` for the default OpenDAL client.
    pub async fn start(
        choice: &ClientChoice,
        target: &TargetSpec,
        knobs: &Knobs,
    ) -> Result<Option<Arc<Self>>> {
        choice.check_supported()?;
        let imp = match choice {
            ClientChoice::Opendal => return Ok(None),
            ClientChoice::Exec { command } => Imp::Exec(Arc::new(
                exec::ExecBackend::spawn(command, target, knobs).await?,
            )),
            #[cfg(feature = "aws-sdk")]
            ClientChoice::AwsSdk => {
                // Fail early on a target the SDK cannot drive (no bucket, no credentials).
                aws::AwsBackend::new(target, knobs, opendal::HttpTransporter::default())?;
                Imp::Aws
            }
            #[cfg(not(feature = "aws-sdk"))]
            ClientChoice::AwsSdk => unreachable!("refused by check_supported"),
        };
        Ok(Some(Arc::new(Self {
            choice: choice.clone(),
            imp,
        })))
    }

    pub fn choice(&self) -> &ClientChoice {
        &self.choice
    }

    /// What the client can do (the workload mix is adapted to it).
    pub fn capability(&self) -> opendal::Capability {
        match &self.imp {
            Imp::Exec(e) => e.capability(),
            #[cfg(feature = "aws-sdk")]
            Imp::Aws => aws::capability(),
        }
    }

    /// `report.client`.
    pub fn info(&self) -> ClientInfo {
        match &self.imp {
            Imp::Exec(e) => {
                let h = &e.hello;
                ClientInfo {
                    kind: "exec".into(),
                    version: Some(match &h.version {
                        Some(v) => format!("{} {v}", h.client),
                        None => h.client.clone(),
                    }),
                    phases_available: h
                        .phases
                        .iter()
                        .filter(|p| KNOWN_PHASES.contains(&p.as_str()))
                        .cloned()
                        .collect(),
                }
            }
            #[cfg(feature = "aws-sdk")]
            Imp::Aws => self.choice.info(),
        }
    }

    /// Adjust the profile to what the client can check; returns warnings for the report.
    pub fn adjust(&self, profile: &mut Profile) -> Vec<String> {
        let mut w = Vec::new();
        if matches!(self.imp, Imp::Exec(_)) && profile.workload.verify {
            // Adapters report sizes, not bodies: reads cannot be checked byte for byte.
            profile.workload.verify = false;
            w.push(
                "integrity checks are off: the exec client reports how many bytes it read, \
                 not the bytes"
                    .into(),
            );
        }
        w
    }

    /// The operator a measurement window drives: this client behind OpenDAL's API, its
    /// requests recorded into `recorder`. OpenDAL's retry layer is not added: the client
    /// retries itself (`knobs.retries` is passed to it).
    pub fn operator(
        &self,
        target: &TargetSpec,
        knobs: &Knobs,
        recorder: &Arc<Recorder>,
    ) -> Result<opendal::Operator> {
        let root = target
            .resolved_options()?
            .get("root")
            .cloned()
            .unwrap_or_else(|| "/".into());
        let cap = self.capability();
        let svc = match &self.imp {
            Imp::Exec(e) => {
                let b: Arc<dyn service::Backend> = Arc::new(e.clone());
                service::ClientService::new(b, "exec", &root, cap)
            }
            #[cfg(feature = "aws-sdk")]
            Imp::Aws => {
                let http = traced_transport(target, knobs, recorder)?;
                let b: Arc<dyn service::Backend> =
                    Arc::new(aws::AwsBackend::new(target, knobs, http)?);
                service::ClientService::new(b, "aws_sdk", &root, cap)
            }
        };
        let op = svc
            .into_operator()?
            // Propagates the operation's scope into OpenDAL's sub-tasks (its HTTP transport
            // is unused by these clients).
            .layer(crate::engine::operator::trace_layer(
                target,
                knobs,
                Some(recorder.clone()),
            )?)
            .layer(ClientStack::from_knobs(knobs).timeout_layer());
        recorder.set_cache_enabled(false);
        Ok(op)
    }

    /// Stop the client (an adapter is asked to exit).
    pub async fn shutdown(&self) {
        #[allow(irrefutable_let_patterns)]
        if let Imp::Exec(e) = &self.imp {
            e.shutdown().await;
        }
    }
}

/// oprobe's traced HTTP transport for `target` (what the OpenDAL client measures through),
/// recording into `recorder`.
#[cfg(feature = "aws-sdk")]
fn traced_transport(
    target: &TargetSpec,
    knobs: &Knobs,
    recorder: &Arc<Recorder>,
) -> Result<opendal::HttpTransporter> {
    use crate::config::TransportMode;
    use crate::net::FidelityTransport;
    Ok(match knobs.transport {
        TransportMode::Microscope => opendal::HttpTransporter::new(
            crate::engine::operator::transport(target, knobs, Some(recorder.clone()))?,
        ),
        TransportMode::Opendal => {
            let https = target.endpoint().is_none_or(|e| e.starts_with("https"));
            opendal::HttpTransporter::new(FidelityTransport::new(
                crate::engine::operator::transport_config(target, knobs)?,
                https,
                Some(recorder.clone()),
            )?)
        }
    })
}

/// Findings that need a stage the client cannot observe, and those stages.
const NEEDS: &[(&str, &[&str])] = &[
    ("client_queue", &["queue"]),
    ("cold_connections", &["connect"]),
    ("remote_skew", &["connect"]),
    ("slow_start", &["connect"]),
    ("consumer_stall", &["stall"]),
    ("server_latency", &["ttfb"]),
    ("download_bound", &["body"]),
    ("upload_bound", &["send"]),
];

/// Remove the findings `report.client` cannot support (design v5 §7) and say so in the
/// report's warnings. The `exec` client's time outside the SDK includes the protocol, so its
/// client-overhead findings are suppressed too.
pub fn suppress_unsupported(report: &mut crate::model::Report) {
    let client = report.client.clone();
    if client.kind == "opendal" {
        return;
    }
    let mut gone: Vec<String> = Vec::new();
    report.findings.retain(|f| {
        let missing = NEEDS
            .iter()
            .find(|(code, _)| *code == f.code)
            .is_some_and(|(_, needs)| needs.iter().any(|p| !client.has_phase(p)));
        let overhead =
            client.kind == "exec" && matches!(f.code.as_str(), "client_overhead" | "client_cpu");
        if missing || overhead {
            gone.push(f.code.clone());
        }
        !(missing || overhead)
    });
    if !gone.is_empty() {
        report.warnings.push(format!(
            "findings suppressed: the `{}` client cannot observe what they need ({})",
            client.kind,
            gone.join(", ")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_checks_clients() {
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
        assert!(
            ClientChoice::Exec {
                command: "x".into()
            }
            .check_supported()
            .is_ok()
        );
        assert_eq!(
            ClientChoice::AwsSdk.check_supported().is_ok(),
            cfg!(feature = "aws-sdk")
        );
        assert_eq!(ClientChoice::Opendal.info().kind, "opendal");
        assert_eq!(ClientChoice::AwsSdk.info().kind, "aws_sdk");
    }

    #[test]
    fn findings_without_their_phases_are_suppressed() {
        let mut r = crate::report::fixture::report();
        let base = r.findings.first().cloned().expect("fixture finding");
        let f = |code: &str| {
            let mut f = base.clone();
            f.code = code.into();
            f
        };
        r.findings = vec![f("server_latency"), f("throttling"), f("client_overhead")];
        r.client = ClientInfo {
            kind: "exec".into(),
            version: None,
            phases_available: vec!["connect".into()],
        };
        suppress_unsupported(&mut r);
        let codes: Vec<_> = r.findings.iter().map(|f| f.code.as_str()).collect();
        assert_eq!(codes, vec!["throttling"]);
        assert!(r.warnings.iter().any(|w| w.contains("server_latency")));
    }
}
