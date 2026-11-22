//! Classified errors (design §8): a stable dotted [`ErrorCode`], an [`ErrorBody`] for machine
//! output, and the exit-code mapping every command uses (defined in `oprobe_trace::error`,
//! re-exported here).
//!
//! Raise a classified error with [`OprobeError`] (or the [`fail!`](crate::fail) macro) where the
//! error happens; user input is parsed by [`crate::input`], whose errors are typed. Any other
//! `anyhow::Error` is classified at the CLI boundary by [`classify`] from typed sources only
//! (OpenDAL error kinds, dial/TLS/timeout errors, I/O kinds); there is no message matching, so
//! an untyped error is `internal` — a bug to fix where it is raised.

pub use oprobe_trace::error::{ErrorBody, ErrorCode, OprobeError, exit};

/// `return Err(fail!(ErrorCode::UsageUnknownRun, "no run `{}`", r))`: a classified
/// `anyhow::Error`.
#[macro_export]
macro_rules! fail {
    ($code:expr, $($arg:tt)*) => {
        ::anyhow::Error::new($crate::errors::OprobeError::new($code, format!($($arg)*)))
    };
}

/// Attach a classification to any fallible result.
pub trait Classify<T> {
    fn code(self, code: ErrorCode) -> anyhow::Result<T>;
}

impl<T, E: Into<anyhow::Error>> Classify<T> for Result<T, E> {
    fn code(self, code: ErrorCode) -> anyhow::Result<T> {
        self.map_err(|e| {
            let e: anyhow::Error = e.into();
            if e.downcast_ref::<OprobeError>().is_some() {
                return e;
            }
            let msg = format!("{e:#}");
            anyhow::Error::new(OprobeError::new(code, msg))
        })
    }
}

/// Classify an error chain: an [`OprobeError`] anywhere in the chain wins, then typed sources
/// (OpenDAL error kinds, dial/TLS/timeout errors), else `internal`. `scheme` selects
/// provider-specific hints.
pub fn classify(err: &anyhow::Error, scheme: &str) -> ErrorBody {
    let chain = format!("{err:#}");
    let hint_text = || {
        crate::analysis::hints::hint_for(&chain, scheme).map(|h| {
            if h.steps.is_empty() {
                h.title
            } else {
                format!("{}: {}", h.title, h.steps.join("; "))
            }
        })
    };
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<OprobeError>() {
            let mut body = ErrorBody::new(e.code, chain_message(err));
            body.hint = e
                .hint
                .as_deref()
                .map(crate::util::strip_url_credentials)
                .or_else(hint_text);
            return body;
        }
    }
    let code = typed_code(err).unwrap_or(ErrorCode::Internal);
    let mut body = ErrorBody::new(code, chain_message(err));
    body.hint = hint_text();
    body
}

/// The whole chain on one line (`outer: inner: root`), with URL credentials (`user:pass@`)
/// removed: OpenDAL contexts quote endpoints as configured.
fn chain_message(err: &anyhow::Error) -> String {
    crate::util::strip_url_credentials(&format!("{err:#}"))
}

/// The code of an OpenDAL error, from its kind (and, for `Unexpected`, the failure class
/// `engine::ops::error_class_of` derives from its status and source). Credential failures
/// (`failed to load signing credential`, `no valid credential found`: OpenDAL raises these as
/// `ConfigInvalid` or `Unexpected` depending on the service) are `auth.no_credentials`.
/// `None` when the error says nothing more specific than "unexpected".
pub fn opendal_code(oe: &opendal::Error) -> Option<ErrorCode> {
    use opendal::ErrorKind as K;
    // The message, its context (`called: reqsign::Sign`) and its source.
    let credential = oe.to_string().to_ascii_lowercase().contains("credential");
    Some(match oe.kind() {
        K::NotFound => ErrorCode::StorageNotFound,
        K::PermissionDenied => ErrorCode::AuthDenied,
        K::RateLimited => ErrorCode::StorageThrottled,
        K::Unsupported => ErrorCode::StorageUnsupported,
        K::ConfigInvalid if credential => ErrorCode::AuthNoCredentials,
        K::ConfigInvalid => ErrorCode::ConfigInvalid,
        K::Unexpected if credential => ErrorCode::AuthNoCredentials,
        _ => match ErrorCode::from_class(&crate::engine::ops::error_class_of(oe, None)) {
            ErrorCode::Internal => return None,
            c => c,
        },
    })
}

