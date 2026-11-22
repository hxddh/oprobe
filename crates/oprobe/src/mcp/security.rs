//! The MCP server's security rails (design v5 §5, S1–S3):
//!
//! - **Target overrides** ([`override_refusal`]): a tool call may not move a configured or
//!   credentialed target elsewhere (`endpoint`, `proxy`, `insecure`, `options`, URL query
//!   options, `set endpoint=…`): the credentials it resolves to would be sent to that host.
//!   Overrides are accepted only for unsigned (`anonymous`) requests and the in-process
//!   `memory://` / `lab://` targets, and never towards a link-local address (cloud metadata).
//!   With `--allow-target`, `tools::check_target` is stricter still.
//! - **Endpoint addresses** ([`check_endpoint_address`]): `probe_endpoint` refuses link-local,
//!   loopback (except `lab://`), private and reserved addresses unless the server allows them.
//! - **Untrusted text** ([`untrusted`], [`sanitize`]): object keys, error bodies and server
//!   messages are escaped and truncated, and kept out of the text content.

use serde_json::Value;

use crate::config::Profile;
use crate::errors::{ErrorCode, OprobeError};
use crate::guard::{AddressClass, address_class, endpoint_host, resolve_classes};

/// Longest untrusted string kept in a response (characters).
pub const MAX_UNTRUSTED_CHARS: usize = 512;

/// Whether `t` sends unsigned requests: every option `--anonymous` sets is present with its
/// value (a call cannot pass `anonymous: true` and switch signing back on with `options`).
pub fn is_anonymous(t: &crate::config::TargetSpec) -> bool {
    match crate::config::anonymous_options(&t.scheme) {
        Ok(opts) => opts
            .iter()
            .all(|(k, v)| t.options.get(*k).is_some_and(|x| x == v)),
        Err(_) => false,
    }
}

/// The refusal of a call whose overrides (`diff`: the names of what differs between the
/// target as configured and as the call would use it) would retarget its credentials, if any.
pub fn override_refusal(given: &str, effective: &Profile, diff: &[String]) -> Option<OprobeError> {
    if diff.is_empty() {
        return None;
    }
    if crate::guard::in_process(&effective.target) || is_anonymous(&effective.target) {
        return None;
    }
    Some(
        OprobeError::new(
            ErrorCode::SafetyRefused,
            format!(
                "this call overrides where target `{}` points ({}): its credentials would be sent there, so the MCP server refuses it",
                untrusted(given, 200),
                diff.join(", ")
            ),
        )
        .with_hint(
            "save the target with its endpoint first (save_target, or `oprobe target add` by a person), \
             pass anonymous=true for a public bucket, or use lab://demo or memory://",
        ),
    )
}

/// Endpoint overrides that are accepted (anonymous requests) still may not reach a
/// link-local address: that is where cloud metadata services hand out credentials.
pub async fn check_override_endpoint(
    effective: &Profile,
    allow_private: bool,
) -> anyhow::Result<()> {
    if allow_private || crate::guard::in_process(&effective.target) {
        return Ok(());
    }
    let Some(ep) = effective.target.options.get("endpoint") else {
        return Ok(());
    };
    if ep.contains("${") {
        return Ok(());
    }
    let Some((host, port)) = host_port(ep) else {
        return Ok(());
    };
    let classes = resolve_classes(&host, port).await.unwrap_or_default();
    if let Some((ip, _)) = classes.iter().find(|(_, c)| *c == AddressClass::LinkLocal) {
        return Err(OprobeError::new(
            ErrorCode::SafetyRefused,
            format!(
                "endpoint `{}` resolves to the link-local address {ip}: refused on this MCP server",
                untrusted(ep, 200)
            ),
        )
        .with_hint("link-local addresses serve cloud metadata; restart the server with --allow-private-endpoints if this is intended")
        .into());
    }
    Ok(())
}

/// `host` and port of an `http(s)://` URL (default ports filled in).
pub fn host_port(url: &str) -> Option<(String, u16)> {
    let u = url::Url::parse(url.trim()).ok()?;
    let host = endpoint_host(url)?;
    let port = u.port_or_known_default()?;
    Some((host, port))
}

/// `probe_endpoint`: refuse an endpoint (or proxy) on a non-public network unless allowed.
/// `lab://` URLs probe the in-process lab on loopback and are always allowed.
pub async fn check_endpoint_address(
    endpoint: &str,
    proxy: Option<&str>,
    allowed: bool,
) -> anyhow::Result<()> {
    if allowed {
        return Ok(());
    }
    let e = endpoint.trim();
    if e.to_ascii_lowercase().starts_with("lab://") {
        return Ok(());
    }
    let mut urls: Vec<String> = Vec::new();
    if e.starts_with("http://") || e.starts_with("https://") {
        urls.push(e.to_owned());
    } else if let Ok(t) = crate::config::TargetSpec::from_url(e)
        && let Some(ep) = t.options.get("endpoint")
    {
        urls.push(ep.clone());
    }
    if let Some(p) = proxy
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "none")
    {
        urls.push(p.to_owned());
    }
    for u in urls {
        let Some((host, port)) = host_port(&u) else {
            continue;
        };
        // A literal address needs no lookup; a name is resolved (all its addresses count).
        let classes = match host.parse::<std::net::IpAddr>() {
            Ok(ip) => vec![(ip, address_class(ip))],
            Err(_) => resolve_classes(&host, port).await?,
        };
        if let Some((ip, class)) = classes.iter().find(|(_, c)| *c != AddressClass::Public) {
            return Err(OprobeError::new(
                ErrorCode::SafetyRefused,
                format!(
                    "`{}` resolves to {ip} ({}): probe_endpoint only probes public addresses on this MCP server",
                    untrusted(&u, 200),
                    class_label(*class)
                ),
            )
            .with_hint(
                "probe lab://demo for a local lab, or restart the server with --allow-private-endpoints \
                 (or an --allow-target entry for this endpoint)",
            )
            .into());
        }
    }
    Ok(())
}

