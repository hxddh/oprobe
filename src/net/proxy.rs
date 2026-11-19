//! HTTP CONNECT egress proxy support (`HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY`).

use std::io;
use std::net::IpAddr;

use tokio::io::{AsyncRead, AsyncWrite};

use super::dial::{read_response_head, write_all};

#[derive(Clone, Debug, PartialEq)]
pub struct ProxyConfig {
    pub host: String,
    pub port: u16,
    auth: Option<String>,
    no_proxy: Vec<String>,
}

impl ProxyConfig {
    /// Parse `http://[user:pass@]host:port`.
    pub fn parse(raw: &str, no_proxy: &str) -> anyhow::Result<Self> {
        let with_scheme = if raw.contains("://") {
            raw.to_owned()
        } else {
            format!("http://{raw}")
        };
        let u = url::Url::parse(&with_scheme)
            .map_err(|e| anyhow::anyhow!("invalid proxy URL `{raw}`: {e}"))?;
        anyhow::ensure!(
            u.scheme() == "http",
            "only http:// proxies (HTTP CONNECT) are supported, got `{}`",
            u.scheme()
        );
        let host = u
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("proxy URL `{raw}` has no host"))?
            .trim_matches(['[', ']'])
            .to_owned();
        let port = u.port().unwrap_or(80);
        let auth = if !u.username().is_empty() {
            let user = percent_decode(u.username());
            let pass = percent_decode(u.password().unwrap_or(""));
            Some(base64(format!("{user}:{pass}").as_bytes()))
        } else {
            None
        };
        Ok(Self {
            host,
            port,
            auth,
            no_proxy: no_proxy
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        })
    }

    /// Resolve the proxy for a target scheme from the standard environment variables.
    pub fn from_env(https: bool) -> Option<Self> {
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .or_else(|| std::env::var(k.to_ascii_lowercase()).ok())
                .filter(|v| !v.trim().is_empty())
        };
        let raw = if https {
            get("HTTPS_PROXY").or_else(|| get("ALL_PROXY"))
        } else {
            get("HTTP_PROXY").or_else(|| get("ALL_PROXY"))
        }?;
        let no_proxy = get("NO_PROXY").unwrap_or_default();
        Self::parse(&raw, &no_proxy).ok()
    }

    /// Display form without credentials.
    pub fn display(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Whether `host` matches a `NO_PROXY` entry.
    pub fn bypass(&self, host: &str) -> bool {
        let h = host.trim_matches(['[', ']']).to_ascii_lowercase();
        let ip: Option<IpAddr> = h.parse().ok();
        self.no_proxy.iter().any(|rule| {
            if rule == "*" {
                return true;
            }
            if let (Some(ip), Some((net, bits))) = (ip, rule.split_once('/')) {
                return cidr_contains(net, bits, ip);
            }
            let r = rule.trim_start_matches("*.").trim_start_matches('.');
            h == r || h.ends_with(&format!(".{r}"))
        })
    }

    pub(crate) async fn connect_tunnel<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        s: &mut S,
        host: &str,
        port: u16,
    ) -> io::Result<()> {
        let authority = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let mut req = format!(
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: oprobe/{}\r\n",
            env!("CARGO_PKG_VERSION")
        );
        if let Some(a) = &self.auth {
            req.push_str(&format!("Proxy-Authorization: Basic {a}\r\n"));
        }
        req.push_str("\r\n");
        write_all(s, req.as_bytes()).await?;
        let head = read_response_head(s).await?;
        let status_line = head.lines().next().unwrap_or_default();
        let code: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        if code / 100 != 2 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("proxy refused CONNECT {authority}: {status_line}"),
            ));
        }
        Ok(())
    }
}

fn cidr_contains(net: &str, bits: &str, ip: IpAddr) -> bool {
    let (Ok(net), Ok(bits)) = (net.parse::<IpAddr>(), bits.parse::<u32>()) else {
        return false;
    };
    match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(i)) if bits <= 32 => {
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            (u32::from(n) & mask) == (u32::from(i) & mask)
        }
        (IpAddr::V6(n), IpAddr::V6(i)) if bits <= 128 => {
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            (u128::from(n) & mask) == (u128::from(i) & mask)
        }
        _ => false,
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_and_no_proxy() {
        let p = ProxyConfig::parse(
            "http://u:p%40ss@10.0.0.1:3128",
            "localhost,.internal,10.0.0.0/8,*.svc",
        )
        .unwrap();
        assert_eq!(p.host, "10.0.0.1");
        assert_eq!(p.port, 3128);
        assert_eq!(p.auth.as_deref(), Some(base64(b"u:p@ss").as_str()));
        assert!(p.bypass("localhost"));
        assert!(p.bypass("a.b.internal"));
        assert!(p.bypass("10.1.2.3"));
        assert!(p.bypass("x.svc"));
        assert!(!p.bypass("s3.amazonaws.com"));
        assert_eq!(base64(b"hello"), "aGVsbG8=");
    }
}
