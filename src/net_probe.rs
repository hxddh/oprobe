//! Microsecond-precision low-level DNS + TCP + TLS 1.2/1.3 + ALPN + HTTP/1.1 & HTTP/2
//! cold-vs-warm handshake profiler (`oprobe net-probe`).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use hdrhistogram::Histogram;
use http::{Method, Request};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls_pki_types::ServerName;
use serde::{Deserialize, Serialize};
use tokio::net::{lookup_host, TcpStream};
use tokio::time::timeout;
use url::Url;

use crate::tls::{build_tls_connector, TlsClientOptions};

/// Single socket + TLS + HTTP cold/warm handshake sample.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HandshakeSample {
    pub sample_index: usize,
    pub remote_addr: String,
    pub http_status: u16,
    pub dns_lookup_us: u64,
    pub tcp_connect_us: u64,
    pub tls_handshake_us: u64,
    pub http_head_ttfb_us: u64,
    pub warm_reused_ttfb_us: u64,
    pub alpn_protocol: String,
    pub tls_version: String,
    pub cipher_suite: String,
}

/// Aggregated endpoint handshake & warm-vs-cold TTFB profile.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EndpointHandshakeSummary {
    pub endpoint: String,
    pub host: String,
    pub port: u16,
    pub uses_tls: bool,
    pub resolved_ips: Vec<String>,
    pub sample_count: usize,
    pub dns_p50_ms: f64,
    pub dns_p99_ms: f64,
    pub tcp_rtt_p50_ms: f64,
    pub tcp_rtt_p99_ms: f64,
    pub tls_p50_ms: f64,
    pub tls_p99_ms: f64,
    pub head_ttfb_p50_ms: f64,
    pub head_ttfb_p99_ms: f64,
    pub warm_ttfb_p50_ms: f64,
    pub warm_ttfb_p99_ms: f64,
    pub cold_total_ms: f64,
    pub cold_vs_warm_ratio: f64,
    pub negotiated_alpn: String,
    pub tls_version: String,
    pub cipher_suite: String,
    pub samples: Vec<HandshakeSample>,
}

impl EndpointHandshakeSummary {
    /// Zero-network profile returned for local `memory` or `fs` backends.
    pub fn local_in_process(scheme: &str) -> Self {
        Self {
            endpoint: format!("{scheme}://in-process"),
            host: "localhost".to_owned(),
            port: 0,
            uses_tls: false,
            resolved_ips: vec!["127.0.0.1".to_owned()],
            sample_count: 0,
            dns_p50_ms: 0.0,
            dns_p99_ms: 0.0,
            tcp_rtt_p50_ms: 0.0,
            tcp_rtt_p99_ms: 0.0,
            tls_p50_ms: 0.0,
            tls_p99_ms: 0.0,
            head_ttfb_p50_ms: 0.0,
            head_ttfb_p99_ms: 0.0,
            warm_ttfb_p50_ms: 0.0,
            warm_ttfb_p99_ms: 0.0,
            cold_total_ms: 0.0,
            cold_vs_warm_ratio: 1.0,
            negotiated_alpn: "in-process".to_owned(),
            tls_version: "none".to_owned(),
            cipher_suite: "none".to_owned(),
            samples: Vec::new(),
        }
    }
}

