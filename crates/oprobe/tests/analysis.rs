//! Findings end to end: a lab with stragglers, profiled through the CLI, its finding, evidence
//! and remedy, and `show --by stage`. Every test uses its own config and data directory.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

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
            .args(["--color", "never"]);
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
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

fn background_lab(cfg: LabConfig) -> (tokio::runtime::Runtime, oprobe::lab::LabHandle) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(async {
        LabServer::spawn(
            "127.0.0.1:0",
            LabConfig {
                throttle_status_code: 503,
                ..cfg
            },
        )
        .await
        .expect("lab starts")
    });
    (rt, h)
}

fn lab_args(endpoint: &str) -> Vec<String> {
    let endpoint = format!("options.endpoint={endpoint}");
    [
        "s3://bkt/x",
        "--set",
        endpoint.as_str(),
        "--set",
        "http.proxy=none",
        "--set",
        "options.access_key_id=x",
        "--set",
        "options.secret_access_key=x",
        "--set",
        "options.region=us-east-1",
        "--preset",
        "small-files",
        "--objects",
        "8",
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
fn stragglers_are_found_explained_and_attributed() {
    let env = Env::new();
    let (_rt, lab) = background_lab(LabConfig {
        straggler_every_n: 25,
        straggler_delay: Duration::from_millis(300),
        base_ttfb_delay: Duration::from_millis(3),
        ..Default::default()
    });
    let o = env
        .cmd()
        .arg("run")
        .args(lab_args(&lab.endpoint_url))
        .args(["-c", "8", "-d", "3s", "-o", "json"])
        // A retry that re-issues a cut request quickly (OpenDAL's default waits 1 s).
        .args([
            "--set",
            "layers.retry.min_delay=50ms",
            "--set",
            "layers.retry.jitter=on",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let v = &json(&o)["runs"][0];
    let findings = v["report"]["findings"].as_array().unwrap();
    let tail = findings
        .iter()
        .find(|f| f["code"] == "tail_stragglers")
        .unwrap_or_else(|| panic!("no straggler finding: {findings:#?}"));
    assert!(
        tail["cause"]
            .as_str()
            .unwrap()
            .contains("tail-cut (deadline"),
        "{tail}"
    );
    let vals = &tail["evidence"]["values"];
    let predicted = vals["predicted_p99_ms"].as_f64().unwrap();
    let current = vals["current_p99_ms"].as_f64().unwrap();
    // The injected stragglers wait 300 ms on top of a few ms: whatever the runner's speed,
    // some requests are above the threshold, the threshold is below the straggler delay and
    // a tail-cut takes most of the p99 away.
    assert!(vals["straggler_pct"].as_f64().unwrap() > 0.0, "{tail}");
    assert!(
        vals["straggler_ttfb_ms"].as_f64().unwrap() >= 250.0,
        "{tail}"
    );
    assert!(
        vals["tail_cut_deadline_ms"].as_f64().unwrap() < 300.0,
        "{tail}"
    );
    assert!(current >= 250.0, "{tail}");
    assert!(predicted < current / 2.0, "{tail}");
    assert!(!tail["evidence"]["op_ids"].as_array().unwrap().is_empty());
    let ver = &tail["verification"];
    let run_id = v["run_id"].as_str().unwrap();
    assert_eq!(
        ver["command"].as_str().unwrap(),
        format!("oprobe prove {run_id} tail_stragglers")
    );
    assert_eq!(
        ver["overrides"]["operator"]["layers.tail_cut"], "on",
        "{ver}"
    );
    let rerun = ver["rerun"].as_str().unwrap();
    assert!(
        rerun.starts_with(&format!("oprobe prove {run_id} --with "))
            && rerun.contains("--with layers.tail_cut=on"),
        "{rerun}"
    );
    let cmd = ver["replay"].as_str().unwrap();
    assert!(
        cmd.contains("--set layers.tail_cut=on")
            && cmd.contains(&format!("--set options.endpoint={}", lab.endpoint_url))
            && cmd.contains("-c 8"),
        "{cmd}"
    );
    // The replay is valid: it plans the same run with tail-cut on.
    {
        let words: Vec<&str> = cmd.split_whitespace().collect();
        assert_eq!(words[0], "oprobe");
        let plan = env
            .cmd()
            .args(&words[1..])
            .args(["--dry-run", "-o", "json"])
            .output()
            .unwrap();
        assert!(plan.status.success(), "{cmd}: {}", err(&plan));
    }
    // At most one tail finding.
    assert_eq!(
        findings
            .iter()
            .filter(|f| f["code"].as_str().unwrap().starts_with("tail"))
            .count(),
        1
    );
    // Where the time went: the stragglers' time is TTFB.
    let by = env.run(&["show", run_id, "--by", "stage", "-o", "json"]);
    assert!(by.status.success(), "{}", err(&by));
    let bv = json(&by);
    assert!(
        bv["by"]["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["api"] == "GetObject"),
        "{bv}"
    );
    assert_eq!(bv["by"]["groups"][0]["key"], "ttfb", "{bv}");
}
