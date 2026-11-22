//! `oprobe compat`: a correctness & compatibility suite for a storage target.
//!
//! Every check runs through OpenDAL (what applications use) against objects under one run
//! prefix (`oprobe-run-<id>/`, swept by `oprobe cleanup`), which is deleted (and verified empty) at the end. Checks
//! the service's capability rules out are skipped, not failed; the score is the share of the
//! checks that ran and passed.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures::TryStreamExt;
use opendal::{Buffer, Capability, ErrorKind, Operator};
use schemars::JsonSchema;
use serde::Serialize;

/// Outcome of one check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompatStatus {
    Pass,
    Fail,
    Skipped,
}

/// One compatibility check.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CompatCheck {
    /// Stable identifier, e.g. `conditional_create`.
    pub id: String,
    /// `consistency`, `conditional`, `multipart`, `range`, `integrity`, `metadata`, `delete`,
    /// `list`, `copy`, `versioning`, `http`.
    pub category: String,
    pub status: CompatStatus,
    /// What happened (the observed behavior, or why the check was skipped).
    pub detail: String,
    pub duration_ms: f64,
}

/// Result of the suite.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CompatReport {
    pub target: String,
    pub scheme: String,
    /// Object prefix the suite wrote under (deleted afterwards).
    pub prefix: String,
    pub checks: Vec<CompatCheck>,
    pub passed_count: u64,
    pub failed_count: u64,
    pub skipped_count: u64,
    /// passed / (passed + failed), percent.
    pub score_pct: f64,
    /// The prefix was deleted and verified empty.
    pub cleaned_up: bool,
}

type Check = Result<Outcome>;

/// A check's own verdict: passed (with what was observed) or not applicable.
enum Outcome {
    Pass(String),
    Skip(String),
}

fn pass(s: impl Into<String>) -> Check {
    Ok(Outcome::Pass(s.into()))
}

fn skip(s: impl Into<String>) -> Check {
    Ok(Outcome::Skip(s.into()))
}

/// Deterministic test payload.
fn payload(seed: u64, len: usize) -> Buffer {
    crate::engine::dataset::generate_payload(seed, len).0.into()
}

fn same(a: &Buffer, b: &Buffer) -> bool {
    a.len() == b.len() && a.to_bytes() == b.to_bytes()
}

/// The error kind an operation failed with, or `None` when it succeeded.
fn kind_of<T>(r: &opendal::Result<T>) -> Option<ErrorKind> {
    r.as_ref().err().map(|e| e.kind())
}

struct Suite<'a> {
    op: &'a Operator,
    cap: Capability,
    prefix: String,
    checks: Vec<CompatCheck>,
}

impl Suite<'_> {
    fn key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    async fn run<F>(&mut self, id: &str, category: &str, f: F)
    where
        F: std::future::Future<Output = Check>,
    {
        let t = Instant::now();
        // A hung check must not hang the suite.
        let res = match tokio::time::timeout(Duration::from_secs(120), f).await {
            Ok(r) => r,
            Err(_) => Err(anyhow!("timed out after 120 s")),
        };
        let (status, detail) = match res {
            Ok(Outcome::Pass(d)) => (CompatStatus::Pass, d),
            Ok(Outcome::Skip(d)) => (CompatStatus::Skipped, d),
            Err(e) => (CompatStatus::Fail, format!("{e:#}")),
        };
        self.checks.push(CompatCheck {
            id: id.to_owned(),
            category: category.to_owned(),
            status,
            detail,
            duration_ms: t.elapsed().as_secs_f64() * 1000.0,
        });
    }
}

