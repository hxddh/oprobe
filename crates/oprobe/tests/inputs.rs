//! No user input yields `internal` (design v4 §8): a sweep of bad inputs through the CLI
//! (`-o json`). Every error must be classified (`usage.*`, `config.*`, `auth.*`, `safety.*` or
//! `run.*`) and exit 2 for usage and config errors.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

fn cmd(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_oprobe"));
    c.env("OPROBE_DATA_DIR", dir.join("data"))
        .env("OPROBE_CONFIG", dir.join("config.toml"))
        .env_remove("OPROBE_OUTPUT")
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .stdin(Stdio::null());
    c
}

/// Run the CLI with `-o json`; returns `(exit code, error code, error)`.
fn cli_error(dir: &Path, args: &[&str]) -> (i32, String, Value) {
    let o = cmd(dir).args(args).args(["-o", "json"]).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| {
        panic!(
            "{args:?}: no JSON on stdout ({e}): {out}\nstderr: {}",
            String::from_utf8_lossy(&o.stderr)
        )
    });
    assert_eq!(v["type"], "error", "{args:?} did not fail: {v}");
    let code = v["error"]["code"].as_str().unwrap().to_owned();
    (o.status.code().unwrap_or(-1), code, v["error"].clone())
}

#[test]
fn bad_inputs_are_classified_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("bad.toml"),
        "url = \"memory://\"\nlayers = 3\n",
    )
    .unwrap();
    let bad_file = dir.path().join("bad.toml");
    let bad_file = bad_file.to_str().unwrap();
    let cases: &[(&[&str], &str)] = &[
        (&["run", "ftp://x"], "usage."),
        (&["run", "s3://"], "usage."),
        (&["run", "memory://", "--preset", "nope"], "usage."),
        (&["run", "memory://", "-d", "5x"], "usage."),
        (&["run", "memory://", "-c", "0"], "usage."),
        (&["run", "memory://", "--rate", "-3"], "usage."),
        (&["run", "memory://", "--object-size", "lots"], "usage."),
        (&["run", "memory://", "--max-bytes", "lots"], "usage."),
        (&["run", "memory://", "--set", "novalue"], "usage."),
        (
            &["run", "memory://", "--set", "layers.retry.max_times=lots"],
            "usage.",
        ),
        (&["run", "memory://", "--set", "read.chunk=1"], "usage."),
        (&["run", "memory://", "--set", "http.version=h3"], "usage."),
        (
            &[
                "run",
                "memory://",
                "--set",
                "layers.tail_cut.percentile=100",
            ],
            "usage.",
        ),
        (&["run", "memory://", "--set", "concurrency=0"], "usage."),
        (&["run", "memory://", "--set", "mix=["], "usage."),
        (
            &["run", "memory://", "--set", "options.no_such_option=1"],
            "config.",
        ),
        (
            &["run", "s3://bkt", "--set", "options.endpoint=ftp://x"],
            "usage.",
        ),
        (&["run", "no-such-operator"], "usage."),
        (&["run", "missing.toml"], "usage."),
        (&["run", bad_file], "config."),
        (&["show", "no-such-run"], "usage."),
        (&["prove", "no-such-run"], "usage."),
        (&["prove", "last", "--objective", "cost"], "usage."),
        (&["lab", "nope"], "usage."),
    ];
    for (args, family) in cases {
        let (exit, code, err) = cli_error(dir.path(), args);
        assert!(code.starts_with(family), "{args:?}: {code} ({err})");
        assert_eq!(exit, 2, "{args:?}: {code} ({err})");
        assert!(
            err["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{args:?}: no message ({err})"
        );
    }
}

#[test]
fn near_misses_get_a_did_you_mean_hint() {
    let dir = tempfile::tempdir().unwrap();
    for (args, want) in [
        (
            &["run", "memory://", "--preset", "smal-files"][..],
            "small-files",
        ),
        (
            &["run", "memory://", "--set", "layers.retry.max_tims=2"][..],
            "layers.retry.max_times",
        ),
        (
            &["run", "memory://", "--set", "concurency=2"][..],
            "concurrency",
        ),
        (&["lab", "stragglrs"][..], "stragglers"),
    ] {
        let (_, _, err) = cli_error(dir.path(), args);
        let hint = err["hint"].as_str().unwrap_or_default();
        assert!(hint.contains(want), "{args:?}: {hint}");
    }
}

/// Missing credentials are `auth.no_credentials`, not `internal` (from the preflight).
#[test]
fn missing_gcs_credentials_are_auth_no_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let (exit, code, err) = cli_error(
        dir.path(),
        &[
            "run",
            "gcs://oprobe-no-such-bucket/x",
            "-d",
            "1s",
            "--set",
            "options.disable_vm_metadata=true",
            "--set",
            "options.disable_config_load=true",
        ],
    );
    assert_eq!(code, "auth.no_credentials", "{err}");
    assert_eq!(exit, 1);
}

/// Formats other than text and JSON are refused before anything runs.
#[test]
fn only_text_and_json_outputs_exist() {
    let dir = tempfile::tempdir().unwrap();
    for f in ["ndjson", "md", "html"] {
        let o = cmd(dir.path())
            .args(["show", "--list", "-o", f])
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(2), "-o {f}");
    }
}

/// A pasted web address that is not a storage URL is refused without echoing its signature
/// (a pre-signed URL, a SAS token) or credentials, on stdout or stderr.
#[cfg(not(feature = "all-services"))]
#[test]
fn a_refused_web_address_never_echoes_its_secrets() {
    let dir = tempfile::tempdir().unwrap();
    for url in [
        "https://user:hunter2@minio.local:9000/b/x?X-Amz-Signature=deadbeef1234&X-Amz-Credential=AKIAEXAMPLE",
        "https://example.com/c/blob?sv=2024-01-01&sig=sastoken1234",
        "https://b.s3-website-us-east-1.amazonaws.com/k?X-Amz-Signature=deadbeef1234",
    ] {
        let o = cmd(dir.path())
            .args(["run", url, "-o", "json"])
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        let err = String::from_utf8_lossy(&o.stderr);
        let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{url}: {e}: {out}"));
        assert_eq!(v["error"]["code"], "usage.invalid_argument", "{v}");
        assert_eq!(o.status.code(), Some(2));
        for secret in ["hunter2", "deadbeef1234", "AKIAEXAMPLE", "sastoken1234"] {
            assert!(!out.contains(secret), "{secret} on stdout: {out}");
            assert!(!err.contains(secret), "{secret} on stderr: {err}");
        }
    }
    // An S3 website address is not an API endpoint: no region is made of it.
    let (_, _, e) = cli_error(
        dir.path(),
        &["run", "https://b.s3-website-us-east-1.amazonaws.com/k"],
    );
    assert!(!e.to_string().contains("region=website"), "{e}");
    assert!(
        e["hint"]
            .as_str()
            .is_some_and(|h| h.contains("s3://bucket/prefix")),
        "{e}"
    );
}
