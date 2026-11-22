//! Turn raw failures into actionable guidance.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Hint {
    pub title: String,
    pub steps: Vec<String>,
}

fn h(title: &str, steps: &[&str]) -> Hint {
    Hint {
        title: title.to_owned(),
        steps: steps.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// Whether `e` (lower-cased) mentions HTTP status `code` as a status, not as part of an object
/// name, date, port or request id: `status: 403`, `status code 403`, `(403`, `403 forbidden`,
/// `http 403`, or the code as a standalone word (` 403 `, `403,`).
fn has_status(e: &str, code: u16) -> bool {
    let c = code.to_string();
    let bytes = e.as_bytes();
    let mut from = 0;
    while let Some(off) = e[from..].find(&c) {
        let i = from + off;
        let j = i + c.len();
        from = j;
        let before = if i == 0 { None } else { Some(bytes[i - 1]) };
        let after = bytes.get(j).copied();
        // Part of a longer number or identifier (obj-00403.bin, :4030, 2026-04-03).
        let glued = |b: Option<u8>| {
            b.is_some_and(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'/')
            })
        };
        let prefix = &e[..i];
        let labelled = [
            "status: ",
            "status:",
            "status code ",
            "status code: ",
            "status=",
            "http ",
            "code: ",
        ]
        .iter()
        .any(|l| prefix.ends_with(l));
        if labelled && !glued(after) {
            return true;
        }
        let open = matches!(before, None | Some(b' ' | b'(' | b'[' | b'"' | b'\''));
        let close = matches!(
            after,
            None | Some(b' ' | b')' | b']' | b',' | b';' | b'"' | b'\'')
        );
        if open && close && !glued(before) {
            return true;
        }
    }
    false
}