/// Typed sources in the chain: OpenDAL kinds, dial stages, reqwest timeouts/connect errors, I/O
/// kinds, TOML parse errors. An OpenDAL error that says only "unexpected" lets a more specific
/// cause further down the chain decide.
fn typed_code(err: &anyhow::Error) -> Option<ErrorCode> {
    for cause in err.chain() {
        if let Some(oe) = cause.downcast_ref::<opendal::Error>()
            && let Some(c) = opendal_code(oe)
        {
            return Some(c);
        }
        if let Some(d) = cause.downcast_ref::<crate::net::dial::DialError>() {
            return Some(match d.stage {
                "dns" => ErrorCode::NetDns,
                "tls" => ErrorCode::NetTls,
                _ => ErrorCode::NetUnreachable,
            });
        }
        if let Some(r) = cause.downcast_ref::<reqwest::Error>() {
            if r.is_timeout() {
                return Some(ErrorCode::NetTimeout);
            }
            if r.is_connect() {
                return Some(ErrorCode::NetUnreachable);
            }
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::TimedOut => return Some(ErrorCode::NetTimeout),
                std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::HostUnreachable
                | std::io::ErrorKind::NetworkUnreachable => return Some(ErrorCode::NetUnreachable),
                std::io::ErrorKind::PermissionDenied => return Some(ErrorCode::AuthDenied),
                _ => {}
            }
        }
        if cause.downcast_ref::<toml::de::Error>().is_some() {
            return Some(ErrorCode::ConfigInvalid);
        }
        if cause
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
        {
            return Some(ErrorCode::NetTimeout);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_in_urls_never_reach_error_messages() {
        let e = anyhow::anyhow!("connect to http://admin:s3cret@minio.local:9000/bucket failed")
            .context("cannot open the target");
        let b = classify(&e, "s3");
        assert!(!b.message.contains("s3cret"), "{}", b.message);
        assert!(
            b.message.contains("http://minio.local:9000/bucket"),
            "{}",
            b.message
        );
        let e: anyhow::Error = OprobeError::new(ErrorCode::NetUnreachable, "x")
            .with_hint("check https://u:p@proxy.example.com")
            .into();
        assert!(!classify(&e, "s3").hint.unwrap().contains("u:p@"));
    }

    #[test]
    fn every_code_has_its_docs_anchor() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
                .join("docs/troubleshooting.md"),
        )
        .unwrap();
        for c in ErrorCode::ALL {
            let url = c.docs();
            let (page, anchor) = url.rsplit_once('#').unwrap();
            assert!(page.ends_with("docs/troubleshooting.md"), "{url}");
            assert!(
                doc.contains(&format!("<a id=\"{anchor}\"></a>")),
                "docs/troubleshooting.md has no anchor `{anchor}` for {c}"
            );
            assert!(
                doc.contains(&format!("### `{c}` (exit {}", c.exit_code())),
                "docs/troubleshooting.md: no section for {c} with exit {}",
                c.exit_code()
            );
        }
    }

    #[test]
    fn codes_round_trip_and_map_to_exit_codes() {
        for c in ErrorCode::ALL {
            assert_eq!(ErrorCode::parse(c.as_str()), Some(*c));
            let json = serde_json::to_string(c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.as_str()));
            let prefix = c.as_str().split('.').next().unwrap();
            let want = match prefix {
                "usage" | "config" => 2,
                "auth" | "net" | "storage" | "internal" => 1,
                "safety" if *c == ErrorCode::SafetyBudgetExceeded => 4,
                "safety" => 5,
                "run" if *c == ErrorCode::RunInterrupted => 130,
                "run" => 3,
                _ => unreachable!(),
            };
            assert_eq!(c.exit_code(), want, "{c}");
        }
        assert_eq!(ErrorCode::ALL.len(), 24);
    }

    #[test]
    fn classified_errors_survive_context() {
        let e = anyhow::Error::new(
            OprobeError::new(ErrorCode::UsageUnknownRun, "no run `x`")
                .with_hint("oprobe runs list"),
        )
        .context("cannot show the run");
        let b = classify(&e, "s3");
        assert_eq!(b.code, ErrorCode::UsageUnknownRun);
        assert_eq!(b.hint.as_deref(), Some("oprobe runs list"));
        assert!(b.message.contains("no run `x`"));
        assert_eq!(b.exit_code(), 2);
    }

    #[test]
    fn opendal_kinds_are_classified() {
        let e = anyhow::Error::new(opendal::Error::new(
            opendal::ErrorKind::PermissionDenied,
            "AccessDenied",
        ));
        assert_eq!(classify(&e, "s3").code, ErrorCode::AuthDenied);
        let e = anyhow::Error::new(opendal::Error::new(opendal::ErrorKind::NotFound, "nope"));
        assert_eq!(classify(&e, "s3").code, ErrorCode::StorageNotFound);
        // Messages are never matched: only typed errors carry a code.
        let e = anyhow::anyhow!("no target given: connection refused, credential");
        assert_eq!(classify(&e, "s3").code, ErrorCode::Internal);
        let e = anyhow::Error::new(opendal::Error::new(
            opendal::ErrorKind::Unexpected,
            "failed to load signing credential",
        ))
        .context("preflight");
        assert_eq!(classify(&e, "gcs").code, ErrorCode::AuthNoCredentials);
        let e = anyhow::Error::new(opendal::Error::new(
            opendal::ErrorKind::ConfigInvalid,
            "no valid credential found",
        ));
        assert_eq!(classify(&e, "s3").code, ErrorCode::AuthNoCredentials);
        let e = anyhow::anyhow!("something odd");
        assert_eq!(classify(&e, "s3").code, ErrorCode::Internal);
    }

    #[test]
    fn api_statuses_are_network_codes() {
        assert_eq!(
            ErrorCode::from_api_status(403, true),
            ErrorCode::NetRateLimited
        );
        assert_eq!(
            ErrorCode::from_api_status(429, false),
            ErrorCode::NetRateLimited
        );
        assert_eq!(ErrorCode::from_api_status(403, false), ErrorCode::NetHttp);
        assert_eq!(ErrorCode::from_api_status(404, false), ErrorCode::NetHttp);
        assert!(ErrorCode::NetRateLimited.retryable());
        assert_eq!(ErrorCode::NetHttp.exit_code(), 1);
    }

    #[test]
    fn error_body_json_shape() {
        let b = ErrorBody::new(ErrorCode::SafetyReadOnly, "refused");
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["code"], "safety.read_only");
        assert_eq!(v["retryable"], false);
        assert!(v.get("hint").is_none());
    }
}
