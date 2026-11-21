//! Stand-alone endpoint microscope (`oprobe probe`): repeated cold dials with exact
//! DNS / TCP / proxy / TLS timing, then a cold and a warm request on each connection.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use http::{Method, Request};
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::{Deserialize, Serialize};

use super::dial::Dialer;
use super::proxy::ProxyConfig;
use super::tls::{TlsClientOptions, build_tls_connector};
use super::tlsinfo::CertChain;
use super::transport::HttpVersion;
use crate::model::TlsRow;
use crate::trace::Quantiles;
use crate::trace::recorder::quantiles;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProbeSample {
    pub remote: String,
    pub status: u16,
    pub dns_ms: f64,
    pub tcp_ms: f64,
    pub proxy_ms: f64,
    pub tls_ms: f64,
    /// First request on a fresh connection: request sent -> response headers.
    pub cold_ttfb_ms: f64,
    /// Second request reusing the same connection.
    pub warm_ttfb_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_date: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProbeReport {
    pub endpoint: String,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    pub resolved: Vec<String>,
    pub alpn: String,
    pub tls_version: String,
    pub cipher: String,
    pub dns: Quantiles,
    pub tcp: Quantiles,
    pub proxy_connect: Quantiles,
    pub tls_handshake: Quantiles,
    pub cold_ttfb: Quantiles,
    pub warm_ttfb: Quantiles,
    /// Median cost of a cold request (connect + cold TTFB) vs a warm request.
    pub cold_request_ms: f64,
    pub warm_request_ms: f64,
    /// Local clock minus server `Date` header, seconds (signature schemes reject > 900 s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_skew_s: Option<f64>,
    pub samples: Vec<ProbeSample>,
    /// Every handshake (the first is full; later ones resume when the server allows it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handshakes: Vec<TlsRow>,
    /// The certificate chain the server presented (`ProbeOptions::certs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_chain: Option<CertChain>,
}

pub struct ProbeOptions {
    pub samples: usize,
    pub timeout: Duration,
    pub http: HttpVersion,
    pub tls: TlsClientOptions,
    /// `None` = from environment, `Some(None)` = disabled.
    pub proxy: Option<Option<String>>,
    pub method: Method,
    /// Parse the server's certificate chain.
    pub certs: bool,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            samples: 5,
            timeout: Duration::from_secs(10),
            http: HttpVersion::Auto,
            tls: TlsClientOptions::default(),
            proxy: None,
            method: Method::HEAD,
            certs: true,
        }
    }
}

async fn two_requests<I>(
    io: TokioIo<I>,
    h2: bool,
    uri: &str,
    host_hdr: &str,
    method: &Method,
    step: Duration,
) -> Result<(u16, f64, f64, Option<String>)>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let build = |h2: bool| -> Result<Request<Empty<Bytes>>> {
        let mut b = Request::builder().method(method.clone()).header(
            http::header::USER_AGENT,
            concat!("oprobe/", env!("CARGO_PKG_VERSION")),
        );
        if h2 {
            b = b.uri(uri);
        } else {
            let pq = uri
                .parse::<http::Uri>()?
                .path_and_query()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "/".into());
            b = b.uri(pq).header(http::header::HOST, host_hdr);
        }
        Ok(b.body(Empty::new())?)
    };
    macro_rules! timed {
        ($fut:expr_2021) => {{
            let t = Instant::now();
            let resp = tokio::time::timeout(step, $fut)
                .await
                .context("request timed out")??;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let status = resp.status().as_u16();
            let date = resp
                .headers()
                .get(http::header::DATE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let _ = resp.into_body().collect().await;
            (status, ms, date)
        }};
    }
    if h2 {
        let (mut s, conn) = http2::handshake(TokioExecutor::new(), io).await?;
        let task = tokio::spawn(conn);
        let (st, cold, date) = timed!(s.send_request(build(true)?));
        s.ready().await?;
        let (_, warm, _) = timed!(s.send_request(build(true)?));
        task.abort();
        Ok((st, cold, warm, date))
    } else {
        let (mut s, conn) = http1::handshake(io).await?;
        let task = tokio::spawn(conn);
        let (st, cold, date) = timed!(s.send_request(build(false)?));
        let warm = match tokio::time::timeout(step, s.ready()).await {
            Ok(Ok(())) => timed!(s.send_request(build(false)?)).1,
            _ => f64::NAN,
        };
        task.abort();
        Ok((st, cold, warm, date))
    }
}