/// Map an error chain to a human hint. `scheme` is the storage scheme (s3, gcs, ...).
pub fn hint_for(error_chain: &str, scheme: &str) -> Option<Hint> {
    let e = error_chain.to_ascii_lowercase();
    let has = |n: &str| e.contains(n);
    let creds = match scheme {
        "gcs" => {
            "Set GOOGLE_APPLICATION_CREDENTIALS or `credential_path` in the target's options (`--option KEY=VALUE`, or `[targets.NAME.options]` in the config)."
        }
        "azblob" => {
            "Set `account_name` and `account_key` (or a SAS token) in the target's options (`--option KEY=VALUE`, or `[targets.NAME.options]` in the config)."
        }
        "oss" => {
            "Set `access_key_id` / `access_key_secret` in the target's options (`--option KEY=VALUE`, or `[targets.NAME.options]` in the config) (use ${ENV} references)."
        }
        _ => {
            "Export AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY (or AWS_PROFILE), or reference them as ${ENV} in the target's options (`--option KEY=VALUE`, or `[targets.NAME.options]` in the config)."
        }
    };

    if has("environment variable") && has("not set") {
        return Some(h(
            "A profile references an unset environment variable",
            &["Export the variable named above, or replace the ${NAME} reference in the profile."],
        ));
    }
    if has("invalid profile") || has("unknown field") || has("toml parse") {
        return Some(h(
            "The profile is not valid",
            &["Fix the field named above; `oprobe init` prints a fully commented profile."],
        ));
    }
    if has("region is missing")
        || has("authorizationheadermalformed")
        || has("permanentredirect")
        || has("301 moved")
        || has("wrong region")
    {
        return Some(h(
            "Wrong or missing region",
            &[
                "Set `region` in the target's options (`--option KEY=VALUE`, or `[targets.NAME.options]` in the config) or export AWS_REGION to the bucket's region.",
                "S3-compatible stores (MinIO, R2, Ceph) also need `endpoint`; R2 uses region `auto`.",
            ],
        ));
    }
    if has("requesttimetooskewed") || has("clock skew") {
        return Some(h(
            "Local clock is out of sync",
            &[
                "Signed requests are rejected when the clock is >15 min off: enable NTP (`timedatectl set-ntp true`) and retry.",
                "`oprobe probe <endpoint>` reports the measured skew.",
            ],
        ));
    }
    if has("failed to load signing credential")
        || has("no valid credential")
        || has("no credential")
        || has("credential not found")
        || has("credentials not found")
        || has("could not load credentials")
        || has("missing credentials")
        || has("unable to load credentials")
    {
        return Some(h(
            "No credentials found for the storage service",
            &[
                creds,
                "Using a named profile? Export AWS_PROFILE (or set `credential_path` / `config_path` for other services).",
                "Public bucket? Pass `--anonymous` to send unsigned requests.",
            ],
        ));
    }
    if has("signaturedoesnotmatch")
        || has("invalidaccesskeyid")
        || has("invalid access key")
        || has("accessdenied")
        || has("permission denied")
        || has("permissiondenied")
        || has_status(&e, 403)
        || has_status(&e, 401)
        || has("forbidden")
        || has("unauthorized")
    {
        return Some(h(
            "Request rejected by the storage service (auth / permission)",
            &[
                creds,
                "The credentials need list, read, write and delete permission on the target prefix (oprobe writes under `oprobe-run-*/`).",
                "For read-only access to existing data use `--existing <prefix>`.",
            ],
        ));
    }
    if has("nosuchbucket") || (has_status(&e, 404) && has("bucket")) {
        return Some(h(
            "Bucket does not exist",
            &["Check the bucket name and region/endpoint in the target URL."],
        ));
    }
    if has("slowdown")
        || has_status(&e, 429)
        || has_status(&e, 503)
        || has("too many requests")
        || has("ratelimited")
        || has("rate_limited")
    {
        return Some(h(
            "The service is throttling requests",
            &[
                "Cap the requests in flight (`--set layers.concurrent_limit.http=N`) and retry with jitter (`--set layers.retry.jitter=on`).",
                "`oprobe prove RUN throttling` sizes the cap with short trials and A/B-tests it.",
            ],
        ));
    }
    if has("certificate")
        || has("unknownissuer")
        || has("invalid peer certificate")
        || has("notvalidforname")
        || has("tls handshake")
    {
        return Some(h(
            "TLS certificate verification failed",
            &[
                "Corporate TLS-inspecting proxy or private CA? Pass `--ca-cert <bundle.pem>` (or set SSL_CERT_FILE).",
                "Self-signed development endpoint: `--insecure` skips verification (never in production).",
                "Hostname mismatch: make sure `endpoint` uses the certificate's DNS name, not an IP.",
            ],
        ));
    }
    if has("proxy refused") || has("proxy connect") || has("proxy closed") {
        return Some(h(
            "The egress proxy refused the tunnel",
            &[
                "Check HTTPS_PROXY / HTTP_PROXY and proxy credentials, or add the endpoint to NO_PROXY.",
                "Use `--proxy none` to bypass environment proxies.",
            ],
        ));
    }
    if has("dns") && (has("failed") || has("no addresses") || has("lookup"))
        || has("failed to lookup address")
        || has("name or service not known")
    {
        return Some(h(
            "DNS resolution failed",
            &[
                "Check the endpoint hostname and network/DNS settings; behind a proxy, set HTTPS_PROXY so the proxy resolves names.",
            ],
        ));
    }
    if has("connection refused") || has("actively refused") || has("os error 10061") {
        return Some(h(
            "Connection refused",
            &[
                "Nothing is listening at the endpoint: check host/port, and that the service (e.g. MinIO, `oprobe lab`) is running.",
            ],
        ));
    }
    if has("timed out") || has("timeout") {
        return Some(h(
            "Network timeout",
            &[
                "Check reachability (`oprobe probe <endpoint>`), firewalls and proxies; raise `knobs.timeout` for very large objects.",
            ],
        ));
    }
    if has("unsupported storage scheme") || has("invalid storage url") {
        return Some(h(
            "Unrecognised target",
            &[
                "Use a URL like s3://bucket/prefix, gcs://bucket, azblob://container, fs:///path or memory://.",
            ],
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_connection_refused_is_recognised() {
        let msg = "cannot reach http://127.0.0.1:9 (s3://b): No connection could be made because \
                   the target machine actively refused it. (os error 10061)";
        assert!(hint_for(msg, "s3").unwrap().title.contains("refused"));
    }

    #[test]
    fn maps_common_failures() {
        assert!(
            hint_for("PermissionDenied: AccessDenied", "s3")
                .unwrap()
                .title
                .contains("auth")
        );
        assert!(
            hint_for(
                "tls handshake failed: invalid peer certificate: UnknownIssuer",
                "s3"
            )
            .unwrap()
            .title
            .contains("TLS")
        );
        assert!(
            hint_for("SlowDown 503", "s3")
                .unwrap()
                .title
                .contains("throttling")
        );
        assert!(
            hint_for("RequestTimeTooSkewed", "s3")
                .unwrap()
                .title
                .contains("clock")
        );
        assert!(hint_for("all good", "s3").is_none());
    }

    #[test]
    fn status_codes_match_as_tokens_only() {
        // Object names, ports and dates containing the digits are not statuses.
        for e in [
            "Unexpected at read => path: oprobe-run-x/data/obj-00429.bin",
            "connect to minio:4030 failed",
            "object obj-00403.bin written at 2026-04-03",
            "request id 1503a9f2",
        ] {
            let t = hint_for(e, "s3").map(|h| h.title);
            assert!(
                t.as_deref()
                    .is_none_or(|t| !t.contains("auth") && !t.contains("throttling")),
                "{e}: {t:?}"
            );
        }
        for e in [
            "Unexpected => status: 403, body: <Error>",
            "PermissionDenied (persistent) at stat, context: { status: 403 }",
            "http 401 Unauthorized",
            "request failed (403 Forbidden)",
        ] {
            assert!(hint_for(e, "s3").unwrap().title.contains("auth"), "{e}");
        }
        assert!(
            hint_for("status: 503 Service Unavailable", "s3")
                .unwrap()
                .title
                .contains("throttling")
        );
        assert!(
            hint_for("got 429 too many", "s3")
                .unwrap()
                .title
                .contains("throttling")
        );
    }

    #[test]
    fn missing_credentials_get_their_own_hint() {
        let h = hint_for(
            "PermissionDenied at write => failed to load signing credential",
            "s3",
        )
        .unwrap();
        assert!(h.title.contains("No credentials"), "{}", h.title);
        assert!(h.steps.iter().any(|s| s.contains("--anonymous")));
        assert!(h.steps.iter().any(|s| s.contains("AWS_PROFILE")));
        assert!(
            hint_for("no valid credential found", "gcs")
                .unwrap()
                .title
                .contains("No credentials")
        );
    }
}
