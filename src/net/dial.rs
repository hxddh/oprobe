//! Connection establishment with exact per-phase timing: DNS → TCP → (proxy CONNECT) → TLS,
//! plus an instrumented socket that counts raw wire bytes and timestamps writes.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use await_tree::InstrumentAwait;
use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpStream, lookup_host};
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::proxy::ProxyConfig;
use super::tcpinfo::{self, TcpProbe};
use super::tlsinfo::{CertChain, group_name, suite_name, version_name};
use crate::model::TlsRow;
use crate::trace::{DialRecord, Recorder};

/// Per-connection clock shared between the socket wrapper and the request path.
#[derive(Debug)]
pub struct ConnClock {
    epoch: Instant,
    last_write_us: AtomicU64,
    pub rx: AtomicU64,
    pub tx: AtomicU64,
    /// The socket, for its kernel TCP statistics (`net::tcpinfo`).
    pub sock: Option<TcpProbe>,
}

impl ConnClock {
    fn new(sock: Option<TcpProbe>) -> Arc<Self> {
        Arc::new(Self {
            epoch: Instant::now(),
            last_write_us: AtomicU64::new(0),
            rx: AtomicU64::new(0),
            tx: AtomicU64::new(0),
            sock,
        })
    }

    /// Instant at which the most recent socket write completed.
    pub fn last_write(&self) -> Instant {
        self.epoch + Duration::from_micros(self.last_write_us.load(Ordering::Acquire))
    }
}

/// TCP socket wrapper counting raw wire bytes (TLS records + HTTP framing) and write times.
pub struct CountingStream {
    inner: TcpStream,
    clock: Arc<ConnClock>,
    recorder: Option<Arc<Recorder>>,
}

impl AsyncRead for CountingStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            let n = (buf.filled().len() - before) as u64;
            if n > 0 {
                self.clock.rx.fetch_add(n, Ordering::Relaxed);
                if let Some(r) = &self.recorder {
                    r.add_wire(n, 0);
                }
            }
        }
        poll
    }
}

impl AsyncWrite for CountingStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &poll
            && *n > 0
        {
            let us = self.clock.epoch.elapsed().as_micros() as u64;
            self.clock.last_write_us.store(us, Ordering::Release);
            self.clock.tx.fetch_add(*n as u64, Ordering::Relaxed);
            if let Some(r) = &self.recorder {
                r.add_wire(0, *n as u64);
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

/// Plain or TLS stream over the counting socket.
pub enum ConnStream {
    Plain(CountingStream),
    Tls(Box<TlsStream<CountingStream>>),
}

macro_rules! delegate {
    ($self:ident, $s:ident => $e:expr_2021) => {
        match $self.get_mut() {
            ConnStream::Plain($s) => {
                let $s = Pin::new($s);
                $e
            }
            ConnStream::Tls($s) => {
                let $s = Pin::new($s.as_mut());
                $e
            }
        }
    };
}

impl AsyncRead for ConnStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        delegate!(self, s => s.poll_read(cx, buf))
    }
}

impl AsyncWrite for ConnStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        delegate!(self, s => s.poll_write(cx, buf))
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, s => s.poll_flush(cx))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, s => s.poll_shutdown(cx))
    }
}

/// Exact handshake timings of one dial in microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DialTiming {
    pub dns_us: u64,
    pub tcp_us: u64,
    pub proxy_us: u64,
    pub tls_us: u64,
}

impl DialTiming {
    pub fn total_us(&self) -> u64 {
        self.dns_us + self.tcp_us + self.proxy_us + self.tls_us
    }
}

pub struct Dialed {
    pub stream: ConnStream,
    pub clock: Arc<ConnClock>,
    pub timing: DialTiming,
    /// Negotiated ALPN (`h2`, `http/1.1`) or `None` for plain HTTP.
    pub alpn: Option<String>,
    pub tls_version: String,
    pub cipher: String,
    /// The handshake (full / resumed / HelloRetryRequest, group, cipher) for request rows.
    pub tls: Option<TlsRow>,
    /// The server's certificate chain (only when the dialer captures it, see `Dialer::certs`).
    pub certs: Option<CertChain>,
    /// Address actually connected to (proxy address when tunnelling).
    pub remote: SocketAddr,
    pub resolved: Vec<SocketAddr>,
}

