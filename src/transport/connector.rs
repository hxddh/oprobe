//! Instrumented Hyper TCP/TLS connector (`tower_service::Service<http::Uri>`)
//! measuring exact per-connection DNS lookup, TCP SYN/ACK connect, TLS 1.2/1.3 + ALPN handshake,
//! kernel-level raw wire RX/TX byte counters, and exact socket write timestamps.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use await_tree::InstrumentAwait;
use hyper::rt::{Read as HyperRead, ReadBufCursor, Write as HyperWrite};
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{lookup_host, TcpStream};
use tokio::time::timeout;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tower_service::Service;

use crate::telemetry::TelemetryRegistry;
use crate::tls::{build_tls_connector, TlsClientOptions};

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// Per-socket white-box metadata attached to Hyper responses via `Connected::extra`.
#[derive(Clone, Debug)]
pub struct ConnectionTelemetry {
    pub conn_id: u64,
    pub remote_addr: SocketAddr,
    pub dns_us: u64,
    pub tcp_us: u64,
    pub tls_us: u64,
    pub alpn: String,
    pub tls_version: String,
    pub cipher_suite: String,
    pub dial_completed_at: Instant,
    conn_epoch: Instant,
    last_write_offset_us: Arc<AtomicU64>,
    pub dispatch_seq: Arc<AtomicU64>,
}

impl ConnectionTelemetry {
    /// Timestamp of the most recent socket `poll_write` completion on this connection.
    pub fn last_write_instant(&self) -> Instant {
        let offset_us = self.last_write_offset_us.load(Ordering::Acquire);
        self.conn_epoch + Duration::from_micros(offset_us)
    }
}

/// Raw TCP wrapper counting exact kernel wire RX/TX bytes (including TLS records & HTTP headers).
pub struct RawWireCounterStream {
    inner: TcpStream,
    telemetry: Option<Arc<TelemetryRegistry>>,
    conn_epoch: Instant,
    last_write_offset_us: Arc<AtomicU64>,
}

impl AsyncRead for RawWireCounterStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            let delta = buf.filled().len().saturating_sub(before) as u64;
            if delta > 0 {
                if let Some(ref tel) = self.telemetry {
                    if tel.is_measuring() {
                        tel.wire_bytes_rx.fetch_add(delta, Ordering::Relaxed);
                    }
                }
            }
        }
        poll
    }
}

impl AsyncWrite for RawWireCounterStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &poll {
            if *n > 0 {
                let offset_us = self.conn_epoch.elapsed().as_micros() as u64;
                self.last_write_offset_us.store(offset_us, Ordering::Release);
                if let Some(ref tel) = self.telemetry {
                    if tel.is_measuring() {
                        tel.wire_bytes_tx.fetch_add(*n as u64, Ordering::Relaxed);
                    }
                }
            }
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

enum MaybeTlsStream {
    Plain(RawWireCounterStream),
    Tls(Box<TlsStream<RawWireCounterStream>>),
}

impl AsyncRead for MaybeTlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Instrumented connection stream returned to Hyper's connection pool.
pub struct ProbedConnection {
    io: TokioIo<MaybeTlsStream>,
    meta: ConnectionTelemetry,
}

impl HyperRead for ProbedConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl HyperWrite for ProbedConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }
}

impl Connection for ProbedConnection {
    fn connected(&self) -> Connected {
        let mut conn = Connected::new().extra(self.meta.clone());
        if self.meta.alpn == "h2" {
            conn = conn.negotiated_h2();
        }
        conn
    }
}

/// Hyper connector measuring true DNS, TCP, and TLS 1.2/1.3 handshake timings on every dial.
#[derive(Clone)]
pub struct ProbedHttpConnector {
    tls_connector: TlsConnector,
    telemetry: Option<Arc<TelemetryRegistry>>,
    connect_timeout: Duration,
}

impl ProbedHttpConnector {
    pub fn try_new(
        tls_opts: &TlsClientOptions,
        telemetry: Option<Arc<TelemetryRegistry>>,
        connect_timeout: Duration,
    ) -> anyhow::Result<Self> {
        let tls_connector = build_tls_connector(tls_opts)?;
        Ok(Self {
            tls_connector,
            telemetry,
            connect_timeout,
        })
    }
}

