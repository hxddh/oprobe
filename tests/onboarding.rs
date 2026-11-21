//! Onboarding (design v4 §10): `oprobe demo` is exactly `oprobe run lab://demo`, so a demo and a
//! plain `run lab://demo` (or a `--from` replay of the demo) compare as the same target and
//! workload. Every invocation uses its own config and data directory.

use std::process::{Command, Output};

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oprobe"))
            .env("OPROBE_DATA_DIR", self.dir.path().join("data"))
            .env("OPROBE_CONFIG", self.dir.path().join("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .args(["--color", "never"])
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "oprobe {args:?} exited {:?}\n{}",
            o.status.code(),
            String::from_utf8_lossy(&o.stderr)
        );
        serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
            panic!(
                "oprobe {args:?}: not JSON ({e}): {}",
                String::from_utf8_lossy(&o.stdout)
            )
        })
    }
}

/// The spec of a stored run, parsed back.
fn spec(env: &Env, run: &str) -> oprobe::model::RunSpec {
    let r = env.json(&["runs", "show", run, "-o", "json"]);
    let r = r.get("report").unwrap_or(&r);
    serde_json::from_value(r["spec"].clone()).unwrap()
}

#[test]
fn demo_and_run_lab_demo_are_comparable() {
    let env = Env::new();
    // 1. The demo, with its defaults.
    let demo = env.json(&["demo", "-o", "json"]);
    let demo_id = demo["run_id"].as_str().unwrap().to_owned();
    // Its suggested next steps are `--from` replays of this run.
    let next: Vec<String> = demo["next"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["cli"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(next[0], format!("oprobe run --from {demo_id}"), "{next:?}");
    assert!(
        next.iter()
            .any(|c| c.starts_with("oprobe compare") && c.contains(&demo_id)),
        "{next:?}"
    );

    // 2. `oprobe run lab://demo` with its defaults.
    let run = env.json(&["run", "lab://demo", "-q", "-o", "json"]);
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    // Same target identity, same workload, same knobs.
    let (a, b) = (spec(&env, &demo_id), spec(&env, &run_id));
    assert_eq!(a.target_ref, "lab://demo");
    assert_eq!(b.target_ref, "lab://demo");
    assert_eq!(a.target_id(), b.target_id());
    assert_eq!(a.workload_hash(), b.workload_hash());
    assert_eq!(a.hash(), b.hash());
    assert_eq!(
        a.profile.workload.preset,
        oprobe::config::Preset::SmallFiles
    );
    assert_eq!(a.profile.knobs.concurrency, 32);

    // 3. Compare: no workload or target difference (comparable); exit 0, or 3 for a noisy gate.
    let o = env.run(&["compare", &demo_id, &run_id, "-o", "json"]);
    assert!(
        matches!(o.status.code(), Some(0) | Some(3)),
        "compare exited {:?}: {}",
        o.status.code(),
        String::from_utf8_lossy(&o.stderr)
    );
    let c: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let diff = c.get("diff").unwrap_or(&c);
    if let Some(comparable) = diff.get("comparable").or_else(|| c.get("comparable")) {
        assert_eq!(comparable, true, "{c}");
    }
    let changes = diff["config_changes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let blocking: Vec<_> = changes
        .iter()
        .filter(|ch| ch["kind"] == "workload" || ch["kind"] == "target")
        .collect();
    assert!(blocking.is_empty(), "not comparable: {blocking:?}");
}

/// A tiny HTTP server answering every request with `status` and `body`.
fn serve(status: u16, body: &'static str) -> String {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming().take(8) {
            let Ok(mut s) = s else { continue };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = write!(
                s,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    format!("http://{addr}")
}

fn self_update(base: &str, api: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .env("OPROBE_DATA_DIR", dir.path().join("data"))
        .env("OPROBE_CONFIG", dir.path().join("config.toml"))
        .env("OPROBE_RELEASE_BASE", base)
        .env("OPROBE_UPDATE_API", api)
        .env_remove("GITHUB_TOKEN")
        .args(["--color", "never", "self-update", "--check", "-o", "json"])
        .output()
        .unwrap()
}

#[test]
fn self_update_reads_latest_json_before_the_api() {
    let base = serve(
        200,
        r#"{"schema":"oprobe.release/1","version":"99.1.0","tag":"v99.1.0","release_url":"https://example.invalid/r","assets":[]}"#,
    );
    let api = serve(403, r#"{"message":"API rate limit exceeded"}"#);
    let o = self_update(&base, &api);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["latest_version"], "99.1.0");
    assert_eq!(v["release_source"], "latest_json");
    assert_eq!(v["update_available"], true);
}

#[test]
fn self_update_falls_back_to_the_api_and_maps_403_to_a_network_error() {
    // latest.json missing: the API answers.
    let base = serve(404, "{}");
    let api = serve(
        200,
        r#"{"tag_name":"v99.2.0","html_url":"https://example.invalid/r","assets":[]}"#,
    );
    let o = self_update(&base, &api);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["latest_version"], "99.2.0");
    assert_eq!(v["release_source"], "api");
    // Both refused (GitHub's 403 rate limit): a retryable network error, never storage.throttled.
    let base = serve(403, "{}");
    let api = serve(403, r#"{"message":"API rate limit exceeded"}"#);
    let o = self_update(&base, &api);
    assert_eq!(o.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["error"]["code"], "net.unreachable", "{v}");
    assert_eq!(v["error"]["retryable"], true, "{v}");
}
