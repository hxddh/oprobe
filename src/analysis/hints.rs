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

/// Map an error chain to a human hint. `scheme` is the storage scheme (s3, gcs, ...).
pub fn hint_for(error_chain: &str, scheme: &str) -> Option<Hint> {
    let e = error_chain.to_ascii_lowercase();
    let has = |n: &str| e.contains(n);
    let creds = match scheme {
        "gcs" => "Set GOOGLE_APPLICATION_CREDENTIALS or `credential_path` in [target.options].",
        "azblob" => "Set `account_name` and `account_key` (or a SAS token) in [target.options].",
        "oss" => {
            "Set `access_key_id` / `access_key_secret` in [target.options] (use ${ENV} references)."
        }
        _ => {
            "Export AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY (or AWS_PROFILE), or reference them as ${ENV} in [target.options]."
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
                "Set `region` in [target.options] or export AWS_REGION to the bucket's region.",
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
    if has("signaturedoesnotmatch")
        || has("invalidaccesskeyid")
        || has("invalid access key")
        || has("accessdenied")
        || has("permission denied")
        || has("permissiondenied")
        || has("403")
        || has("401")
    {
        return Some(h(
            "Request rejected by the storage service (auth / permission)",
            &[
                creds,
                "The credentials need list, read, write and delete permission on the target prefix (oprobe writes under `oprobe-run-*/`).",
                "For read-only access to existing data use `--existing <prefix>`; for public buckets use `--anonymous`.",
                "Run `oprobe doctor` to test each permission individually.",
            ],
        ));
    }
    if has("nosuchbucket") || (has("404") && has("bucket")) {
        return Some(h(
            "Bucket does not exist",
            &["Check the bucket name and region/endpoint in the target URL."],
        ));
    }
    if has("slowdown")
        || has("429")
        || has("503")
        || has("too many requests")
        || has("ratelimited")
        || has("rate_limited")
    {
        return Some(h(
            "The service is throttling requests",
            &[
                "Lower `--concurrency` or set `--max-inflight`.",
                "`oprobe tune` finds the highest safe concurrency automatically and backs off on 429/503.",
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
    if has("connection refused") {
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
}