/// Classified dial failure (used for actionable diagnostics).
#[derive(Debug)]
pub struct DialError {
    pub stage: &'static str,
    pub target: String,
    pub source: io::Error,
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} failed for {}: {}",
            self.stage, self.target, self.source
        )
    }
}

impl std::error::Error for DialError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Clone)]
pub struct Dialer {
    pub tls: TlsConnector,
    pub proxy: Option<ProxyConfig>,
    pub connect_timeout: Duration,
    pub recorder: Option<Arc<Recorder>>,
    /// Address that last connected successfully per `host:port`; tried first on the next dial
    /// (like browsers do) so a dead address family costs the Happy Eyeballs stagger only once.
    pub last_good: Arc<std::sync::Mutex<std::collections::HashMap<String, SocketAddr>>>,
    /// Parse the server's certificate chain on every TLS dial (doctor / probe).
    pub certs: bool,
}

impl Dialer {
    pub fn new(
        tls: TlsConnector,
        proxy: Option<ProxyConfig>,
        connect_timeout: Duration,
        recorder: Option<Arc<Recorder>>,
    ) -> Self {
        Self {
            tls,
            proxy,
            connect_timeout,
            recorder,
            last_good: Default::default(),
            certs: false,
        }
    }

    /// Dial `host:port`, optionally performing a TLS handshake. Every phase is timed.
    pub async fn dial(&self, https: bool, host: &str, port: u16) -> Result<Dialed, DialError> {
        let res = self.dial_inner(https, host, port).await;
        if let Some(r) = &self.recorder {
            match &res {
                Ok(d) => r.record_dial(DialRecord {
                    ok: true,
                    proxy: self
                        .proxy
                        .as_ref()
                        .filter(|p| !p.bypass(host))
                        .map(|p| p.display()),
                    dns_us: d.timing.dns_us,
                    tcp_us: d.timing.tcp_us,
                    proxy_us: d.timing.proxy_us,
                    tls_us: d.timing.tls_us,
                    remote: d.remote.to_string(),
                    alpn: d.alpn.clone().unwrap_or_else(|| "http/1.1".into()),
                    tls_version: d.tls_version.clone(),
                    cipher: d.cipher.clone(),
                }),
                Err(_) => r.record_dial(DialRecord {
                    ok: false,
                    proxy: None,
                    dns_us: 0,
                    tcp_us: 0,
                    proxy_us: 0,
                    tls_us: 0,
                    remote: String::new(),
                    alpn: String::new(),
                    tls_version: String::new(),
                    cipher: String::new(),
                }),
            }
        }
        res
    }

    async fn dial_inner(&self, https: bool, host: &str, port: u16) -> Result<Dialed, DialError> {
        let target = format!("{host}:{port}");
        let err = |stage: &'static str, e: io::Error| DialError {
            stage,
            target: target.clone(),
            source: e,
        };
        let proxy = self.proxy.as_ref().filter(|p| !p.bypass(host));
        let (conn_host, conn_port) = match proxy {
            Some(p) => (p.host.as_str(), p.port),
            None => (host, port),
        };
        let step = self.connect_timeout;