/// Run the suite against `op` (an operator for the target; writes stay under `prefix`).
pub async fn run_suite(op: &Operator, target: &str, scheme: &str, prefix: &str) -> CompatReport {
    let cap = op.info().capability();
    let mut s = Suite {
        op,
        cap,
        prefix: prefix.to_owned(),
        checks: Vec::new(),
    };
    let op = s.op;
    let cap = s.cap;

    // --- consistency ----------------------------------------------------------------------
    let k = s.key("raw.bin");
    s.run("read_after_write", "consistency", async {
        let data = payload(1, 64 * 1024);
        op.write(&k, data.clone()).await?;
        let got = op.read(&k).await?;
        ensure!(
            same(&got, &data),
            "read {} bytes that differ from the {} written",
            got.len(),
            data.len()
        );
        pass("a new object is readable at once with the bytes written")
    })
    .await;

    let k = s.key("overwrite.bin");
    s.run("read_after_overwrite", "consistency", async {
        op.write(&k, payload(2, 4096)).await?;
        let v2 = payload(3, 8192);
        op.write(&k, v2.clone()).await?;
        let got = op.read(&k).await?;
        ensure!(
            same(&got, &v2),
            "a read right after the overwrite returned the old version"
        );
        pass("an overwrite is visible to the next read")
    })
    .await;

    let k = s.key("stat.bin");
    s.run("stat_after_write", "consistency", async {
        op.write(&k, payload(4, 1234)).await?;
        let m = op.stat(&k).await?;
        ensure!(
            m.content_length() == 1234,
            "stat reports {} bytes, wrote 1234",
            m.content_length()
        );
        pass(match m.etag() {
            Some(e) => format!("size matches, etag {e}"),
            None => "size matches (no etag)".into(),
        })
    })
    .await;

    let dir = s.key("list/");
    s.run("list_after_write", "consistency", async {
        if !cap.list {
            return skip("the service cannot list");
        }
        let names: Vec<String> = (0..3).map(|i| format!("{dir}k{i}.bin")).collect();
        for n in &names {
            op.write(n, payload(5, 100)).await?;
        }
        let listed: Vec<String> = op
            .lister_with(&dir)
            .await?
            .map_ok(|e| e.path().to_owned())
            .try_collect()
            .await?;
        let missing: Vec<&String> = names.iter().filter(|n| !listed.contains(n)).collect();
        ensure!(
            missing.is_empty(),
            "listed right after writing, missing: {missing:?}"
        );
        pass("new objects are listed at once")
    })
    .await;

    let k = s.key("missing.bin");
    s.run("not_found", "http", async {
        let r = op.stat(&k).await;
        ensure!(
            kind_of(&r) == Some(ErrorKind::NotFound),
            "stat of a missing key: {:?} (expected NotFound)",
            r.err()
        );
        let r = op.read(&k).await;
        ensure!(
            kind_of(&r) == Some(ErrorKind::NotFound),
            "read of a missing key: {:?}",
            r.err()
        );
        pass("stat and read of a missing key fail with NotFound")
    })
    .await;

    let k = s.key("odd name/a b+c%ü=é~.bin");
    s.run("special_characters", "http", async {
        let data = payload(6, 300);
        op.write(&k, data.clone()).await?;
        ensure!(same(&op.read(&k).await?, &data), "read back differs");
        if cap.list {
            let dir = k
                .rsplit_once('/')
                .map(|(d, _)| format!("{d}/"))
                .unwrap_or_default();
            let listed: Vec<String> = op
                .lister_with(&dir)
                .await?
                .map_ok(|e| e.path().to_owned())
                .try_collect()
                .await?;
            ensure!(listed.contains(&k), "listed as {listed:?}");
        }
        pass("spaces, +, %, = and non-ASCII round-trip in keys")
    })
    .await;

    let k = s.key("empty.bin");
    s.run("empty_object", "consistency", async {
        if !cap.write_can_empty {
            return skip("the service cannot store empty objects");
        }
        op.write(&k, Buffer::new()).await?;
        ensure!(op.stat(&k).await?.content_length() == 0, "size is not 0");
        ensure!(op.read(&k).await?.is_empty(), "read returned bytes");
        pass("a zero-byte object round-trips")
    })
    .await;

    // --- conditional requests -----------------------------------------------------------
    let k = s.key("cond-create.bin");
    s.run("conditional_create", "conditional", async {
        if !cap.write_with_if_not_exists {
            return skip("the service has no create-if-absent (If-None-Match: *)");
        }
        op.write_with(&k, payload(7, 100))
            .if_not_exists(true)
            .await?;
        let second = op.write_with(&k, payload(8, 100)).if_not_exists(true).await;
        ensure!(
            kind_of(&second) == Some(ErrorKind::ConditionNotMatch),
            "a second create of the same key: {:?} (expected a 412 precondition failure)",
            second.err()
        );
        pass("create-if-absent succeeds once, then fails with 412")
    })
    .await;

    let k = s.key("cond-overwrite.bin");
    s.run("conditional_overwrite", "conditional", async {
        if !cap.write_with_if_match {
            return skip("the service has no If-Match writes");
        }
        let m = op.write(&k, payload(9, 100)).await?;
        let etag = match m.etag() {
            Some(e) => e.to_owned(),
            None => op.stat(&k).await?.etag().context("no etag")?.to_owned(),
        };
        op.write_with(&k, payload(10, 100)).if_match(&etag).await?;
        let stale = op.write_with(&k, payload(11, 100)).if_match(&etag).await;
        ensure!(
            kind_of(&stale) == Some(ErrorKind::ConditionNotMatch),
            "an overwrite with a stale etag: {:?} (expected 412)",
            stale.err()
        );
        ensure!(
            same(&op.read(&k).await?, &payload(10, 100)),
            "the stale write changed the object"
        );
        pass("If-Match overwrites with the current etag and rejects a stale one with 412")
    })
    .await;

    let k = s.key("cond-read.bin");
    s.run("conditional_read", "conditional", async {
        if !cap.read_with_if_match {
            return skip("the service has no conditional reads");
        }
        op.write(&k, payload(12, 100)).await?;
        let r = op.read_with(&k).if_match("\"oprobe-no-such-etag\"").await;
        ensure!(
            kind_of(&r) == Some(ErrorKind::ConditionNotMatch),
            "a read with a wrong If-Match: {:?} (expected 412)",
            r.err()
        );
        pass("a read with a wrong If-Match fails with 412")
    })
    .await;

    // --- ranges ---------------------------------------------------------------------------
    let k = s.key("range.bin");
    let size = 10_000u64;
    s.run("range_reads", "range", async {
        let data = payload(13, size as usize).to_bytes();
        op.write(&k, data.clone()).await?;
        let got = op.read_with(&k).range(100..200).await?.to_bytes();
        ensure!(got == data.slice(100..200), "bytes 100-199 differ");
        let got = op.read_with(&k).range(9_000..).await?.to_bytes();
        ensure!(
            got == data.slice(9_000..),
            "open-ended range differs ({} bytes)",
            got.len()
        );
        pass("bounded and open-ended ranges return exactly the requested bytes")
    })
    .await;

    s.run("range_suffix", "range", async {
        if !cap.read_with_suffix {
            return skip("the service cannot read suffix ranges");
        }
        let data = payload(13, size as usize).to_bytes();
        let got = op
            .read_with(&k)
            .range(opendal::BytesRange::suffix(100))
            .await?
            .to_bytes();
        ensure!(
            got == data.slice(9_900..),
            "the last 100 bytes differ ({} bytes)",
            got.len()
        );
        let all = op
            .read_with(&k)
            .range(opendal::BytesRange::suffix(size + 500))
            .await?;
        ensure!(
            all.len() as u64 == size,
            "a suffix longer than the object returned {} bytes",
            all.len()
        );
        pass("suffix ranges (bytes=-N) return the tail, the whole object when N > size")
    })
    .await;

    s.run("range_out_of_bounds", "range", async {
        let r = op.read_with(&k).range(size + 10..size + 20).await;
        match r {
            Err(e) if e.kind() == ErrorKind::RangeNotSatisfied => {
                pass("a range past the end fails with 416")
            }
            // Some OpenDAL services (gcs, azblob) report the server's 416 as `Unexpected`: the
            // server behaved correctly, so the check passes (the message names the status).
            Err(e) if format!("{e:#}").contains("416") => pass(
                "a range past the end fails with 416 (reported by OpenDAL as an unexpected error)",
            ),
            Ok(b) if b.is_empty() => pass("a range past the end returns no bytes"),
            Ok(b) => bail!("a range past the end returned {} bytes", b.len()),
            Err(e) => bail!("a range past the end: {e} (expected 416 Range Not Satisfiable)"),
        }
    })
    .await;

    // --- multipart ------------------------------------------------------------------------
    let min = cap.write_multi_min_size.unwrap_or(5 * 1024 * 1024);
    let k = s.key("multipart.bin");
    s.run("multipart_small_last_part", "multipart", async {
        if !cap.write_can_multi {
            return skip("the service has no multipart uploads");
        }
        let data = payload(14, 2 * min + 1024).to_bytes();
        let mut w = op.writer_with(&k).chunk(min).await?;
        w.write(data.clone()).await?;
        w.close().await?;
        let got = op.read(&k).await?.to_bytes();
        ensure!(
            got == data,
            "the assembled object differs ({} of {} bytes)",
            got.len(),
            data.len()
        );
        pass(format!(
            "3 parts of {} + {} + 1 KiB assemble into the object written",
            crate::util::ByteSize(min as u64),
            crate::util::ByteSize(min as u64)
        ))
    })
    .await;

    let k = s.key("multipart-abort.bin");
    s.run("multipart_abort", "multipart", async {
        if !cap.write_can_multi {
            return skip("the service has no multipart uploads");
        }
        let mut w = op.writer_with(&k).chunk(min).await?;
        w.write(payload(15, min + 1)).await?;
        w.abort().await?;
        let r = op.stat(&k).await;
        ensure!(
            kind_of(&r) == Some(ErrorKind::NotFound),
            "after abort the object exists: {:?}",
            r.map(|m| m.content_length())
        );
        pass("an aborted upload leaves no object")
    })
    .await;

    // --- integrity & metadata -----------------------------------------------------------
    let k = s.key("meta.bin");
    s.run("user_metadata", "metadata", async {
        if !cap.write_with_user_metadata {
            return skip("the service does not store user metadata");
        }
        let meta = [
            // Names valid everywhere: Azure requires C#-identifier names (no `-`), S3/GCS
            // lower-case them.
            ("oprobea".to_owned(), "1".to_owned()),
            ("oprobeb".to_owned(), "two words".to_owned()),
        ];
        op.write_with(&k, payload(16, 10))
            .user_metadata(meta.clone())
            .await?;
        let m = op.stat(&k).await?;
        let got = m.user_metadata();
        for (key, v) in &meta {
            let have = got.as_ref().and_then(|g| g.get(key));
            ensure!(
                have == Some(v.as_str()),
                "user metadata {key} = {have:?} (wrote {v:?})"
            );
        }
        pass("user metadata round-trips through stat")
    })
    .await;

    let k = s.key("ctype.json");
    s.run("content_type", "metadata", async {
        if !cap.write_with_content_type {
            return skip("the service does not store content types");
        }
        op.write_with(&k, payload(17, 10))
            .content_type("application/json")
            .await?;
        let m = op.stat(&k).await?;
        ensure!(
            m.content_type() == Some("application/json"),
            "content type {:?}",
            m.content_type()
        );
        pass("the content type round-trips")
    })
    .await;

    // --- copy / delete / list -------------------------------------------------------------
    let (src, dst) = (s.key("copy-src.bin"), s.key("copy-dst.bin"));
    s.run("copy", "copy", async {
        if !cap.copy {
            return skip("the service has no server-side copy");
        }
        let data = payload(18, 5000);
        op.write(&src, data.clone()).await?;
        op.copy(&src, &dst).await?;
        ensure!(same(&op.read(&dst).await?, &data), "the copy differs");
        pass("server-side copy produces identical bytes")
    })
    .await;

    let k = s.key("delete.bin");
    s.run("delete_idempotent", "delete", async {
        op.write(&k, payload(19, 10)).await?;
        op.delete(&k).await?;
        let again = op.delete(&k).await;
        ensure!(
            again.is_ok(),
            "deleting a missing key failed: {:?}",
            again.err()
        );
        ensure!(
            kind_of(&op.stat(&k).await) == Some(ErrorKind::NotFound),
            "deleted object still visible"
        );
        pass("delete removes the object; deleting again succeeds")
    })
    .await;

    let dir = s.key("batch/");
    s.run("batch_delete", "delete", async {
        let max = cap.delete_max_size.unwrap_or(1);
        if max < 2 {
            return skip("the service deletes one key per request");
        }
        let keys: Vec<String> = (0..5).map(|i| format!("{dir}d{i}.bin")).collect();
        for k in &keys {
            op.write(k, payload(20, 10)).await?;
        }
        op.delete_iter(keys.clone()).await?;
        for k in &keys {
            ensure!(
                kind_of(&op.stat(k).await) == Some(ErrorKind::NotFound),
                "{k} survived the batch"
            );
        }
        pass(format!(
            "one batch request deleted 5 keys (≤ {max} per batch)"
        ))
    })
    .await;

    let dir = s.key("pages/");
    s.run("list_pagination", "list", async {
        if !cap.list_with_limit {
            return skip("the service lists without a page size");
        }
        let keys: Vec<String> = (0..7).map(|i| format!("{dir}p{i}.bin")).collect();
        for k in &keys {
            op.write(k, payload(21, 10)).await?;
        }
        let listed: Vec<String> = op
            .lister_with(&dir)
            .limit(2)
            .await?
            .map_ok(|e| e.path().to_owned())
            .try_collect()
            .await?;
        let missing: Vec<&String> = keys.iter().filter(|k| !listed.contains(k)).collect();
        ensure!(missing.is_empty(), "pages of 2 lost keys: {missing:?}");
        ensure!(
            listed.len() == keys.len(),
            "pages of 2 listed {} keys, wrote {}",
            listed.len(),
            keys.len()
        );
        pass("7 keys listed completely in pages of 2 (continuation tokens)")
    })
    .await;

    let k = s.key("versions.bin");
    s.run("versioning", "versioning", async {
        if !cap.list_with_versions || !cap.read_with_version {
            return skip("versioning is not enabled for this target (enable_versioning)");
        }
        op.write(&k, payload(22, 10)).await?;
        op.write(&k, payload(23, 10)).await?;
        let versions: Vec<opendal::Entry> = op
            .lister_with(&k)
            .versions(true)
            .await?
            .try_collect()
            .await?;
        match versions.len() {
            0 => bail!("listing versions of an existing object returned none"),
            1 => skip("versioning is not enabled on this bucket (one version kept per key)"),
            n => pass(format!("{n} versions listed after two writes")),
        }
    })
    .await;

    s.run("redirects", "http", async {
        skip("needs an endpoint that redirects (307); not tested")
    })
    .await;

    let checks = s.checks;
    let n = |st: CompatStatus| checks.iter().filter(|c| c.status == st).count() as u64;
    let (passed, failed, skipped) = (
        n(CompatStatus::Pass),
        n(CompatStatus::Fail),
        n(CompatStatus::Skipped),
    );
    CompatReport {
        target: target.to_owned(),
        scheme: scheme.to_owned(),
        prefix: prefix.to_owned(),
        checks,
        passed_count: passed,
        failed_count: failed,
        skipped_count: skipped,
        score_pct: if passed + failed == 0 {
            0.0
        } else {
            passed as f64 * 100.0 / (passed + failed) as f64
        },
        cleaned_up: false,
    }
}

