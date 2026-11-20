//! `oprobe compat` against the lab (every check passes or is skipped for a documented lab
//! limitation) and an in-process backend; the CLI's safety and gate behavior.

use std::process::Command;

use oprobe::compat::{CompatStatus, run_suite};
use oprobe::config::TargetSpec;

fn oprobe_cmd() -> Command {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| tempfile::tempdir().unwrap());
    let mut c = Command::new(env!("CARGO_BIN_EXE_oprobe"));
    c.env("OPROBE_DATA_DIR", home.path().join("data"))
        .env("OPROBE_CONFIG", home.path().join("config.toml"))
        .env_remove("OPROBE_OUTPUT");
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn the_lab_passes_every_applicable_check() {
    let t = TargetSpec::from_url("lab://clean/compat")
        .unwrap()
        .normalized()
        .unwrap();
    let base = oprobe::engine::operator::base_operator(&t).unwrap();
    let op = oprobe::engine::operator::preflight_operator(&base, &t).unwrap();
    let mut r = run_suite(&op, &t.display(), &t.scheme, "oprobe-compat-test/").await;
    r.push(oprobe::compat::checksum_check(&t, "oprobe-compat-test/").await);
    let failed: Vec<_> = r
        .checks
        .iter()
        .filter(|c| c.status == CompatStatus::Fail)
        .collect();
    assert!(failed.is_empty(), "{failed:#?}");
    // Lab limitations: no versioning, no redirects.
    let skipped: Vec<&str> = r
        .checks
        .iter()
        .filter(|c| c.status == CompatStatus::Skipped)
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(skipped, ["versioning", "redirects"]);
    assert_eq!(r.score_pct, 100.0);
    for id in [
        "conditional_create",
        "conditional_overwrite",
        "range_out_of_bounds",
        "multipart_abort",
        "user_metadata",
        "checksum_crc32c",
        "list_pagination",
        "batch_delete",
    ] {
        assert!(
            r.checks
                .iter()
                .any(|c| c.id == id && c.status == CompatStatus::Pass),
            "{id}"
        );
    }
}

#[test]
fn compat_command_reports_a_score_and_cleans_up() {
    let out = oprobe_cmd()
        .args(["compat", "lab://clean", "-o", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["score_pct"], 100.0, "{v:#}");
    assert_eq!(v["cleaned_up"], true);
    assert!(v["prefix"].as_str().unwrap().starts_with("oprobe-compat-"));
    assert!(v["checks"].as_array().unwrap().len() >= 20);

    // In-process backends skip what they cannot do; the gate fails below the minimum.
    let out = oprobe_cmd()
        .args(["compat", "memory://", "--min-score", "101", "-o", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));

    // Read-only refuses (exit 5) before touching the target.
    let out = oprobe_cmd()
        .args(["compat", "lab://clean", "--read-only"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&out.stderr).contains("read-only"));
}
