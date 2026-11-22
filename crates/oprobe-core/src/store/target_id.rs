//! Target identity (design v4 §2): one normalised name for "the same place in storage".
//!
//! [`TargetId::of`] maps a target to `{ scheme, authority, root }`:
//! - the scheme and host are lowercased; the scheme is the OpenDAL service (`s3a://` → `s3`);
//! - default ports are dropped (`:443` for https, `:80` for http);
//! - `.` and `..` are resolved; `..` above the root is an error;
//! - `//` collapses; the trailing `/`, the query and the fragment are stripped;
//! - local paths map to `fs` with an absolute path;
//! - the bucket or container is folded into `authority` (`bucket`, or `bucket@host:port` when
//!   the target names an endpoint), lowercased;
//! - `lab://SCENARIO` targets are `lab` + the scenario (their in-process endpoint is ephemeral).
//!
//! [`TargetId::contains`] is a prefix test on whole path segments. Users: MCP `--allow-target`,
//! baselines, compare's "same target" rule, cleanup's prefix checks and the App's compare pairing.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::TargetSpec;
use crate::errors::{ErrorCode, OprobeError};

/// The normalised identity of a storage location.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct TargetId {
    /// OpenDAL service (`s3`, `gcs`, `azblob`, `fs`, `memory`, …) or `lab`.
    pub scheme: String,
    /// `bucket`, `bucket@host[:port]`, `host[:port]` or empty (`fs`, `memory`).
    pub authority: String,
    /// Absolute, normalised path: `/` or `/a/b` (no trailing `/`).
    pub root: String,
}

fn refuse(msg: impl Into<String>) -> OprobeError {
    OprobeError::new(ErrorCode::UsageInvalidArgument, msg)
}

/// Resolve `.`, `..` and empty segments of `path` (either separator), as an absolute path.
/// `..` above the root is an error.
pub fn normalize_path(path: &str) -> Result<String, OprobeError> {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    return Err(refuse(format!("path `{path}` goes above its root (`..`)"))
                        .with_hint("remove the `..` segments"));
                }
            }
            s => out.push(s),
        }
    }
    Ok(format!("/{}", out.join("/")))
}

/// `host[:port]` of an endpoint URL, lowercased, default port dropped.
fn endpoint_authority(endpoint: &str) -> Option<String> {
    let u = url::Url::parse(endpoint.trim()).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    // `Url::port` is already `None` for the scheme's default port.
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    })
}