/// The checksum check: OpenDAL's S3 service computing CRC32C on writes (`checksum_algorithm`)
/// and the server accepting it. S3 only.
pub async fn checksum_check(target: &crate::config::TargetSpec, prefix: &str) -> CompatCheck {
    let t = Instant::now();
    let res: Check = async {
        if target.scheme != "s3" {
            return skip("checksum headers are checked for S3-compatible targets");
        }
        let mut t2 = target.clone();
        t2.options
            .insert("checksum_algorithm".into(), "crc32c".into());
        let base = crate::engine::operator::base_operator(&t2)?;
        let op = crate::engine::operator::preflight_operator(&base, &t2)?;
        let k = format!("{prefix}crc32c.bin");
        let data = payload(24, 70_000);
        op.write(&k, data.clone()).await?;
        ensure!(same(&op.read(&k).await?, &data), "read back differs");
        pass("writes carrying x-amz-checksum-crc32c are accepted and read back intact")
    }
    .await;
    let (status, detail) = match res {
        Ok(Outcome::Pass(d)) => (CompatStatus::Pass, d),
        Ok(Outcome::Skip(d)) => (CompatStatus::Skipped, d),
        Err(e) => (CompatStatus::Fail, format!("{e:#}")),
    };
    CompatCheck {
        id: "checksum_crc32c".into(),
        category: "integrity".into(),
        status,
        detail,
        duration_ms: t.elapsed().as_secs_f64() * 1000.0,
    }
}

impl CompatReport {
    /// Add a check computed outside [`run_suite`] and recount.
    pub fn push(&mut self, c: CompatCheck) {
        self.checks.push(c);
        let n = |st: CompatStatus| self.checks.iter().filter(|c| c.status == st).count() as u64;
        self.passed_count = n(CompatStatus::Pass);
        self.failed_count = n(CompatStatus::Fail);
        self.skipped_count = n(CompatStatus::Skipped);
        let ran = self.passed_count + self.failed_count;
        self.score_pct = if ran == 0 {
            0.0
        } else {
            self.passed_count as f64 * 100.0 / ran as f64
        };
    }
}
