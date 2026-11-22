//! Clients and distributed load end to end (design v5 §7, §8): `oprobe run --workers local:3`
//! against the in-process lab, `oprobe worker` and its token, and `--client exec:` with the
//! minimal reference adapter (only when python3 is installed). Every test uses its own config
//! and data directory.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use oprobe::lab::{LabConfig, LabServer};

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_oprobe"));
        c.env("OPROBE_DATA_DIR", self.path("data"))
            .env("OPROBE_CONFIG", self.path("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .env_remove("OPROBE_WORKER_TOKEN")
            .args(["--color", "never"]);
        c
    }
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn json(o: &Output) -> serde_json::Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&o.stdout),
            err(o)
        )
    })
}

fn background_lab() -> (tokio::runtime::Runtime, oprobe::lab::LabHandle) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(async {
        LabServer::spawn("127.0.0.1:0", LabConfig::default())
            .await
            .expect("lab starts")
    });
    (rt, h)
}

fn lab_args(endpoint: &str) -> Vec<String> {
    [
        "s3://bkt/x",
        "--endpoint",
        endpoint,
        "--proxy",
        "none",
        "--option",
        "access_key_id=x",
        "--option",
        "secret_access_key=x",
        "--region",
        "us-east-1",
        "--preset",
        "small-files",
        "--objects",
        "12",
        "--object-size",
        "16KiB",
        "--warmup",
        "0s",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[test]
fn local_workers_merge_into_one_saved_run() {
    let env = Env::new();
    let (_rt, lab) = background_lab();
    let o = env
        .cmd()
        .arg("run")
        .args(lab_args(&lab.endpoint_url))
        .args(["-c", "6", "-d", "2s", "--workers", "local:3", "-o", "json"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let v = json(&o);
    let r = &v["report"];
    let workers = r["workers"].as_array().unwrap();
    assert_eq!(workers.len(), 3, "{workers:#?}");
    let ops: u64 = workers
        .iter()
        .map(|w| w["ops_count"].as_u64().unwrap())
        .sum();
    assert!(ops > 0);
    assert_eq!(r["summary"]["ops_count"].as_u64(), Some(ops));
    assert!(
        workers
            .iter()
            .all(|w| w["addr"].as_str().unwrap().starts_with("127.0.0.1:"))
    );
    assert_eq!(r["dimensions"]["workers"].as_array().unwrap().len(), 3);
    assert_eq!(r["client"]["kind"], "opendal");
    // The merged run is saved and reads back like any other.
    let id = v["run_id"].as_str().unwrap();
    let show = env
        .cmd()
        .args(["runs", "show", id, "-o", "json"])
        .output()
        .unwrap();
    assert!(show.status.success(), "{}", err(&show));
    // Every worker removed its prefix.
    assert!(
        lab.stored_keys().iter().all(|k| !k.contains(id)),
        "{:?}",
        lab.stored_keys()
    );
}

#[test]
fn local_workers_share_poisson_arrivals_and_the_run_file() {
    let env = Env::new();
    let (_rt, lab) = background_lab();
    let o = env
        .cmd()
        .arg("run")
        .args(lab_args(&lab.endpoint_url))
        .args([
            "-c",
            "6",
            "-d",
            "2s",
            "--rate",
            "60",
            "--arrivals",
            "poisson",
            "--workers",
            "local:3",
            "-o",
            "json",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let v = json(&o);
    let r = &v["report"];
    let s = &r["schedule"];
    assert_eq!(s["open_loop"], true, "{s:#}");
    assert_eq!(s["arrivals"], "poisson", "{s:#}");
    // About 120 arrivals in total (3 × 20/s for 2 s), not 3 × 120.
    let intended = s["intended_ops_count"].as_u64().unwrap();
    assert!((40..=240).contains(&intended), "{s:#}");
    let ops = r["summary"]["ops_count"].as_u64().unwrap();
    assert!(ops > 0 && ops <= intended, "{ops} of {intended}");
    // The saved run file holds the rows of every worker (ids carry the worker above bit 48).
    let id = v["run_id"].as_str().unwrap();
    let csv = env
        .cmd()
        .args(["runs", "export", id, "-f", "csv", "--table", "ops"])
        .output()
        .unwrap();
    assert!(csv.status.success(), "{}", err(&csv));
    let text = String::from_utf8_lossy(&csv.stdout);
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    let col = header
        .iter()
        .position(|h| *h == "op_id")
        .expect("op_id column");
    let mut workers = std::collections::BTreeSet::new();
    let mut rows = 0u64;
    for l in lines {
        let id: u64 = l.split(',').nth(col).unwrap().parse().unwrap();
        workers.insert(id >> 48);
        rows += 1;
    }
    assert_eq!(rows, ops, "every measured operation has its row");
    assert_eq!(workers.len(), 3, "{workers:?}");
}

#[test]
fn replay_needs_local_workers() {
    let env = Env::new();
    let token = env.path("token");
    std::fs::write(&token, "0123456789abcdef0123456789abcdef").unwrap();
    let log = env.path("access.log");
    std::fs::write(&log, "").unwrap();
    let o = env
        .cmd()
        .args(["run", "memory://", "--replay"])
        .arg(&log)
        .args(["--workers", "127.0.0.1:9", "--worker-token-file"])
        .arg(&token)
        .args(["-o", "json"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    let all = format!("{}{}", String::from_utf8_lossy(&o.stdout), err(&o));
    assert!(
        all.contains("usage.invalid_argument") && all.contains("local:N"),
        "{all}"
    );
}

#[test]
fn workers_need_their_token() {
    let env = Env::new();
    let token = env.path("token");
    std::fs::write(&token, "0123456789abcdef0123456789abcdef").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A token other users can read is refused.
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        let o = env
            .cmd()
            .args(["worker", "--listen", "127.0.0.1:0", "--token-file"])
            .arg(&token)
            .output()
            .unwrap();
        assert!(!o.status.success());
        assert!(err(&o).contains("readable by other users"), "{}", err(&o));
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut w = env
        .cmd()
        .args([
            "worker",
            "--listen",
            "127.0.0.1:0",
            "--exit-with-stdin",
            "--token-file",
        ])
        .arg(&token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(w.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let hello: serde_json::Value = serde_json::from_str(&line).unwrap();
    let addr = hello["listening"].as_str().unwrap().to_owned();
    assert_eq!(hello["protocol"], "oprobe.worker/1");
    assert!(
        !hello["clients"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "exec")
    );
    let wrong = env.path("wrong");
    std::fs::write(&wrong, "not-the-token-at-all-xxxxxxxx").unwrap();
    let o = env
        .cmd()
        .args([
            "run",
            "memory://",
            "-d",
            "200ms",
            "--warmup",
            "0s",
            "--workers",
            &addr,
            "--worker-token-file",
        ])
        .arg(&wrong)
        .args(["-o", "json"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    // In json mode the error document is on stdout.
    let all = format!("{}{}", String::from_utf8_lossy(&o.stdout), err(&o));
    assert!(
        all.contains("refused the token") && all.contains("auth.denied"),
        "{all}"
    );
    // Closing its stdin stops a local worker.
    drop(w.stdin.take());
    assert!(w.wait().unwrap().success());
}

#[test]
fn an_exec_client_drives_the_run() {
    if !Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipped: python3 is not installed");
        return;
    }
    let env = Env::new();
    let (_rt, lab) = background_lab();
    let adapter =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../adapters/minimal/oprobe_minimal.py");
    let o = env
        .cmd()
        .arg("run")
        .args(lab_args(&lab.endpoint_url))
        .args(["-c", "4", "-d", "1s", "-o", "json", "--client"])
        .arg(format!("exec:python3 -u '{}'", adapter.display()))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let v = json(&o);
    let r = &v["report"];
    assert_eq!(r["client"]["kind"], "exec");
    assert!(
        r["client"]["version"]
            .as_str()
            .unwrap()
            .starts_with("python-http.client")
    );
    assert!(r["summary"]["ops_count"].as_u64().unwrap() > 0);
    assert_eq!(r["summary"]["failed_ops_count"].as_u64(), Some(0));
    // The client is not part of the replayable spec: a run file never starts a command.
    assert!(!r["spec"].to_string().contains("oprobe_minimal"));
}