/// A local path as an absolute `/`-separated path (Windows drives keep their letter, lowercased).
fn absolute_local(path: &str) -> Result<String, OprobeError> {
    let p = path.replace('\\', "/");
    let b = p.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    if drive {
        let rest = normalize_path(&p[2..])?;
        return Ok(format!("/{}:{rest}", (b[0] as char).to_ascii_lowercase()));
    }
    if p.starts_with('/') {
        return normalize_path(&p);
    }
    let cwd = std::env::current_dir()
        .map(|d| d.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    normalize_path(&format!("{cwd}/{p}"))
}

impl TargetId {
    /// The identity of `t` (a target as built from flags, a config entry or a profile; its
    /// options win over what its URL implies). `..` above the root is an error.
    pub fn of(t: &TargetSpec) -> Result<TargetId, OprobeError> {
        if let Some(id) = t.url.as_deref().and_then(lab_id) {
            return id;
        }
        let (scheme, mut options) = match (&t.url, t.scheme.is_empty()) {
            (Some(url), _) => {
                check_raw_url(url)?;
                let base = TargetSpec::from_url(url).map_err(|e| {
                    let b = crate::errors::classify(&e, "s3");
                    let mut o = OprobeError::new(b.code, b.message);
                    o.hint = b.hint;
                    o
                })?;
                let scheme = if t.scheme.is_empty() {
                    base.scheme
                } else {
                    t.scheme.clone()
                };
                (scheme, base.options)
            }
            (None, false) => (t.scheme.clone(), Default::default()),
            (None, true) => return Err(refuse("target needs either `url` or `scheme`")),
        };
        for (k, v) in &t.options {
            options.insert(k.clone(), v.clone());
        }
        let scheme = scheme.to_ascii_lowercase();
        let get = |k: &str| {
            options
                .get(k)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let root_opt = get("root").unwrap_or_default();
        let root = match scheme.as_str() {
            "fs" => absolute_local(if root_opt.is_empty() { "/" } else { &root_opt })?,
            _ => normalize_path(&root_opt)?,
        };
        let name = ["bucket", "container", "filesystem", "share_name"]
            .iter()
            .find_map(|k| get(k))
            .map(|n| n.to_ascii_lowercase());
        let host = get("endpoint").and_then(|e| endpoint_authority(&e));
        let authority = match (name, host) {
            (Some(n), Some(h)) => format!("{n}@{h}"),
            (Some(n), None) => n,
            (None, Some(h)) => h,
            (None, None) => String::new(),
        };
        Ok(TargetId {
            scheme,
            authority,
            root,
        })
    }

    /// Parse a target URL (`s3://bucket/prefix`, `lab://demo`, `/local/path`, `C:\data`) into
    /// its identity, without resolving credentials or starting anything.
    pub fn parse(raw: &str) -> Result<TargetId, OprobeError> {
        let s = raw.trim();
        if let Some(id) = lab_id(s) {
            return id;
        }
        if !s.contains("://") {
            // A local path.
            return Ok(TargetId {
                scheme: "fs".into(),
                authority: String::new(),
                root: absolute_local(s)?,
            });
        }
        Self::of(&TargetSpec::from_url(s).map_err(|e| {
            let b = crate::errors::classify(&e, "s3");
            let mut o = OprobeError::new(b.code, b.message);
            o.hint = b.hint;
            o
        })?)
    }

    /// Whether `other` is this location or below it: same scheme and authority, and this root's
    /// segments are a prefix of the other's (`/data` contains `/data/x`, not `/database`).
    pub fn contains(&self, other: &TargetId) -> bool {
        if self.scheme != other.scheme || self.authority != other.authority {
            return false;
        }
        let segs = |r: &str| -> Vec<String> {
            r.split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let (a, b) = (segs(&self.root), segs(&other.root));
        a.len() <= b.len() && a.iter().zip(&b).all(|(x, y)| x == y)
    }

    /// This location with `prefix` appended to the root (an `existing` prefix, a run prefix).
    /// `..` that leaves this root is an error.
    pub fn join(&self, prefix: &str) -> Result<TargetId, OprobeError> {
        let joined = normalize_path(&format!("{}/{}", self.root, prefix))?;
        let id = TargetId {
            root: joined,
            ..self.clone()
        };
        if !self.contains(&id) {
            return Err(refuse(format!(
                "prefix `{prefix}` leaves the target root `{}`",
                self.root
            ))
            .with_hint("remove the `..` segments"));
        }
        Ok(id)
    }
}

/// `..` above the root of a URL's path is refused before URL parsing could clamp it away.
fn check_raw_url(raw: &str) -> Result<(), OprobeError> {
    let Some((_, rest)) = raw.trim().split_once("://") else {
        return Ok(());
    };
    let path = rest
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .split_once(['/', '\\'])
        .map(|(_, p)| p)
        .unwrap_or_default();
    normalize_path(path).map(|_| ())
}

/// `lab://SCENARIO[/root]`: identified by scenario, never by the ephemeral endpoint.
fn lab_id(raw: &str) -> Option<Result<TargetId, OprobeError>> {
    let lower = raw.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("lab://")?;
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (scenario, path) = rest.split_once('/').unwrap_or((rest, ""));
    Some((|| {
        let sc = crate::input::lab_scenario(scenario)?;
        Ok(TargetId {
            scheme: "lab".into(),
            authority: sc.name().to_owned(),
            root: normalize_path(path)?,
        })
    })())
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}{}", self.scheme, self.authority, self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(u: &str) -> TargetId {
        TargetId::parse(u).unwrap_or_else(|e| panic!("{u}: {e}"))
    }

    #[test]
    fn normalises_scheme_case_bucket_and_slashes() {
        let want = id("s3://bench/data");
        assert_eq!(want.to_string(), "s3://bench/data");
        for u in [
            "S3://bench/data",
            "s3://BENCH/data",
            "s3://bench/data/",
            "s3://bench//data",
            "s3://bench/./data",
            "s3://bench/x/../data",
            "s3://bench/data?region=eu-west-1",
            "s3://bench/data#frag",
            "s3a://bench/data",
        ] {
            assert_eq!(id(u), want, "{u}");
        }
        // Path case is significant.
        assert_ne!(id("s3://bench/Data"), want);
        assert_eq!(id("s3://bench").root, "/");
        assert_eq!(id("s3://bench/").to_string(), "s3://bench/");
        assert_eq!(id("gs://b/x").scheme, "gcs");
        assert_eq!(id("azblob://Cont/p").authority, "cont");
    }

    #[test]
    fn dot_dot_above_the_root_is_an_error() {
        for u in ["s3://bench/../prod", "s3://bench/a/../../b", "fs:///../x"] {
            let e = TargetId::parse(u).unwrap_err();
            assert_eq!(e.code, ErrorCode::UsageInvalidArgument, "{u}");
            assert!(e.message.contains(".."), "{u}: {}", e.message);
        }
        assert!(normalize_path("/a/../..").is_err());
        assert_eq!(normalize_path("a/./b//c/").unwrap(), "/a/b/c");
    }

    #[test]
    fn endpoints_fold_into_the_authority_without_default_ports() {
        let mut t = TargetSpec::from_url("s3://Bench/p").unwrap();
        t.options
            .insert("endpoint".into(), "HTTPS://MinIO.Local:443/".into());
        let a = TargetId::of(&t).unwrap();
        assert_eq!(a.authority, "bench@minio.local");
        t.options
            .insert("endpoint".into(), "https://minio.local".into());
        assert_eq!(TargetId::of(&t).unwrap(), a);
        t.options
            .insert("endpoint".into(), "http://minio.local:80".into());
        assert_eq!(TargetId::of(&t).unwrap().authority, "bench@minio.local");
        t.options
            .insert("endpoint".into(), "http://127.0.0.1:9000".into());
        assert_eq!(TargetId::of(&t).unwrap().authority, "bench@127.0.0.1:9000");
        // Without an endpoint the bucket alone names the place.
        assert_eq!(id("s3://bench").authority, "bench");
        // Options win over the URL.
        let mut o = TargetSpec::from_url("s3://bench/p").unwrap();
        o.options.insert("root".into(), "/other".into());
        assert_eq!(TargetId::of(&o).unwrap().root, "/other");
        o.options.insert("bucket".into(), "Prod".into());
        assert_eq!(TargetId::of(&o).unwrap().authority, "prod");
    }

    #[test]
    fn local_paths_map_to_absolute_fs_paths() {
        assert_eq!(id("fs:///tmp/x/").to_string(), "fs:///tmp/x");
        assert_eq!(id("fs:///tmp/a/../x"), id("/tmp/x"));
        assert_eq!(id("file:///tmp/x"), id("/tmp//x/"));
        let rel = id("fs://relative/dir");
        assert!(
            rel.root.ends_with("/relative/dir") && rel.root.len() > 13,
            "{rel}"
        );
        assert_eq!(id("relative/dir"), rel);
        assert_eq!(id(r"C:\Data\x\"), id("C:/Data/x"));
        assert!(id(r"C:\Data").root.starts_with("/c:"), "{}", id(r"C:\Data"));
        assert_eq!(id("memory://").to_string(), "memory:///");
    }

    #[test]
    fn lab_targets_are_identified_by_scenario() {
        let a = id("lab://demo");
        assert_eq!(a.to_string(), "lab://demo/");
        assert_eq!(id("LAB://Demo/"), a);
        assert_ne!(id("lab://throttle"), a);
        assert!(TargetId::parse("lab://nope").is_err());
        // The profile of a lab target resolves to the same identity.
        let t = TargetSpec::from_url("lab://demo").unwrap();
        assert_eq!(TargetId::of(&t).unwrap(), a);
    }

    #[test]
    fn contains_is_a_prefix_on_whole_segments() {
        let allowed = id("fs:///srv/allowed/");
        assert!(allowed.contains(&id("fs:///srv/allowed")));
        assert!(allowed.contains(&id("fs:///srv/allowed/x/y")));
        assert!(!allowed.contains(&id("fs:///srv/allowed-evil")));
        assert!(!allowed.contains(&id("fs:///srv")));
        // The `..` bypass of 3.0 (SEC-1): normalised before the prefix test.
        assert!(!allowed.contains(&id("fs:///srv/allowed/../other")));
        assert!(!allowed.contains(&id("fs:///srv//allowed/./../other")));
        let b = id("s3://B/oprobe");
        assert!(b.contains(&id("s3://b/oprobe/run")));
        assert!(!b.contains(&id("s3://b/oprobe/../prod")));
        assert!(!b.contains(&id("s3://b2/oprobe")));
        assert!(!b.contains(&id("gs://b/oprobe")));
        assert!(id("s3://b").contains(&id("s3://b/anything")));
        // `existing` prefixes are joined, never escaping the root.
        assert!(b.join("x/y").is_ok());
        assert!(b.join("../../prod/").is_err());
        assert!(b.join("../oprobe/x").is_ok());
        assert!(b.join("x/../../prod").is_err());
    }
}
