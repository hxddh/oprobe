//! `--workers local:N`: N worker processes on this machine (design v5 §8), for testing and to
//! use more cores than one process drives.
//!
//! Each is `oprobe worker --listen 127.0.0.1:0` with a fresh random token (a private temporary
//! file) and `--exit-with-stdin`: a worker exits when this process goes away, even if it is
//! killed.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

use crate::errors::{ErrorCode, OprobeError};

/// Running local workers (stopped when dropped).
pub struct LocalWorkers {
    pub addrs: Vec<String>,
    pub token: String,
    children: Vec<(Child, Option<ChildStdin>)>,
    token_file: PathBuf,
}

impl Drop for LocalWorkers {
    fn drop(&mut self) {
        for (c, stdin) in &mut self.children {
            stdin.take();
            let _ = c.start_kill();
        }
        let _ = std::fs::remove_file(&self.token_file);
    }
}

/// A random token: 32 bytes, hex.
pub fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The `oprobe` binary to start workers from: `$OPROBE_WORKER_BIN`, else this executable when
/// it is `oprobe`.
fn worker_binary() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("OPROBE_WORKER_BIN").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().context("cannot locate the oprobe executable")?;
    if exe.file_stem().and_then(|s| s.to_str()) == Some("oprobe") {
        return Ok(exe);
    }
    Err(OprobeError::new(
        ErrorCode::UsageInvalidArgument,
        format!(
            "local workers need the oprobe binary, and this process is `{}`",
            exe.display()
        ),
    )
    .with_hint("set OPROBE_WORKER_BIN to the oprobe executable")
    .into())
}

fn write_private(path: &PathBuf, content: &str) -> Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o
        .open(path)
        .with_context(|| format!("cannot create `{}`", path.display()))?;
    f.write_all(content.as_bytes())?;
    Ok(())
}

/// Start `n` local workers and wait until each listens.
pub async fn spawn(n: u32) -> Result<LocalWorkers> {
    let bin = worker_binary()?;
    let token = random_token();
    let token_file =
        std::env::temp_dir().join(format!("oprobe-worker-{}.token", &random_token()[..16]));
    write_private(&token_file, &token)?;
    let mut lw = LocalWorkers {
        addrs: Vec::new(),
        token,
        children: Vec::new(),
        token_file: token_file.clone(),
    };
    for i in 0..n {
        let mut child = Command::new(&bin)
            .args(["worker", "--listen", "127.0.0.1:0", "--token-file"])
            .arg(&token_file)
            .arg("--exit-with-stdin")
            // This machine's user started them: an exec client here is no wider than running
            // it directly (the token is private and random).
            .arg("--allow-exec")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!("cannot start local worker {} (`{}`)", i + 1, bin.display())
            })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("worker stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .map_err(|_| anyhow::anyhow!("local worker {} did not start within 30 s", i + 1))??
            .ok_or_else(|| anyhow::anyhow!("local worker {} exited at start", i + 1))?;
        let v: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("local worker {} printed `{line}`", i + 1))?;
        let addr = v
            .get("listening")
            .and_then(|a| a.as_str())
            .context("the worker did not say where it listens")?
            .to_owned();
        // Keep draining its stdout (it prints nothing else, but never block it).
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        lw.addrs.push(addr);
        lw.children.push((child, stdin));
    }
    Ok(lw)
}