fn class_label(c: AddressClass) -> &'static str {
    match c {
        AddressClass::Loopback => "loopback",
        AddressClass::LinkLocal => "link-local",
        AddressClass::Private => "a private network",
        AddressClass::Reserved => "a reserved range",
        AddressClass::Public => "public",
    }
}

/// Text from outside oprobe (object keys, error bodies, server messages) made safe to show: control
/// characters escaped, bidirectional overrides and zero-width characters removed, at most `max`
/// characters (an ellipsis marks the cut).
pub fn untrusted(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 2));
    let mut n = 0usize;
    for c in s.chars() {
        if n >= max {
            out.push('…');
            break;
        }
        match c {
            '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200b}'..='\u{200f}'
            | '\u{feff}' => {
                continue;
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:04x}}}", c as u32)),
            c => out.push(c),
        }
        n += 1;
    }
    out
}

/// Apply [`untrusted`] to every string of a structured result (keys included), with a larger
/// budget for long, oprobe-authored prose.
pub fn sanitize(v: &mut Value) {
    fn walk(v: &mut Value, depth: usize) {
        if depth > 64 {
            return;
        }
        match v {
            Value::String(s) => {
                if needs_cleaning(s) {
                    *s = untrusted(s, 4096);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| walk(x, depth + 1)),
            Value::Object(m) => {
                let dirty: Vec<String> = m.keys().filter(|k| needs_cleaning(k)).cloned().collect();
                for k in dirty {
                    if let Some(x) = m.remove(&k) {
                        m.insert(untrusted(&k, MAX_UNTRUSTED_CHARS), x);
                    }
                }
                m.values_mut().for_each(|x| walk(x, depth + 1));
            }
            _ => {}
        }
    }
    walk(v, 0)
}

fn needs_cleaning(s: &str) -> bool {
    s.chars().count() > 4096
        || s.chars().any(|c| {
            (c.is_control() && c != '\n')
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200f}' | '\u{feff}')
        })
}

/// Error codes whose message carries text from the storage service or the network (an error
/// body, a server message): it moves to `untrusted.message` and stays out of the text content.
pub fn message_is_untrusted(code: ErrorCode) -> bool {
    let c = code.as_str();
    c == "internal"
        || ["net.", "auth.", "storage."]
            .iter()
            .any(|p| c.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_text_is_escaped_and_truncated() {
        assert_eq!(untrusted("a\nb\u{1b}[31m", 100), "a\\nb\\u{001b}[31m");
        assert_eq!(untrusted("ok\u{202e}gpj.exe", 100), "okgpj.exe");
        assert_eq!(untrusted("abcdef", 3), "abc…");
        let mut v = serde_json::json!({"keys": ["x\u{0}y", "fine"], "n": 1, "k\u{7}": "v"});
        sanitize(&mut v);
        assert_eq!(v["keys"][0], "x\\u{0000}y");
        assert_eq!(v["keys"][1], "fine");
        assert!(v.get("k\\u{0007}").is_some(), "{v}");
    }

    #[test]
    fn anonymous_means_every_unsigned_option_holds() {
        let mut t = crate::config::TargetSpec::from_url("s3://b/p").unwrap();
        assert!(!is_anonymous(&t));
        for (k, v) in crate::config::anonymous_options("s3").unwrap() {
            t.options.insert((*k).into(), (*v).into());
        }
        assert!(is_anonymous(&t));
        let (k, _) = crate::config::anonymous_options("s3").unwrap()[0];
        t.options.insert(k.into(), "false".into());
        assert!(!is_anonymous(&t));
    }

    #[tokio::test]
    async fn probe_refuses_non_public_addresses() {
        let refused = |r: anyhow::Result<()>| {
            crate::errors::classify(&r.unwrap_err(), "s3").code == ErrorCode::SafetyRefused
        };
        assert!(refused(
            check_endpoint_address("http://169.254.169.254/latest/meta-data", None, false).await
        ));
        assert!(refused(
            check_endpoint_address("http://127.0.0.1:9000", None, false).await
        ));
        assert!(refused(
            check_endpoint_address("http://10.0.0.1", None, false).await
        ));
        assert!(refused(
            check_endpoint_address("http://[fd00::1]:80", None, false).await
        ));
        assert!(refused(
            check_endpoint_address("https://8.8.8.8", Some("http://192.168.0.1:3128"), false).await
        ));
        assert!(refused(
            check_endpoint_address("s3://b/x?endpoint=http://169.254.169.254", None, false).await
        ));
        check_endpoint_address("https://8.8.8.8", None, false)
            .await
            .unwrap();
        check_endpoint_address("lab://demo", None, false)
            .await
            .unwrap();
        check_endpoint_address("http://127.0.0.1:9000", None, true)
            .await
            .unwrap();
    }
}
