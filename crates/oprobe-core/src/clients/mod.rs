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

    /// `python3`, when this machine has it (the exec tests are skipped otherwise).
    pub(crate) fn python3() -> Option<&'static str> {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| "python3")
    }

    /// The minimal reference adapter (`adapters/minimal`), as an `exec:` client.
    pub(crate) fn minimal_adapter(py: &str) -> ClientChoice {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../adapters/minimal/oprobe_minimal.py");
        ClientChoice::Exec {
            command: format!("{py} -u '{}'", script.display()),
        }
    }

    /// Every operation the protocol has, against the lab, with `client`.
    async fn every_operation(
        client: ClientChoice,
    ) -> (crate::model::Run, crate::lab::LabHandle, u64) {
        let lab = crate::lab::LabServer::spawn("127.0.0.1:0", Default::default())
            .await
            .unwrap();
        let mut p = lab.profile("bkt");
        p.client = client;
        p.workload.duration = crate::util::HumanDuration(std::time::Duration::from_millis(1500));
        p.workload.warmup = crate::util::HumanDuration(std::time::Duration::ZERO);
        p.knobs.concurrency = 4;
        p.workload.mix = serde_json::from_value(serde_json::json!([
            {"op": "read"},
            {"op": "read", "size": "4KiB"},
            {"op": "write", "size": "16KiB"},
            {"op": "stat"},
            {"op": "list", "recursive": true},
            {"op": "delete", "batch": 2},
            {"op": "copy"},
            {"op": "conditional_write", "size": "4KiB"},
            {"op": "metadata_write", "size": "4KiB"},
        ]))
        .unwrap();
        let before = lab.requests_served();
        let rails =
            crate::engine::measure::Rails::new(&p, false, None, Default::default()).unwrap();
        let m = crate::engine::measure::measure(crate::engine::measure::Request::new(
            p,
            rails,
            crate::engine::Cancel::new(),
            Arc::new(|_| {}),
        ))
        .await
        .unwrap();
        (m.run, lab, before)
    }

    fn check_every_operation(r: &crate::model::Report, lab: &crate::lab::LabHandle, before: u64) {
        assert!(r.summary.ops > 20, "{:?}", r.summary);
        assert_eq!(
            r.summary.failed_ops, 0,
            "{:?} {:?}",
            r.resilience, r.warnings
        );
        for v in [
            "read.full",
            "read.range",
            "write",
            "stat",
            "list",
            "delete.batch",
            "copy",
            "write.conditional",
            "write.metadata",
        ] {
            assert!(
                r.verbs.iter().any(|x| x.verb == v && x.count > 0),
                "{v} missing: {:?}",
                r.verbs
                    .iter()
                    .map(|x| (&x.verb, x.count))
                    .collect::<Vec<_>>()
            );
        }
        // The requests the client issued are requests the lab served (the dataset and the
        // cleanup went through OpenDAL's own client).
        let recorded: u64 = r.apis.iter().map(|a| a.count).sum();
        assert!(recorded > 0 && lab.requests_served() - before >= recorded);
        let get = r
            .apis
            .iter()
            .find(|a| a.api == crate::trace::Api::GetObject)
            .unwrap();
        assert!(get.stages.ttfb.count > 0 && get.bytes_down > 0);
        // Nothing left behind.
        assert!(
            lab.stored_keys().iter().all(|k| !k.contains(&r.run.id)),
            "{:?}",
            lab.stored_keys()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_exec_adapter_drives_every_operation_against_the_lab() {
        let Some(py) = python3() else {
            eprintln!("skipped: python3 is not installed");
            return;
        };
        let (run, lab, before) = every_operation(minimal_adapter(py)).await;
        let r = &run.report;
        assert_eq!(r.client.kind, "exec");
        assert!(
            r.client
                .version
                .as_deref()
                .is_some_and(|v| v.starts_with("python-http.client")),
            "{:?}",
            r.client
        );
        assert!(r.client.has_phase("ttfb") && !r.client.has_phase("queue"));
        check_every_operation(r, &lab, before);
    }

    /// Check any adapter against the lab by hand:
    /// `OPROBE_TEST_ADAPTER="python3 adapters/boto3/oprobe_boto3.py" cargo test -p oprobe-core
    /// --lib adapter_from_env -- --ignored` (the reference adapters need their SDK installed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs OPROBE_TEST_ADAPTER and the adapter's SDK"]
    async fn adapter_from_env() {
        let command = std::env::var("OPROBE_TEST_ADAPTER").expect("OPROBE_TEST_ADAPTER");
        let (run, lab, before) = every_operation(ClientChoice::Exec { command }).await;
        let r = &run.report;
        if let Some(o) = run.ops.iter().find(|o| !o.ok) {
            panic!("{} failed: {:?}", o.verb, o.error);
        }
        eprintln!("client: {:?}", r.client);
        check_every_operation(r, &lab, before);
    }

    #[cfg(feature = "aws-sdk")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_aws_sdk_drives_every_operation_against_the_lab() {
        let (run, lab, before) = every_operation(ClientChoice::AwsSdk).await;
        let r = &run.report;
        if let Some(o) = run.ops.iter().find(|o| !o.ok) {
            panic!("{} failed: {:?}", o.verb, o.error);
        }
        assert_eq!(r.client.kind, "aws_sdk");
        assert!(
            r.client
                .version
                .as_deref()
                .is_some_and(|v| v.starts_with("aws-sdk-s3 1.")),
            "{:?}",
            r.client
        );
        check_every_operation(r, &lab, before);
        // oprobe's transport saw the SDK's connections.
        assert!(r.connections.dialed > 0, "{:?}", r.connections);
        assert!(run.reqs.iter().any(|q| q.api == "ListObjects"));
    }

    #[tokio::test]
    async fn a_broken_adapter_is_a_clear_error() {
        let Some(py) = python3() else {
            eprintln!("skipped: python3 is not installed");
            return;
        };
        let t = TargetSpec::from_url("s3://b/p").unwrap();
        let e = Driver::start(
            &ClientChoice::Exec {
                command: format!(
                    "{py} -c 'import sys; print(\"boom\", file=sys.stderr); sys.exit(3)'"
                ),
            },
            &t,
            &Knobs::default(),
        )
        .await
        .expect_err("refused");
        let msg = format!("{e:#}");
        assert!(
            msg.contains("exited before answering") && msg.contains("boom"),
            "{msg}"
        );
        let e = Driver::start(
            &ClientChoice::Exec {
                command: "/nonexistent/adapter".into(),
            },
            &t,
            &Knobs::default(),
        )
        .await
        .expect_err("refused");
        assert!(format!("{e:#}").contains("cannot start"));
    }
}
