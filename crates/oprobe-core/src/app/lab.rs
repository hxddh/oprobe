//! `oprobe lab`: the local S3-compatible server with faults.

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::input::*;
use crate::output::CommandOutput;
use crate::report::style::{bold, dim, green};
use crate::report::write_file;
use crate::util::parse_duration;

/// What `oprobe lab` serves (printed once it listens).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct LabOutput {
    pub endpoint: String,
    pub tls: bool,
    /// A command that benchmarks the lab.
    pub try_command: String,
}

impl CommandOutput for LabOutput {
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![crate::output::NextStep::new(
            "measure the lab",
            self.try_command.clone(),
        )]
    }

    fn text(&self) -> String {
        format!(
            "{} S3 lab listening on {}\n  try: {}\n  {}\n",
            green("●"),
            bold(&self.endpoint),
            self.try_command,
            dim("any bucket name works; data lives in memory; Ctrl-C to stop")
        )
    }
}

impl LabOutput {
    /// What `lab` serves: its endpoint and a command that profiles it.
    pub fn of(lab: &crate::lab::LabHandle) -> Self {
        let tls = lab.endpoint_url.starts_with("https://");
        Self {
            endpoint: lab.endpoint_url.clone(),
            tls,
            try_command: format!(
                "AWS_ACCESS_KEY_ID=lab AWS_SECRET_ACCESS_KEY=lab oprobe run s3://bench/demo --set options.endpoint={} --set options.region=us-east-1 --set http.proxy=none{}",
                lab.endpoint_url,
                if tls { " --set http.insecure=true" } else { "" }
            ),
        }
    }
}

/// `oprobe lab`: start the server (it serves until the returned handle is dropped).
pub async fn lab(a: LabRequest) -> Result<crate::lab::LabHandle> {
    use crate::lab::{LabConfig, LabServer, lab_ca_pem};
    if let Some(p) = &a.write_ca {
        write_file(p, lab_ca_pem())?;
    }
    let mut cfg = match &a.scenario {
        Some(s) => crate::lab::Scenario::parse(s)?.config(),
        None => LabConfig::default(),
    };
    // Flags override the scenario where they were given (differ from their defaults).
    cfg.enable_tls = a.tls;
    cfg.enable_http2 = !a.h1_only;
    cfg.throttle_status_code = a.throttle_status;
    cfg.throttle_early_close |= a.throttle_early_close;
    let latency = parse_duration(&a.latency)?;
    if !latency.is_zero() {
        cfg.base_ttfb_delay = latency;
    }
    let complete = parse_duration(&a.complete_delay)?;
    if !complete.is_zero() {
        cfg.multipart_merge_delay = complete;
    }
    if a.straggler_every > 0 {
        cfg.straggler_every_n = a.straggler_every;
        cfg.straggler_delay = parse_duration(&a.straggler)?;
    }
    cfg.max_inflight_before_503 = a.throttle_above.or(cfg.max_inflight_before_503);
    cfg.throttle_every_n_requests = a.throttle_every.or(cfg.throttle_every_n_requests);
    cfg.stream_bandwidth_kib_s = a.bandwidth_kib.or(cfg.stream_bandwidth_kib_s);
    cfg.error_every_n = a.error_every.or(cfg.error_every_n);
    cfg.reset_every_n = a.reset_every.or(cfg.reset_every_n);
    let mut limits = crate::lab::LabLimits {
        allow_remote: a.allow_remote,
        ..Default::default()
    };
    if let Some(v) = &a.max_object_size {
        limits.max_object_bytes = crate::input::size(v)?.0;
    }
    if let Some(v) = &a.max_store_size {
        limits.max_store_bytes = crate::input::size(v)?.0;
    }
    if let Some(n) = a.max_connections {
        limits.max_connections = n.max(1);
    }
    LabServer::spawn_with(&a.bind, cfg, limits)
        .await
        .map_err(|e| bind_error(e, &a.bind))
}

/// The error of a lab that could not listen on `bind`: a usage error that says so (a busy
/// port was `internal`, with no hint). Other errors pass through unchanged.
fn bind_error(e: anyhow::Error, bind: &str) -> anyhow::Error {
    use crate::errors::{ErrorCode, OprobeError};
    let Some(io) = e.downcast_ref::<std::io::Error>() else {
        return e;
    };
    let (cause, hint) = if io.kind() == std::io::ErrorKind::AddrInUse {
        (
            "the address is already in use".to_owned(),
            "stop the process that listens there, or pick another port with `--bind` \
             (`--bind 127.0.0.1:0` takes a free one)",
        )
    } else {
        (
            io.to_string(),
            "pass an address this machine can listen on, such as `--bind 127.0.0.1:0`",
        )
    };
    OprobeError::new(
        ErrorCode::UsageInvalidArgument,
        format!("cannot listen on {bind}: {cause}"),
    )
    .with_hint(hint)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `oprobe lab --bind` on a busy port was `internal` (exit 1) with no hint.
    #[tokio::test]
    async fn a_busy_port_is_a_usage_error_with_a_hint() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = taken.local_addr().unwrap().to_string();
        let e = lab(LabRequest {
            bind: bind.clone(),
            ..Default::default()
        })
        .await
        .err()
        .expect("the port is taken");
        let b = crate::errors::classify(&e, "s3");
        assert_eq!(b.code, crate::errors::ErrorCode::UsageInvalidArgument);
        assert_eq!(
            b.message,
            format!("cannot listen on {bind}: the address is already in use")
        );
        assert!(b.hint.unwrap_or_default().contains("--bind 127.0.0.1:0"));
    }
}