        // 1. DNS
        let t = Instant::now();
        let addrs: Vec<SocketAddr> = match conn_host.parse::<std::net::IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, conn_port)],
            Err(_) => timeout(
                step,
                lookup_host((conn_host, conn_port))
                    .instrument_await(await_tree::span!("dns {conn_host}")),
            )
            .await
            .map_err(|_| {
                err(
                    "dns",
                    io::Error::new(io::ErrorKind::TimedOut, "DNS lookup timed out"),
                )
            })?
            .map_err(|e| err("dns", e))?
            .collect(),
        };
        let dns_us = t.elapsed().as_micros() as u64;
        if addrs.is_empty() {
            return Err(err(
                "dns",
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no addresses for {conn_host}"),
                ),
            ));
        }

        // 2. TCP
        let key = format!("{conn_host}:{conn_port}");
        let preferred = self
            .last_good
            .lock()
            .ok()
            .and_then(|m| m.get(&key).copied());
        let t = Instant::now();
        let tcp = timeout(
            step,
            happy_eyeballs(&addrs, preferred)
                .instrument_await(await_tree::span!("tcp {conn_host}:{conn_port}")),
        )
        .await
        .map_err(|_| {
            err(
                "tcp connect",
                io::Error::new(io::ErrorKind::TimedOut, "TCP connect timed out"),
            )
        })?
        .map_err(|e| err("tcp connect", e))?;
        let tcp_us = t.elapsed().as_micros() as u64;
        let remote = tcp.peer_addr().unwrap_or(addrs[0]);
        if preferred != Some(remote)
            && let Ok(mut m) = self.last_good.lock()
        {
            m.insert(key, remote);
        }
        let _ = tcp.set_nodelay(true);

        let clock = ConnClock::new(tcpinfo::sock_id_of(&tcp).map(TcpProbe::new));
        let mut sock = CountingStream {
            inner: tcp,
            clock: clock.clone(),
            recorder: self.recorder.clone(),
        };

        // 3. Proxy CONNECT tunnel
        let mut proxy_us = 0;
        if let Some(p) = proxy {
            let t = Instant::now();
            timeout(step, p.connect_tunnel(&mut sock, host, port))
                .instrument_await(await_tree::span!("proxy CONNECT {host}:{port}"))
                .await
                .map_err(|_| {
                    err(
                        "proxy CONNECT",
                        io::Error::new(io::ErrorKind::TimedOut, "proxy CONNECT timed out"),
                    )
                })?
                .map_err(|e| err("proxy CONNECT", e))?;
            proxy_us = t.elapsed().as_micros() as u64;
        }

        // 4. TLS
        if !https {
            return Ok(Dialed {
                stream: ConnStream::Plain(sock),
                clock,
                timing: DialTiming {
                    dns_us,
                    tcp_us,
                    proxy_us,
                    tls_us: 0,
                },
                alpn: None,
                tls_version: "none".into(),
                cipher: "none".into(),
                tls: None,
                certs: None,
                remote,
                resolved: addrs,
            });
        }
        let sni = ServerName::try_from(host.trim_matches(['[', ']']).to_owned()).map_err(|_| {
            err(
                "tls",
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid TLS server name `{host}`"),
                ),
            )
        })?;
        let t = Instant::now();
        let tls = timeout(
            step,
            self.tls
                .connect(sni, sock)
                .instrument_await(await_tree::span!("tls {host}")),
        )
        .await
        .map_err(|_| {
            err(
                "tls handshake",
                io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"),
            )
        })?
        .map_err(|e| err("tls handshake", e))?;
        let tls_us = t.elapsed().as_micros() as u64;
        let (_, info) = tls.get_ref();
        let alpn = info
            .alpn_protocol()
            .map(|p| String::from_utf8_lossy(p).into_owned());
        let tls_version = info
            .protocol_version()
            .map(version_name)
            .unwrap_or_default();
        let cipher = info
            .negotiated_cipher_suite()
            .map(|c| suite_name(c.suite()))
            .unwrap_or_default();
        let row = TlsRow {
            resumed: info.handshake_kind() == Some(rustls::HandshakeKind::Resumed),
            version: tls_version.clone(),
            cipher: (!cipher.is_empty()).then(|| cipher.clone()),
            hrr: info.handshake_kind() == Some(rustls::HandshakeKind::FullWithHelloRetryRequest),
            group: info
                .negotiated_key_exchange_group()
                .map(|g| group_name(g.name())),
        };
        let certs = if self.certs {
            info.peer_certificates().map(CertChain::from_der)
        } else {
            None
        };
        Ok(Dialed {
            stream: ConnStream::Tls(Box::new(tls)),
            clock,
            timing: DialTiming {
                dns_us,
                tcp_us,
                proxy_us,
                tls_us,
            },
            alpn,
            tls_version,
            cipher,
            tls: Some(row),
            certs,
            remote,
            resolved: addrs,
        })
    }
}