/// Probe an `http(s)://` endpoint.
pub async fn probe_endpoint(endpoint: &str, opts: &ProbeOptions) -> Result<ProbeReport> {
    use crate::errors::{ErrorCode, OprobeError};
    let usage = |m: String| {
        OprobeError::new(ErrorCode::UsageInvalidArgument, m)
            .with_hint("pass an http(s):// endpoint such as https://s3.us-east-1.amazonaws.com, or a storage URL")
    };
    let url = url::Url::parse(endpoint)
        .map_err(|e| usage(format!("invalid endpoint URL `{endpoint}`: {e}")))?;
    let https = match url.scheme() {
        "https" => true,
        "http" => false,
        s => {
            return Err(usage(format!(
                "unsupported scheme `{s}` (expected http:// or https://)"
            ))
            .into());
        }
    };
    let host = url
        .host_str()
        .ok_or_else(|| usage(format!("endpoint `{endpoint}` has no host")))?
        .trim_matches(['[', ']'])
        .to_owned();
    let port = url
        .port_or_known_default()
        .unwrap_or(if https { 443 } else { 80 });
    let host_hdr = match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.clone(),
    };
    let mut tls = opts.tls.clone();
    tls.alpn_protocols = match opts.http {
        HttpVersion::Auto => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        HttpVersion::H1 => vec![b"http/1.1".to_vec()],
        HttpVersion::H2 => vec![b"h2".to_vec()],
    };
    let proxy = match &opts.proxy {
        None => ProxyConfig::from_env(https),
        Some(None) => None,
        Some(Some(raw)) => Some(ProxyConfig::parse(raw, "")?),
    }
    .filter(|p| !p.bypass(&host));
    let mut dialer = Dialer::new(
        build_tls_connector(&tls)?,
        proxy.clone(),
        opts.timeout,
        None,
    );
    dialer.certs = opts.certs;

    let mk = || hdrhistogram::Histogram::<u64>::new_with_bounds(1, 3_600_000_000, 3).expect("hist");
    let (mut h_dns, mut h_tcp, mut h_proxy, mut h_tls, mut h_cold, mut h_warm) =
        (mk(), mk(), mk(), mk(), mk(), mk());
    let mut report = ProbeReport {
        endpoint: endpoint.to_owned(),
        host: host.clone(),
        port,
        tls: https,
        proxy: proxy.as_ref().map(|p| p.display()),
        ..Default::default()
    };
    let mut skews = Vec::new();
    for _ in 0..opts.samples.clamp(1, 100) {
        let d = dialer
            .dial(https, &host, port)
            .await
            .map_err(anyhow::Error::new)?;
        if report.resolved.is_empty() {
            let mut r: Vec<String> = d.resolved.iter().map(|a| a.ip().to_string()).collect();
            r.dedup();
            report.resolved = r;
        }
        let h2 = match d.alpn.as_deref() {
            Some("h2") => true,
            Some(_) => false,
            None => opts.http == HttpVersion::H2,
        };
        report.alpn = d.alpn.clone().unwrap_or_else(|| "http/1.1 (plain)".into());
        report.tls_version = d.tls_version.clone();
        report.cipher = d.cipher.clone();
        if let Some(t) = &d.tls {
            report.handshakes.push(t.clone());
        }
        if report.cert_chain.is_none() {
            report.cert_chain = d.certs.clone();
        }
        let t = d.timing;
        let remote = d.remote.to_string();
        let t_req = Instant::now();
        let (status, cold, warm, date) = two_requests(
            TokioIo::new(d.stream),
            h2,
            url.as_str(),
            &host_hdr,
            &opts.method,
            opts.timeout,
        )
        .await?;
        // `Date` comes from the first (cold) response: compare it with the local clock halfway
        // through that request, not through both.
        let local_now = std::time::SystemTime::now() - t_req.elapsed()
            + std::time::Duration::from_secs_f64((cold / 2000.0).max(0.0));
        let ms = |us: u64| us as f64 / 1000.0;
        let rec = |h: &mut hdrhistogram::Histogram<u64>, v: f64| {
            if v.is_finite() {
                let _ = h.record(((v * 1000.0) as u64).max(1));
            }
        };
        rec(&mut h_dns, ms(t.dns_us));
        rec(&mut h_tcp, ms(t.tcp_us));
        if t.proxy_us > 0 {
            rec(&mut h_proxy, ms(t.proxy_us));
        }
        if t.tls_us > 0 {
            rec(&mut h_tls, ms(t.tls_us));
        }
        rec(&mut h_cold, cold);
        rec(&mut h_warm, warm);
        // `Date` has one-second resolution and is truncated: on average the server's clock
        // was half a second past it.
        if let Some(server) = date
            .as_deref()
            .and_then(httpdate_parse)
            .map(|d| d + std::time::Duration::from_millis(500))
        {
            let skew = match local_now.duration_since(server) {
                Ok(d) => d.as_secs_f64(),
                Err(e) => -e.duration().as_secs_f64(),
            };
            skews.push(skew);
        }
        report.samples.push(ProbeSample {
            remote,
            status,
            dns_ms: ms(t.dns_us),
            tcp_ms: ms(t.tcp_us),
            proxy_ms: ms(t.proxy_us),
            tls_ms: ms(t.tls_us),
            cold_ttfb_ms: cold,
            warm_ttfb_ms: warm,
            server_date: date,
        });
    }
    report.dns = quantiles(&h_dns);
    report.tcp = quantiles(&h_tcp);
    report.proxy_connect = quantiles(&h_proxy);
    report.tls_handshake = quantiles(&h_tls);
    report.cold_ttfb = quantiles(&h_cold);
    report.warm_ttfb = quantiles(&h_warm);
    report.cold_request_ms = report.dns.p50
        + report.tcp.p50
        + report.proxy_connect.p50
        + report.tls_handshake.p50
        + report.cold_ttfb.p50;
    report.warm_request_ms = report.warm_ttfb.p50;
    if !skews.is_empty() {
        skews.sort_by(f64::total_cmp);
        report.clock_skew_s = Some(skews[skews.len() / 2]);
    }
    Ok(report)
}

/// Parse an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub fn httpdate_parse(s: &str) -> Option<std::time::SystemTime> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let month = match parts[2] {
        "Jan" => "01",
        "Feb" => "02",
        "Mar" => "03",
        "Apr" => "04",
        "May" => "05",
        "Jun" => "06",
        "Jul" => "07",
        "Aug" => "08",
        "Sep" => "09",
        "Oct" => "10",
        "Nov" => "11",
        "Dec" => "12",
        _ => return None,
    };
    let hms: Vec<u32> = parts[4]
        .split(':')
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    if hms.len() != 3 {
        return None;
    }
    crate::util::utc_time(
        parts[3].parse().ok()?,
        month.parse().ok()?,
        parts[1].parse().ok()?,
        hms[0],
        hms[1],
        hms[2],
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_http_dates() {
        let t = super::httpdate_parse("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(crate::util::fmt_rfc3339(t), "1994-11-06T08:49:37Z");
    }
}