impl Service<http::Uri> for ProbedHttpConnector {
    type Response = ProbedConnection;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: http::Uri) -> Self::Future {
        let tls_connector = self.tls_connector.clone();
        let telemetry = self.telemetry.clone();
        let step_timeout = self.connect_timeout;

        Box::pin(async move {
            let scheme = dst.scheme_str().unwrap_or("http");
            let is_https = match scheme {
                "https" => true,
                "http" => false,
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unsupported URI scheme `{other}`"),
                    ));
                }
            };

            let host = dst
                .host()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "URI missing host"))?
                .trim_matches(['[', ']'])
                .to_owned();
            let port = dst.port_u16().unwrap_or(if is_https { 443 } else { 80 });

            // 1. Exact DNS Resolution
            let t_dns = Instant::now();
            let addrs: Vec<SocketAddr> = timeout(
                step_timeout,
                lookup_host((host.as_str(), port))
                    .instrument_await(await_tree::span!("net::dns_lookup {host}:{port}")),
            )
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "DNS lookup timed out"))??
            .collect();
            let dns_us = t_dns.elapsed().as_micros() as u64;

            if addrs.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("DNS returned zero addresses for {host}:{port}"),
                ));
            }

            // 2. Exact TCP SYN -> SYN-ACK Handshake (supports dual-stack fallback across resolved IPs)
            let t_tcp = Instant::now();
            let tcp_stream = timeout(
                step_timeout,
                TcpStream::connect(&addrs[..])
                    .instrument_await(await_tree::span!("net::tcp_connect {host}:{port}")),
            )
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TCP connect timed out"))??;
            let tcp_us = t_tcp.elapsed().as_micros() as u64;
            let remote_addr = tcp_stream.peer_addr().unwrap_or(addrs[0]);
            let _ = tcp_stream.set_nodelay(true);

            let conn_epoch = Instant::now();
            let last_write_offset_us = Arc::new(AtomicU64::new(0));
            let wire_stream = RawWireCounterStream {
                inner: tcp_stream,
                telemetry,
                conn_epoch,
                last_write_offset_us: last_write_offset_us.clone(),
            };

            // 3. Optional TLS 1.2/1.3 + ALPN Handshake
            let mut tls_us = 0_u64;
            let mut alpn = "http/1.1".to_owned();
            let mut tls_version = "plain".to_owned();
            let mut cipher_suite = "none".to_owned();

            let stream = if is_https {
                let server_name = ServerName::try_from(host.clone()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid TLS SNI host `{host}`"),
                    )
                })?;

                let t_tls = Instant::now();
                let tls_stream = timeout(
                    step_timeout,
                    tls_connector
                        .connect(server_name, wire_stream)
                        .instrument_await(await_tree::span!("net::tls_handshake {host}")),
                )
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"))??;
                tls_us = t_tls.elapsed().as_micros() as u64;

                let (_, conn_info) = tls_stream.get_ref();
                if let Some(proto) = conn_info.alpn_protocol() {
                    alpn = String::from_utf8_lossy(proto).into_owned();
                }
                if let Some(ver) = conn_info.protocol_version() {
                    tls_version = format!("{ver:?}");
                }
                if let Some(suite) = conn_info.negotiated_cipher_suite() {
                    cipher_suite = format!("{:?}", suite.suite());
                }

                MaybeTlsStream::Tls(Box::new(tls_stream))
            } else {
                MaybeTlsStream::Plain(wire_stream)
            };

            let dial_completed_at = Instant::now();
            let init_offset = conn_epoch.elapsed().as_micros() as u64;
            last_write_offset_us.store(init_offset, Ordering::Release);

            let meta = ConnectionTelemetry {
                conn_id: NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed),
                remote_addr,
                dns_us,
                tcp_us,
                tls_us,
                alpn,
                tls_version,
                cipher_suite,
                dial_completed_at,
                conn_epoch,
                last_write_offset_us,
                dispatch_seq: Arc::new(AtomicU64::new(0)),
            };

            Ok(ProbedConnection {
                io: TokioIo::new(stream),
                meta,
            })
        })
    }
}