async fn measure_http1_cold_and_warm<I>(
    io: TokioIo<I>,
    host_header: &str,
    path_and_query: &str,
    step_timeout: Duration,
) -> Result<(u16, u64, u64)>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .context("HTTP/1.1 client handshake failed")?;
    let conn_task = tokio::spawn(async move {
        let _ = conn.await;
    });

    let build_req = || -> Result<Request<Empty<Bytes>>> {
        Ok(Request::builder()
            .method(Method::HEAD)
            .uri(path_and_query)
            .header(http::header::HOST, host_header)
            .header(http::header::USER_AGENT, "oprobe/1.0.0")
            .header(http::header::CONNECTION, "keep-alive")
            .body(Empty::<Bytes>::new())?)
    };

    let t_cold = Instant::now();
    let resp1 = timeout(step_timeout, sender.send_request(build_req()?))
        .await
        .context("HTTP/1.1 cold HEAD timed out")?
        .context("HTTP/1.1 cold HEAD failed")?;
    let cold_ttfb_us = (t_cold.elapsed().as_micros() as u64).max(1);
    let status = resp1.status().as_u16();
    let _ = resp1.into_body().collect().await;

    // Measure warm reused HEAD on the exact same open socket if server kept connection alive
    let warm_ttfb_us = if sender.ready().await.is_ok() {
        let t_warm = Instant::now();
        match timeout(step_timeout, sender.send_request(build_req()?)).await {
            Ok(Ok(resp2)) => {
                let us = (t_warm.elapsed().as_micros() as u64).max(1);
                let _ = resp2.into_body().collect().await;
                us
            }
            _ => cold_ttfb_us,
        }
    } else {
        cold_ttfb_us
    };

    conn_task.abort();
    Ok((status, cold_ttfb_us, warm_ttfb_us))
}

async fn measure_http2_cold_and_warm<I>(
    io: TokioIo<I>,
    authority: &str,
    scheme: &str,
    path_and_query: &str,
    step_timeout: Duration,
) -> Result<(u16, u64, u64)>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
        .await
        .context("HTTP/2 client handshake failed")?;
    let conn_task = tokio::spawn(async move {
        let _ = conn.await;
    });

    let full_uri = format!("{scheme}://{authority}{path_and_query}");
    let build_req = || -> std::result::Result<Request<Empty<Bytes>>, http::Error> {
        Request::builder()
            .method(Method::HEAD)
            .uri(&full_uri)
            .header(http::header::USER_AGENT, "oprobe/1.0.0")
            .body(Empty::<Bytes>::new())
    };

    let t_cold = Instant::now();
    let resp1 = timeout(step_timeout, sender.send_request(build_req()?))
        .await
        .context("HTTP/2 cold HEAD timed out")?
        .context("HTTP/2 cold HEAD failed")?;
    let cold_ttfb_us = (t_cold.elapsed().as_micros() as u64).max(1);
    let status = resp1.status().as_u16();
    let _ = resp1.into_body().collect().await;

    let warm_ttfb_us = if sender.ready().await.is_ok() {
        let t_warm = Instant::now();
        match timeout(step_timeout, sender.send_request(build_req()?)).await {
            Ok(Ok(resp2)) => {
                let us = (t_warm.elapsed().as_micros() as u64).max(1);
                let _ = resp2.into_body().collect().await;
                us
            }
            _ => cold_ttfb_us,
        }
    } else {
        cold_ttfb_us
    };

    conn_task.abort();
    let _: Option<Infallible> = None;
    Ok((status, cold_ttfb_us, warm_ttfb_us))
}

/// Probe an HTTP/HTTPS endpoint with `rounds` independent cold dials + warm reuse checks
/// using default WebPKI/system TLS roots.
pub async fn probe_endpoint_handshakes(
    endpoint_url: &str,
    rounds: usize,
    per_step_timeout: Duration,
) -> Result<EndpointHandshakeSummary> {
    probe_endpoint_handshakes_with_tls(
        endpoint_url,
        rounds,
        per_step_timeout,
        &TlsClientOptions::default(),
    )
    .await
}

