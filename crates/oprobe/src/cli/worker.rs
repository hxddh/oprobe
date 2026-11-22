//! `oprobe worker`: serve load for distributed runs (design v5 §8).

use anyhow::{Context, Result};

use super::Ctx;
use super::args::WorkerArgs;
use crate::distributed::{WorkerOutput, protocol, worker};
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::report::style::dim;

fn read(path: &std::path::Path, what: &str) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the {what} `{}`", path.display()))
}

/// The token, refusing a file other users can read.
fn token(path: &std::path::Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("cannot read the token file `{}`", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(OprobeError::new(
                ErrorCode::SafetyRefused,
                format!(
                    "the token file `{}` is readable by other users (mode {:o})",
                    path.display(),
                    mode & 0o777
                ),
            )
            .with_hint(format!("restrict it: chmod 600 {}", path.display()))
            .into());
        }
    }
    let t = read(path, "token file")?.trim().to_owned();
    if t.len() < 16 {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            "the worker token is shorter than 16 characters",
        )
        .with_hint("write a long random secret, e.g. `openssl rand -hex 32 > token`")
        .into());
    }
    Ok(t)
}

pub async fn cmd_worker(a: WorkerArgs, ctx: &Ctx) -> Result<u8> {
    let token = token(&a.token_file)?;
    let tls = match (&a.tls_cert, &a.tls_key) {
        (Some(c), Some(k)) => Some((read(c, "certificate")?, read(k, "private key")?)),
        _ => None,
    };
    let listener = tokio::net::TcpListener::bind(&a.listen)
        .await
        .map_err(|e| {
            OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("cannot listen on {}: {e}", a.listen),
            )
            .with_hint("pick another address or port (--listen HOST:PORT)")
        })?;
    let addr = listener.local_addr()?;
    let out = WorkerOutput {
        listening: addr.to_string(),
        protocol: protocol::PROTOCOL.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        tls: tls.is_some(),
        clients: worker::clients(a.allow_exec),
    };
    if a.exit_with_stdin {
        // Local workers: one compact line the coordinator reads.
        println!("{}", serde_json::to_string(&out)?);
    } else {
        ctx.out.finish(&out)?;
        if !addr.ip().is_loopback() && tls.is_none() {
            ctx.out.warn(
                "the worker protocol is unencrypted: coordinators send only redacted specs to \
                 it (credentials come from this machine); use --tls-cert on untrusted networks",
            );
        }
    }
    let cfg = worker::WorkerConfig {
        token,
        allow_exec: a.allow_exec,
        tls,
        paths: ctx.paths.clone(),
    };
    let stop_on_stdin = a.exit_with_stdin;
    let shutdown = async move {
        if stop_on_stdin {
            // The coordinator holds our stdin; EOF means it is gone.
            let stdin = async {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 64];
                let mut s = tokio::io::stdin();
                while matches!(s.read(&mut buf).await, Ok(n) if n > 0) {}
            };
            tokio::select! {
                _ = stdin => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    worker::serve(listener, cfg, shutdown).await?;
    if !a.exit_with_stdin {
        ctx.out.info(dim("worker stopped"));
    }
    Ok(exit::OK)
}