/// Read an HTTP/1 response head (up to `\r\n\r\n`) from a raw stream.
pub(crate) async fn read_response_head<S: AsyncRead + Unpin>(s: &mut S) -> io::Result<String> {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = s.read(&mut byte).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "proxy closed the connection",
            ));
        }
        buf.push(byte[0]);
        if buf.len() > 16 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "proxy response head too large",
            ));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

pub(crate) async fn write_all<S: AsyncWrite + Unpin>(s: &mut S, data: &[u8]) -> io::Result<()> {
    s.write_all(data).await?;
    s.flush().await
}

/// Interleave address families (RFC 8305 §4): v6, v4, v6, v4, ...
/// Move the address that worked last time to the front (if it is still in the DNS answer).
fn prefer(mut order: Vec<SocketAddr>, preferred: Option<SocketAddr>) -> Vec<SocketAddr> {
    if let Some(i) = preferred.and_then(|p| order.iter().position(|a| *a == p)) {
        let a = order.remove(i);
        order.insert(0, a);
    }
    order
}

fn interleave(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let (v6, v4): (Vec<SocketAddr>, Vec<SocketAddr>) = addrs.iter().partition(|a| a.is_ipv6());
    let first_v6 = addrs.first().is_some_and(|a| a.is_ipv6());
    let (a, b) = if first_v6 { (v6, v4) } else { (v4, v6) };
    let mut out = Vec::with_capacity(addrs.len());
    for i in 0..a.len().max(b.len()) {
        out.extend(a.get(i).copied());
        out.extend(b.get(i).copied());
    }
    out
}

/// Happy Eyeballs connect: start the next address 250 ms after the previous one (or as soon
/// as it fails), first successful connection wins. Avoids multi-second stalls on hosts where
/// one address family is unreachable (e.g. `localhost` → `::1` with an IPv4-only listener).
async fn happy_eyeballs(
    addrs: &[SocketAddr],
    preferred: Option<SocketAddr>,
) -> io::Result<TcpStream> {
    use futures::stream::{FuturesUnordered, StreamExt};
    const STAGGER: Duration = Duration::from_millis(250);
    let order = prefer(interleave(addrs), preferred);
    let mut pending = FuturesUnordered::new();
    let mut next = 0;
    let mut last_err = None;
    loop {
        if next < order.len() {
            pending.push(TcpStream::connect(order[next]));
            next += 1;
        }
        if pending.is_empty() {
            return Err(last_err.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no addresses to connect to")
            }));
        }
        let more = next < order.len();
        tokio::select! {
            res = pending.next() => match res {
                Some(Ok(s)) => return Ok(s),
                Some(Err(e)) => last_err = Some(e),
                None => {}
            },
            _ = tokio::time::sleep(STAGGER), if more => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaves_families() {
        let a: Vec<SocketAddr> = ["[::1]:1", "[::2]:1", "127.0.0.1:1", "127.0.0.2:1"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let o = interleave(&a);
        assert!(o[0].is_ipv6() && o[1].is_ipv4() && o[2].is_ipv6() && o[3].is_ipv4());
        let p = prefer(o.clone(), Some(a[3]));
        assert_eq!(p[0], a[3]);
        assert_eq!(p.len(), 4);
        assert_eq!(prefer(o.clone(), Some("10.0.0.1:1".parse().unwrap())), o);
    }

    #[tokio::test]
    async fn falls_back_quickly_from_dead_address() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good = l.local_addr().unwrap();
        // 10.255.255.1 is unroutable: connect would hang; the good address must win fast.
        let dead: SocketAddr = "10.255.255.1:9".parse().unwrap();
        let t = Instant::now();
        let s = happy_eyeballs(&[dead, good], None).await.unwrap();
        assert_eq!(s.peer_addr().unwrap(), good);
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }
}