/// Probe an HTTP/HTTPS endpoint with custom [`TlsClientOptions`] (e.g. custom CA or `--insecure`).
pub async fn probe_endpoint_handshakes_with_tls(
    endpoint_url: &str,
    rounds: usize,
    per_step_timeout: Duration,
    tls_opts: &TlsClientOptions,
) -> Result<EndpointHandshakeSummary> {
    let parsed =
        Url::parse(endpoint_url).with_context(|| format!("invalid endpoint URL: {endpoint_url}"))?;
    let scheme = parsed.scheme().to_owned();
    let uses_tls = match scheme.as_str() {
        "https" => true,
        "http" => false,
        other => anyhow::bail!("unsupported URL scheme `{other}` (expected http:// or https://)"),
    };
    let host = parsed
        .host_str()
        .context("endpoint URL missing host")?
        .trim_matches(['[', ']'])
        .to_owned();
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if uses_tls { 443 } else { 80 });

    let authority = if (uses_tls && port == 443) || (!uses_tls && port == 80) {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let path_and_query = match parsed.query() {
        Some(q) => format!("{}?{q}", parsed.path()),
        None => parsed.path().to_owned(),
    };
    let path_and_query = if path_and_query.is_empty() {
        "/".to_owned()
    } else {
        path_and_query
    };

    let rounds = rounds.clamp(1, 64);
    let connector = if uses_tls {
        Some(build_tls_connector(tls_opts)?)
    } else {
        None
    };

    let mut dns_hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)?;
    let mut tcp_hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)?;
    let mut tls_hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)?;
    let mut cold_ttfb_hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)?;
    let mut warm_ttfb_hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)?;

    let mut resolved_ips = Vec::new();
    let mut samples = Vec::with_capacity(rounds);
    let mut negotiated_alpn = "http/1.1".to_owned();
    let mut tls_version = if uses_tls {
        "TLSv1.3".to_owned()
    } else {
        "plain".to_owned()
    };
    let mut cipher_suite = "none".to_owned();

    for idx in 0..rounds {
        // Stage 1: DNS Resolution
        let t_dns = Instant::now();
        let addrs: Vec<SocketAddr> = timeout(per_step_timeout, lookup_host((host.as_str(), port)))
            .await
            .context("DNS lookup timed out")?
            .with_context(|| format!("failed to resolve {host}:{port}"))?
            .collect();
        let dns_us = (t_dns.elapsed().as_micros() as u64).max(1);

        if idx == 0 {
            resolved_ips = addrs.iter().map(|a| a.ip().to_string()).collect();
            resolved_ips.sort();
            resolved_ips.dedup();
        }
        anyhow::ensure!(!addrs.is_empty(), "DNS returned zero socket addresses");

        // Stage 2: TCP SYN -> SYN-ACK Connect (dual-stack aware)
        let t_tcp = Instant::now();
        let tcp_stream = timeout(per_step_timeout, TcpStream::connect(&addrs[..]))
            .await
            .context("TCP connect timed out")?
            .with_context(|| format!("TCP connect to {host}:{port} failed"))?;
        let tcp_us = (t_tcp.elapsed().as_micros() as u64).max(1);
        let target_addr = tcp_stream.peer_addr().unwrap_or(addrs[0]);
        let _ = tcp_stream.set_nodelay(true);

        // Stage 3 & 4: Optional TLS Handshake + Real HTTP/1.1 or HTTP/2 Cold & Warm HEAD TTFB
        let mut tls_us = 0_u64;
        let (http_status, head_ttfb_us, warm_reused_ttfb_us) = if let Some(ref tls_conn) = connector
        {
            let server_name = ServerName::try_from(host.clone())
                .map_err(|_| anyhow::anyhow!("invalid DNS/IP name for TLS SNI: {host}"))?;

            let t_tls = Instant::now();
            let tls_stream = timeout(per_step_timeout, tls_conn.connect(server_name, tcp_stream))
                .await
                .context("TLS handshake timed out")?
                .context("TLS handshake failed")?;
            tls_us = (t_tls.elapsed().as_micros() as u64).max(1);

            let (_, conn_info) = tls_stream.get_ref();
            negotiated_alpn = conn_info
                .alpn_protocol()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_else(|| "http/1.1".to_owned());
            if let Some(ver) = conn_info.protocol_version() {
                tls_version = format!("{ver:?}");
            }
            if let Some(suite) = conn_info.negotiated_cipher_suite() {
                cipher_suite = format!("{:?}", suite.suite());
            }

            let io = TokioIo::new(tls_stream);
            if negotiated_alpn == "h2" {
                measure_http2_cold_and_warm(
                    io,
                    &authority,
                    &scheme,
                    &path_and_query,
                    per_step_timeout,
                )
                .await?
            } else {
                measure_http1_cold_and_warm(io, &authority, &path_and_query, per_step_timeout)
                    .await?
            }
        } else {
            let io = TokioIo::new(tcp_stream);
            measure_http1_cold_and_warm(io, &authority, &path_and_query, per_step_timeout).await?
        };

        let _ = dns_hist.record(dns_us.clamp(1, 60_000_000));
        let _ = tcp_hist.record(tcp_us.clamp(1, 60_000_000));
        if tls_us > 0 {
            let _ = tls_hist.record(tls_us.clamp(1, 60_000_000));
        }
        let _ = cold_ttfb_hist.record(head_ttfb_us.clamp(1, 60_000_000));
        let _ = warm_ttfb_hist.record(warm_reused_ttfb_us.clamp(1, 60_000_000));

        samples.push(HandshakeSample {
            sample_index: idx + 1,
            remote_addr: target_addr.to_string(),
            http_status,
            dns_lookup_us: dns_us,
            tcp_connect_us: tcp_us,
            tls_handshake_us: tls_us,
            http_head_ttfb_us: head_ttfb_us,
            warm_reused_ttfb_us,
            alpn_protocol: negotiated_alpn.clone(),
            tls_version: tls_version.clone(),
            cipher_suite: cipher_suite.clone(),
        });
    }

    let dns_p50_ms = dns_hist.value_at_quantile(0.50) as f64 / 1000.0;
    let dns_p99_ms = dns_hist.value_at_quantile(0.99) as f64 / 1000.0;
    let tcp_rtt_p50_ms = tcp_hist.value_at_quantile(0.50) as f64 / 1000.0;
    let tcp_rtt_p99_ms = tcp_hist.value_at_quantile(0.99) as f64 / 1000.0;
    let tls_p50_ms = if uses_tls {
        tls_hist.value_at_quantile(0.50) as f64 / 1000.0
    } else {
        0.0
    };
    let tls_p99_ms = if uses_tls {
        tls_hist.value_at_quantile(0.99) as f64 / 1000.0
    } else {
        0.0
    };
    let head_ttfb_p50_ms = cold_ttfb_hist.value_at_quantile(0.50) as f64 / 1000.0;
    let head_ttfb_p99_ms = cold_ttfb_hist.value_at_quantile(0.99) as f64 / 1000.0;
    let warm_ttfb_p50_ms = warm_ttfb_hist.value_at_quantile(0.50) as f64 / 1000.0;
    let warm_ttfb_p99_ms = warm_ttfb_hist.value_at_quantile(0.99) as f64 / 1000.0;

    let cold_total_ms = dns_p50_ms + tcp_rtt_p50_ms + tls_p50_ms + head_ttfb_p50_ms;
    let cold_vs_warm_ratio = if warm_ttfb_p50_ms > 0.001 {
        cold_total_ms / warm_ttfb_p50_ms
    } else {
        1.0
    };

    Ok(EndpointHandshakeSummary {
        endpoint: endpoint_url.to_owned(),
        host,
        port,
        uses_tls,
        resolved_ips,
        sample_count: samples.len(),
        dns_p50_ms,
        dns_p99_ms,
        tcp_rtt_p50_ms,
        tcp_rtt_p99_ms,
        tls_p50_ms,
        tls_p99_ms,
        head_ttfb_p50_ms,
        head_ttfb_p99_ms,
        warm_ttfb_p50_ms,
        warm_ttfb_p99_ms,
        cold_total_ms,
        cold_vs_warm_ratio,
        negotiated_alpn,
        tls_version,
        cipher_suite,
        samples,
    })
}
