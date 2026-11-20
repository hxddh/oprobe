//! oprobe against real storage implementations instead of its own lab server: MinIO (S3),
//! fake-gcs-server (GCS JSON API) and Azurite (Azure Blob). CI starts them and exports the
//! variables below (see `.github/workflows/ci.yml`, job `backends`); without a variable the
//! corresponding test has nothing to talk to and returns early with a note.
//!
//! - `OPROBE_IT_S3_ENDPOINT`     MinIO, bucket `bench`, credentials `minioadmin`/`minioadmin`
//! - `OPROBE_IT_GCS_ENDPOINT`    fake-gcs-server (no auth), bucket `bench`
//! - `OPROBE_IT_AZBLOB_ENDPOINT` Azurite (`http://127.0.0.1:10000/<account>`), container `bench`, with
//!   `OPROBE_IT_AZBLOB_ACCOUNT` / `OPROBE_IT_AZBLOB_KEY` (CI generates the key per run)

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use oprobe::config::*;
use oprobe::engine::{Cancel, RunOptions, Session};
use oprobe::trace::Api;
use oprobe::util::ByteSize;

fn target(scheme: &str, opts: &[(&str, &str)]) -> TargetSpec {
    let mut options: BTreeMap<String, String> = opts
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    options.insert("root".into(), "/oprobe-it".into());
    TargetSpec {
        url: None,
        scheme: scheme.into(),
        options,
        proxy: Some("none".into()),
        ..Default::default()
    }
}

/// A mixed read/write/stat workload through `mode`; asserts what must hold on any backend.
async fn exercise(t: TargetSpec, mode: TransportMode, expect_put: Api) {
    let mut p = Profile::from_target(t);
    p.workload.preset = Preset::Mixed;
    let (mut ds, _) = Preset::Mixed.defaults();
    ds.objects = 8;
    ds.object_size = ByteSize::kib(256);
    p.workload.dataset = Some(ds);
    p.knobs.concurrency = 4;
    p.knobs.transport = mode;
    let s = Session::open(p, Cancel::new(), Arc::new(|_| {}))
        .await
        .expect("session opens");
    let knobs = s.profile.knobs.clone();
    let snap = s
        .run(
            &knobs,
            &RunOptions {
                duration: Duration::from_millis(1500),
                warmup: Duration::from_millis(300),
                max_ops: None,
                rate: None,
                trace_slow_ops: true,
            },
            None,
        )
        .await
        .expect("run succeeds");
    let cleanup = s.close().await;

    assert!(snap.summary.ops > 0, "no operations completed");
    assert_eq!(
        snap.summary.failed_ops, 0,
        "failed operations: {:?}",
        snap.verbs
    );
    assert!(snap.connections.requests > 0, "no HTTP requests traced");
    for api in [Api::GetObject, Api::HeadObject, expect_put] {
        let a = snap.apis.iter().find(|a| a.api == api).unwrap_or_else(|| {
            panic!(
                "{api:?} not classified; saw {:?}",
                snap.apis.iter().map(|a| a.api).collect::<Vec<_>>()
            )
        });
        assert!(a.count > 0);
        assert!(a.stages.ttfb.count > 0, "{api:?} has no TTFB samples");
    }
    assert!(snap.integrity.verified > 0, "nothing verified");
    assert_eq!(snap.integrity.corrupted, 0);
    assert!(cleanup.verified, "cleanup not verified: {cleanup:?}");
}

/// The compatibility suite (`oprobe compat`): what the emulator implements passes; checks it
/// cannot run are skipped; the report is printed for the CI log. Test objects are removed.
async fn compat(t: TargetSpec) {
    let t = t.normalized().unwrap();
    let base = oprobe::engine::operator::base_operator(&t).unwrap();
    let op = oprobe::engine::operator::preflight_operator(&base, &t).unwrap();
    let prefix = "oprobe-compat-it/";
    let mut r = oprobe::compat::run_suite(&op, &t.display(), &t.scheme, prefix).await;
    r.push(oprobe::compat::checksum_check(&t, prefix).await);
    let admin = oprobe::engine::operator::admin_operator(&base, &t).unwrap();
    let cleaned = oprobe::engine::cleanup::delete_prefix(&admin, prefix).await;
    for c in &r.checks {
        eprintln!("{:?} {} {}: {}", c.status, c.category, c.id, c.detail);
    }
    eprintln!("{}: score {:.0}%", t.scheme, r.score_pct);
    assert!(cleaned.verified, "{cleaned:?}");
    // The basics hold everywhere.
    for id in [
        "read_after_write",
        "read_after_overwrite",
        "range_reads",
        "delete_idempotent",
    ] {
        let c = r.checks.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            c.status,
            oprobe::compat::CompatStatus::Pass,
            "{id}: {}",
            c.detail
        );
    }
}

fn env(name: &str) -> Option<String> {
    let v = std::env::var(name).ok().filter(|v| !v.is_empty());
    if v.is_none() {
        eprintln!("{name} not set: no backend to test against (CI provides one)");
    }
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn minio_s3() {
    let Some(ep) = env("OPROBE_IT_S3_ENDPOINT") else {
        return;
    };
    for mode in [TransportMode::Opendal, TransportMode::Microscope] {
        let t = target(
            "s3",
            &[
                ("endpoint", &ep),
                ("bucket", "bench"),
                ("region", "us-east-1"),
                ("access_key_id", "minioadmin"),
                ("secret_access_key", "minioadmin"),
                ("disable_config_load", "true"),
            ],
        );
        if mode == TransportMode::Opendal {
            compat(t.clone()).await;
        }
        exercise(t, mode, Api::PutObject).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fake_gcs() {
    let Some(ep) = env("OPROBE_IT_GCS_ENDPOINT") else {
        return;
    };
    for mode in [TransportMode::Opendal, TransportMode::Microscope] {
        let t = target(
            "gcs",
            &[
                ("endpoint", &ep),
                ("bucket", "bench"),
                ("allow_anonymous", "true"),
                ("disable_vm_metadata", "true"),
                ("disable_config_load", "true"),
            ],
        );
        if mode == TransportMode::Opendal {
            compat(t.clone()).await;
        }
        exercise(t, mode, Api::PutObject).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn azurite_blob() {
    let (Some(ep), Some(account), Some(key)) = (
        env("OPROBE_IT_AZBLOB_ENDPOINT"),
        env("OPROBE_IT_AZBLOB_ACCOUNT"),
        env("OPROBE_IT_AZBLOB_KEY"),
    ) else {
        return;
    };
    for mode in [TransportMode::Opendal, TransportMode::Microscope] {
        let t = target(
            "azblob",
            &[
                ("endpoint", &ep),
                ("container", "bench"),
                ("account_name", &account),
                ("account_key", &key),
            ],
        );
        if mode == TransportMode::Opendal {
            compat(t.clone()).await;
        }
        exercise(t, mode, Api::PutObject).await;
    }
}
