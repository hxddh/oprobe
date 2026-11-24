//! Production-fidelity transport: OpenDAL's own HTTP client (reqwest 0.13 + rustls/aws-lc-rs,
//! exactly as `opendal-http-transport-reqwest` builds it), observed only through reqwest's
//! public extension points:
//!
//! - a timed DNS resolver (`ClientBuilder::dns_resolver`, same `getaddrinfo` path as reqwest's
//!   default resolver),
//! - a connector layer (`ClientBuilder::connector_layer`) that times every dial and learns the
//!   connection's local/remote address,
//! - a delegating TLS session store that timestamps the ClientHello, splitting TCP from TLS,
//! - the response's `HttpInfo` (local address = connection identity), which marks the first
//!   request on every connection as cold and attributes that connection's dial to it.
//!
//! - the same rustls crypto provider and certificate verifier with delegating wrappers that
//!   report the negotiated cipher suite, key-exchange group, HelloRetryRequests and whether the
//!   handshake was resumed (`net::tlsinfo`),
//! - the kernel's TCP statistics of the connection at request end (`net::tcpinfo`: the socket
//!   is found by its addresses and only queried, never owned).
//!
//! What this transport cannot see: time waiting inside reqwest's pool for an
//! idle connection or HTTP/2 stream is part of TTFB.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use await_tree::InstrumentAwait;
use bytes::Bytes;
use futures::Stream;
use http::{Method, Request, Response};
use hyper_util::client::legacy::connect::{Connection, HttpInfo};
use opendal_core::raw::{ServiceOperation, parse_content_encoding, parse_content_length};
use opendal_core::{Buffer, Error, ErrorKind, HttpBody, HttpRedirect, HttpTransport, Result};
use rustls::client::{ClientSessionMemoryCache, ClientSessionStore, Resumption};
use rustls::{ClientConfig, NamedGroup};
use rustls_pki_types::ServerName;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::tcpinfo::{self, TcpProbe};
use super::tls::{InsecureCertVerifier, crypto_provider};
use super::tlsinfo::{HandshakeMarks, ObservedVerifier, TlsEvent, observed_provider};
use super::{HttpVersion, TransportConfig};
use crate::record::{Protocol, RequestRecord, RequestTiming};
use crate::recorder::DialRecord;
use crate::rows::{TcpRow, TlsRow};
use crate::{InflightGuard, OpScope, Recorder};

// ---------------------------------------------------------------------------------------------
// Dial timing (task-local while reqwest's connector future runs)

#[derive(Default, Debug, Clone, Copy)]
struct DialMarks {
    dns_start: Option<Instant>,
    dns_end: Option<Instant>,
    client_hello: Option<Instant>,
    tls_version: Option<&'static str>,
    hs: HandshakeMarks,
}

tokio::task_local! {
    static DIAL: Arc<Mutex<DialMarks>>;
}

fn mark(f: impl FnOnce(&mut DialMarks)) {
    let _ = DIAL.try_with(|m| {
        if let Ok(mut g) = m.lock() {
            f(&mut g)
        }
    });
}

/// Reports of the observing TLS provider / verifier (outside a dial they are ignored).
fn tls_hook(e: TlsEvent) {
    mark(|m| m.hs.apply(e));
}

/// A finished dial waiting for the first request that uses its connection.
#[derive(Clone, Debug)]
struct Dialed {
    dns_us: u64,
    tcp_us: u64,
    proxy_us: u64,
    tls_us: u64,
    end: Instant,
    tls: Option<TlsRow>,
}

/// Where the kernel statistics of a connection come from.
#[derive(Clone, Debug, Default)]
enum Sock {
    /// Not looked up yet (looked up once, at the end of the connection's first request).
    #[default]
    Unknown,
    /// Not found or not supported: never looked up again.
    Missing,
    Found(Arc<TcpProbe>),
}

#[derive(Default)]
struct ConnEntry {
    id: u64,
    dial: Option<Dialed>,
    dial_end: Option<Instant>,
    sock: Sock,
    active_streams: Arc<AtomicU32>,
    last_seq: u64,
}

/// Decrements a connection's active stream counter when the request finishes.
struct StreamGuard(Option<Arc<AtomicU32>>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if let Some(a) = &self.0 {
            a.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Bound on remembered connections (see [`Conns::by_addr`]).
const MAX_TRACKED_CONNS: usize = 8192;

#[derive(Default)]
struct Conns {
    seq: u64,
    /// Connection identity (local, remote address) -> (id, dial awaiting its first request).
    /// The local address alone is not unique: the kernel reuses a local port for connections
    /// to different peers, and object stores answer from many addresses.
    by_addr: HashMap<(SocketAddr, SocketAddr), ConnEntry>,
}

struct Shared {
    recorder: Option<Arc<Recorder>>,
    conns: Mutex<Conns>,
    /// Configured egress proxy (display form) and its addresses: a dial whose peer is one of
    /// them went through the proxy (reqwest applies NO_PROXY itself).
    proxy: Option<String>,
    proxy_addrs: Vec<SocketAddr>,
    /// Query `TCP_INFO` at request end.
    tcp_info: bool,
}

impl Shared {
    /// Kernel TCP statistics of connection `addr` now (see `net::tcpinfo` for why this is safe).
    /// `known` is what the connection table said when the response arrived.
    fn tcp_info(
        &self,
        addr: (SocketAddr, SocketAddr),
        known: Sock,
    ) -> Option<(TcpRow, Option<u64>)> {
        if !self.tcp_info {
            return None;
        }
        let probe = self.probe(addr, known)?;
        let (row, retrans) = probe.sample_with_retrans()?;
        // The bytes on the wire since the connection's previous sample: every request is
        // sampled, so the deltas add up to the connection's traffic.
        if let Some(r) = &self.recorder {
            let (rx, tx) = probe.wire_since_last(&row);
            r.add_wire(rx, tx);
        }
        Some((row, retrans))
    }

    /// The kernel probe of connection `addr` (looked up once per connection; racing streams on
    /// a newly discovered connection converge on the first installed `TcpProbe`).
    fn probe(&self, addr: (SocketAddr, SocketAddr), known: Sock) -> Option<Arc<TcpProbe>> {
        let id = match known {
            Sock::Found(p) => p,
            Sock::Missing => return None,
            Sock::Unknown => {
                // Scan the descriptor table without holding the lock; warm=true so an already
                // pooled connection never double-counts lifetime bytes.
                let found =
                    tcpinfo::find(addr.0, addr.1).map(|id| Arc::new(TcpProbe::new_seeded(id, true)));
                if let Ok(mut c) = self.conns.lock()
                    && let Some(e) = c.by_addr.get_mut(&addr)
                {
                    match &e.sock {
                        Sock::Found(existing) => return Some(existing.clone()),
                        Sock::Missing => return None,
                        Sock::Unknown => {
                            e.sock = found.clone().map_or(Sock::Missing, Sock::Found);
                        }
                    }
                }
                found?
            }
        };
        Some(id)
    }

    /// Unambiguous kernel probe when the pool holds a single idle connection (avoids attributing
    /// other connections' in-flight segments under multi-worker concurrency).
    fn single_probe(&self) -> Option<Arc<TcpProbe>> {
        let c = self.conns.lock().ok()?;
        if c.by_addr.len() != 1 {
            return None;
        }
        let e = c.by_addr.values().next()?;
        if e.active_streams.load(Ordering::Acquire) > 0 {
            return None;
        }
        match &e.sock {
            Sock::Found(p) => Some(p.clone()),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// DNS: timed getaddrinfo, like reqwest's default GaiResolver.

struct TimedResolver;

impl reqwest::dns::Resolve for TimedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            mark(|m| m.dns_start = Some(Instant::now()));
            let res = tokio::net::lookup_host((host.as_str(), 0)).await;
            mark(|m| m.dns_end = Some(Instant::now()));
            let addrs: Vec<SocketAddr> = res?.collect();
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// TLS: delegate to rustls' default in-memory session cache, timestamp the ClientHello.

#[derive(Debug)]
struct HelloClock(Arc<ClientSessionMemoryCache>);

impl ClientSessionStore for HelloClock {
    fn set_kx_hint(&self, server_name: ServerName<'static>, group: NamedGroup) {
        // Called at the end of a TLS 1.3 handshake with the group it used.
        mark(|m| m.hs.final_group = Some(group));
        self.0.set_kx_hint(server_name, group)
    }
    fn kx_hint(&self, server_name: &ServerName<'_>) -> Option<NamedGroup> {
        // Called while building the ClientHello, i.e. right after TCP (and proxy CONNECT).
        mark(|m| {
            m.client_hello.get_or_insert_with(Instant::now);
        });
        self.0.kx_hint(server_name)
    }
    fn set_tls12_session(
        &self,
        server_name: ServerName<'static>,
        value: rustls::client::Tls12ClientSessionValue,
    ) {
        self.0.set_tls12_session(server_name, value)
    }
    fn tls12_session(
        &self,
        server_name: &ServerName<'_>,
    ) -> Option<rustls::client::Tls12ClientSessionValue> {
        self.0.tls12_session(server_name)
    }
    fn remove_tls12_session(&self, server_name: &ServerName<'static>) {
        self.0.remove_tls12_session(server_name)
    }
    fn insert_tls13_ticket(
        &self,
        server_name: ServerName<'static>,
        value: rustls::client::Tls13ClientSessionValue,
    ) {
        self.0.insert_tls13_ticket(server_name, value)
    }
    fn take_tls13_ticket(
        &self,
        server_name: &ServerName<'static>,
    ) -> Option<rustls::client::Tls13ClientSessionValue> {
        self.0.take_tls13_ticket(server_name)
    }
}

/// Learns the negotiated TLS version from the key-schedule labels. Never receives a TLS 1.3
/// secret (`will_log` returns false); TLS 1.2's single `log` call is ignored.
#[derive(Debug)]
struct VersionProbe;

impl rustls::KeyLog for VersionProbe {
    fn log(&self, label: &str, _client_random: &[u8], _secret: &[u8]) {
        if label == "CLIENT_RANDOM" {
            mark(|m| m.tls_version = Some("TLSv1.2"));
        }
    }
    fn will_log(&self, label: &str) -> bool {
        if label.starts_with("CLIENT_HANDSHAKE_TRAFFIC_SECRET") {
            mark(|m| m.tls_version = Some("TLSv1.3"));
        }
        false
    }
}

/// The rustls config reqwest 0.13 builds by default (aws-lc-rs, TLS 1.2 + 1.3, platform
/// verifier, extra roots merged), plus the ClientHello clock and the observing wrappers of
/// `net::tlsinfo` (same suites, groups and verifier; they only report what was negotiated).
fn rustls_config(cfg: &TransportConfig, alpn: Vec<Vec<u8>>) -> anyhow::Result<ClientConfig> {
    let base = crypto_provider();
    let provider = observed_provider(&base, tls_hook);
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(rustls::ALL_VERSIONS)?;
    let mut tls = if cfg.tls.insecure_skip_verify {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(ObservedVerifier {
                inner: InsecureCertVerifier { provider },
                hook: tls_hook,
            }))
            .with_no_client_auth()
    } else {
        let extra: Vec<rustls_pki_types::CertificateDer<'static>> = match &cfg.tls.extra_ca_pem {
            Some(pem) => {
                use rustls_pki_types::pem::PemObject;
                rustls_pki_types::CertificateDer::pem_slice_iter(pem.as_bytes())
                    .collect::<std::result::Result<_, _>>()?
            }
            None => Vec::new(),
        };
        let verifier = if extra.is_empty() {
            rustls_platform_verifier::Verifier::new(provider)?
        } else {
            #[cfg(any(all(unix, not(target_os = "android")), target_os = "windows"))]
            {
                rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider)?
            }
            #[cfg(not(any(all(unix, not(target_os = "android")), target_os = "windows")))]
            {
                anyhow::bail!("extra CA certificates are not supported on this platform")
            }
        };
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(ObservedVerifier {
                inner: verifier,
                hook: tls_hook,
            }))
            .with_no_client_auth()
    };
    tls.alpn_protocols = alpn;
    tls.key_log = Arc::new(VersionProbe);
    tls.resumption = Resumption::store(Arc::new(HelloClock(Arc::new(
        ClientSessionMemoryCache::new(256),
    ))));
    Ok(tls)
}

// ---------------------------------------------------------------------------------------------
// Connector layer: time each dial, key it by the connection's local address.

#[derive(Clone)]
struct DialLayer(Arc<Shared>);

#[derive(Clone)]
struct DialService<S> {
    inner: S,
    shared: Arc<Shared>,
}

impl<S> tower_layer::Layer<S> for DialLayer {
    type Service = DialService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        DialService {
            inner,
            shared: self.0.clone(),
        }
    }
}

impl<S, R> tower_service::Service<R> for DialService<S>
where
    S: tower_service::Service<R>,
    S::Response: Connection,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let shared = self.shared.clone();
        let marks = Arc::new(Mutex::new(DialMarks::default()));
        let fut = DIAL
            .scope(marks.clone(), self.inner.call(req))
            .instrument_await("http::dial");
        Box::pin(async move {
            let start = Instant::now();
            let res = fut.await;
            let end = Instant::now();
            let m = marks.lock().map(|g| *g).unwrap_or_default();
            let tls = m.tls_version.map(|v| m.hs.row(&v.replace("TLSv", "TLS")));
            let us = |a: Instant, b: Instant| b.saturating_duration_since(a).as_micros() as u64;
            let dns_us = match (m.dns_start, m.dns_end) {
                (Some(a), Some(b)) => us(a, b),
                _ => 0,
            };
            let tcp_from = m.dns_end.unwrap_or(start);
            let (mut tcp_us, tls_us) = match m.client_hello {
                Some(h) => (us(tcp_from, h), us(h, end)),
                None => (us(tcp_from, end), 0),
            };
            match &res {
                Ok(conn) => {
                    let connected = conn.connected();
                    let mut ext = http::Extensions::new();
                    connected.get_extras(&mut ext);
                    let info = ext.get::<HttpInfo>().cloned();
                    let proxied = info
                        .as_ref()
                        .is_some_and(|i| shared.proxy_addrs.contains(&i.remote_addr()));
                    // Through a proxy, TCP to the proxy and the CONNECT exchange are not
                    // separable from outside reqwest: report them together as proxy time.
                    let proxy_us = if proxied && tls_us > 0 {
                        std::mem::take(&mut tcp_us)
                    } else {
                        0
                    };
                    if let Some(r) = &shared.recorder {
                        r.record_dial(DialRecord {
                            ok: true,
                            proxy: shared.proxy.clone().filter(|_| proxied),
                            dns_us,
                            tcp_us,
                            proxy_us,
                            tls_us,
                            remote: info
                                .as_ref()
                                .map(|i| i.remote_addr().to_string())
                                .unwrap_or_default(),
                            alpn: if connected.is_negotiated_h2() {
                                "h2".into()
                            } else if tls_us > 0 {
                                "http/1.1".into()
                            } else {
                                String::new()
                            },
                            tls_version: m.tls_version.unwrap_or_default().to_owned(),
                            cipher: tls
                                .as_ref()
                                .and_then(|t| t.cipher.clone())
                                .unwrap_or_default(),
                        });
                    }
                    if let Some(i) = info {
                        // Look up the socket right after the handshake finishes, before hyper
                        // multiplexes streams onto it, so concurrent HTTP/2 streams share one
                        // seeded `TcpProbe` without racing on `/proc/self/fd`.
                        let sock = if shared.tcp_info {
                            tcpinfo::find(i.local_addr(), i.remote_addr()).map_or(
                                Sock::Missing,
                                |sid| Sock::Found(Arc::new(TcpProbe::new_seeded(sid, false))),
                            )
                        } else {
                            Sock::Missing
                        };
                        if let Ok(mut c) = shared.conns.lock() {
                            c.seq += 1;
                            let id = c.seq;
                            if c.by_addr.len() >= MAX_TRACKED_CONNS {
                                let keep_after = id.saturating_sub(MAX_TRACKED_CONNS as u64 / 2);
                                c.by_addr.retain(|_, e| {
                                    e.dial.is_some()
                                        || e.active_streams.load(Ordering::Relaxed) > 0
                                        || e.last_seq >= keep_after
                                });
                            }
                            c.by_addr.insert(
                                (i.local_addr(), i.remote_addr()),
                                ConnEntry {
                                    id,
                                    dial: Some(Dialed {
                                        dns_us,
                                        tcp_us,
                                        proxy_us,
                                        tls_us,
                                        end,
                                        tls,
                                    }),
                                    dial_end: Some(end),
                                    sock,
                                    active_streams: Arc::new(AtomicU32::new(0)),
                                    last_seq: id,
                                },
                            );
                        }
                    }
                }
                Err(_) => {
                    if let Some(r) = &shared.recorder {
                        r.record_dial(DialRecord {
                            ok: false,
                            proxy: None,
                            dns_us,
                            tcp_us,
                            proxy_us: 0,
                            tls_us,
                            remote: String::new(),
                            alpn: String::new(),
                            tls_version: String::new(),
                            cipher: String::new(),
                        });
                    }
                }
            }
            res
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The transport

/// OpenDAL's default reqwest client, observed request by request.
#[derive(Clone)]
pub struct FidelityTransport {
    client: reqwest::Client,
    shared: Arc<Shared>,
    limiter: Option<Arc<Semaphore>>,
}

impl FidelityTransport {
    pub fn new(
        cfg: TransportConfig,
        https: bool,
        recorder: Option<Arc<Recorder>>,
    ) -> anyhow::Result<Self> {
        let tcp_info = cfg.tcp_info && recorder.is_some();
        let proxy = match &cfg.proxy {
            Some(Some(p)) => Some(super::ProxyConfig::parse(p, "")?),
            Some(None) => None,
            None => super::ProxyConfig::from_env(https),
        };
        let proxy_addrs = proxy
            .as_ref()
            .and_then(|p| {
                use std::net::ToSocketAddrs;
                (p.host.as_str(), p.port).to_socket_addrs().ok()
            })
            .map(|a| a.collect())
            .unwrap_or_default();
        let shared = Arc::new(Shared {
            recorder,
            conns: Mutex::new(Conns::default()),
            proxy: proxy.map(|p| p.display()),
            proxy_addrs,
            tcp_info,
        });
        // OpenDAL builds reqwest without the `http2` feature, so its default is HTTP/1.1.
        let alpn: Vec<Vec<u8>> = match cfg.http {
            HttpVersion::Auto | HttpVersion::H1 => vec![b"http/1.1".to_vec()],
            HttpVersion::H2 => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        };
        let b = client_builder(&cfg, https, alpn, shared.clone())?;
        Ok(Self {
            client: b.build()?,
            shared,
            limiter: cfg.max_inflight.map(|n| Arc::new(Semaphore::new(n.max(1)))),
        })
    }
}

/// OpenDAL's default client is `reqwest::Client::new()` (opendal-http-transport-reqwest): no
/// connect timeout, unlimited idle connections per host, 90 s idle timeout, 15 s TCP
/// keep-alive. Fidelity mode keeps every one of those defaults. It only adds what observing
/// requires or what a knob asks for:
///
/// - timed DNS (the same `getaddrinfo` as reqwest's default resolver), a connector layer that
///   timestamps dials, and a rustls config built like reqwest's rustls backend (aws-lc-rs
///   provider, platform verifier) with a session store that timestamps the ClientHello;
/// - `http1_only()` for `http_version = auto | h1`: OpenDAL builds reqwest without `http2`, so its
///   client speaks HTTP/1.1 (oprobe's reqwest has `http2` compiled in for the `h2` knob);
/// - `keep_alive = false` → `pool_max_idle_per_host(0)`; `--proxy` → an explicit proxy setting.
fn client_builder(
    cfg: &TransportConfig,
    https: bool,
    alpn: Vec<Vec<u8>>,
    shared: Arc<Shared>,
) -> anyhow::Result<reqwest::ClientBuilder> {
    let mut b = reqwest::Client::builder()
        .tls_backend_preconfigured(rustls_config(cfg, alpn)?)
        .dns_resolver(Arc::new(TimedResolver))
        .connector_layer(DialLayer(shared));
    if !cfg.keep_alive {
        b = b.pool_max_idle_per_host(0);
    }
    b = match cfg.http {
        HttpVersion::Auto | HttpVersion::H1 => b.http1_only(),
        HttpVersion::H2 if !https => b.http2_prior_knowledge(),
        HttpVersion::H2 => b,
    };
    b = match &cfg.proxy {
        None => b,
        Some(None) => b.no_proxy(),
        Some(Some(p)) => b.proxy(reqwest::Proxy::all(p.as_str())?),
    };
    Ok(b)
}

impl FidelityTransport {
    /// The underlying reqwest client (for benchmarks comparing against an untraced client).
    #[doc(hidden)]
    pub fn reqwest_client(&self) -> reqwest::Client {
        self.client.clone()
    }
}

const REQUEST_ID_HEADERS: [&str; 7] = [
    "x-amz-request-id",
    "x-oss-request-id",
    "x-cos-request-id",
    "x-ms-request-id",
    "x-guploader-uploadid",
    "x-bz-request-id",
    "cf-ray",
];

/// OpenDAL's classification of reqwest errors (opendal-http-transport-reqwest).
fn is_temporary_error(err: &reqwest::Error) -> bool {
    err.is_request() || err.is_body() || err.is_decode()
}

/// Upload frame size: hyper polls the next frame only when it can write, so with bounded
/// frames the last one is handed over once the rest is on its way (within one frame plus the
/// socket send buffer). OpenDAL hands a contiguous body over as a single chunk, which would
/// otherwise mark "last byte sent" before any byte was written.
const SEND_FRAME: usize = 64 * 1024;

/// Request body that notes when its last byte was handed to the connection.
struct TimedBody {
    buf: Buffer,
    /// The rest of the chunk being framed.
    pending: Bytes,
    start: Instant,
    end_us: Arc<AtomicU64>,
    notify: Option<Arc<tokio::sync::Notify>>,
}

impl TimedBody {
    fn remaining(&self) -> usize {
        self.pending.len() + self.buf.len()
    }
}

impl http_body::Body for TimedBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<http_body::Frame<Bytes>, Self::Error>>> {
        let this = &mut *self;
        while this.pending.is_empty() {
            match this.buf.next() {
                Some(b) => this.pending = b,
                None => return Poll::Ready(None),
            }
        }
        let n = this.pending.len().min(SEND_FRAME);
        let frame = this.pending.split_to(n);
        if this.remaining() == 0 {
            let us = (this.start.elapsed().as_micros() as u64).max(1);
            this.end_us.store(us, Ordering::Release);
            if let Some(n) = &this.notify {
                n.notify_waiters();
            }
        }
        Poll::Ready(Some(Ok(http_body::Frame::data(frame))))
    }

    fn is_end_stream(&self) -> bool {
        self.remaining() == 0
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.remaining() as u64)
    }
}

/// Emits a record for a request whose future was dropped before it produced a response
/// (OpenDAL's timeout layer, a tail-cut, a cancelled operation): otherwise the slowest
/// requests would vanish from every statistic.
pub(super) struct CancelGuard {
    armed: bool,
    api: crate::Api,
    service_op: Option<&'static str>,
    method: String,
    bytes_up: u64,
    start_offset_us: u64,
    t_start: Instant,
    scope: Option<Arc<OpScope>>,
    recorder: Option<Arc<Recorder>>,
}

impl CancelGuard {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        api: crate::Api,
        service_op: Option<&'static str>,
        method: String,
        bytes_up: u64,
        start_offset_us: u64,
        t_start: Instant,
        scope: Option<Arc<OpScope>>,
        recorder: Option<Arc<Recorder>>,
    ) -> Self {
        Self {
            armed: true,
            api,
            service_op,
            method,
            bytes_up,
            start_offset_us,
            t_start,
            scope,
            recorder,
        }
    }

    /// The request reached a point where it records itself.
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let record = RequestRecord {
            operation: None,
            api: self.api,
            method: std::mem::take(&mut self.method),
            status: 0,
            protocol: Protocol::Http1,
            reused: false,
            conn_id: 0,
            start_offset_us: self.start_offset_us,
            timing: RequestTiming {
                total_us: self.t_start.elapsed().as_micros() as u64,
                ..Default::default()
            },
            bytes_up: self.bytes_up,
            bytes_down: 0,
            request_id: None,
            retry_after_ms: None,
            error: Some("cancelled before a response (timeout or abandoned)".into()),
            remote: None,
            service_op: self.service_op,
            tls: None,
            tcp: None,
            detail: Some(Box::new(crate::rows::ReqDetail {
                aborted: true,
                temporary: Some(true),
                ..Default::default()
            })),
        };
        Finisher::emit(record, &self.scope, &self.recorder);
    }
}

struct Finisher {
    record: RequestRecord,
    /// The connection (local, remote), for its TCP statistics at request end.
    conn: Option<(SocketAddr, SocketAddr)>,
    /// The connection's socket as known when the response arrived (no second table lookup).
    sock: Sock,
    shared: Arc<Shared>,
    t_start: Instant,
    t_headers: Instant,
    stall: Duration,
    last_yield: Option<Instant>,
    scope: Option<Arc<OpScope>>,
    recorder: Option<Arc<Recorder>>,
    _inflight: InflightGuard,
    _permit: Option<OwnedSemaphorePermit>,
    _stream: StreamGuard,
    /// The first bytes of an error response (its provider error code).
    sniff: Option<super::signals::ErrorSniffer>,
}

impl Finisher {
    fn emit(record: RequestRecord, scope: &Option<Arc<OpScope>>, rec: &Option<Arc<Recorder>>) {
        match scope {
            Some(s) => s.push_request(record),
            None => {
                if let Some(r) = rec {
                    r.record_unscoped_request(&record);
                }
            }
        }
    }

    fn finish(mut self, error: Option<String>) {
        let now = Instant::now();
        if let Some(t) = self.last_yield.take() {
            self.stall += now.saturating_duration_since(t);
        }
        let body_total = now.saturating_duration_since(self.t_headers);
        let stall = self.stall.min(body_total);
        let t = &mut self.record.timing;
        t.stall_us = stall.as_micros() as u64;
        t.body_us = (body_total - stall).as_micros() as u64;
        t.total_us = now.saturating_duration_since(self.t_start).as_micros() as u64;
        if error.is_some() && self.record.error.is_none() {
            self.record.error = error;
        }
        self.record.detail = super::signals::finish(self.record.detail.take(), self.sniff.as_ref());
        // After the clock stopped: the query is not part of the request's time.
        if let Some(addr) = self.conn {
            let sock = std::mem::take(&mut self.sock);
            if let Some((row, retrans)) = self.shared.tcp_info(addr, sock) {
                self.record.tcp = Some(Box::new(row));
                if let Some(n) = retrans.filter(|n| *n > 0) {
                    self.record
                        .detail
                        .get_or_insert_with(Default::default)
                        .retrans_count = Some(n);
                }
            }
        }
        Self::emit(self.record, &self.scope, &self.recorder);
    }
}

struct TimedResponseBody<S> {
    inner: Pin<Box<S>>,
    fin: Option<Finisher>,
    uri: String,
    /// Declared body length, to tell a body read to its end from one dropped part-way.
    expected: Option<u64>,
}

impl<S: Stream<Item = reqwest::Result<Bytes>>> Stream for TimedResponseBody<S> {
    type Item = Result<Buffer>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(f) = this.fin.as_mut()
            && let Some(t) = f.last_yield.take()
        {
            f.stall += t.elapsed();
        }
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    if let Some(f) = this.fin.take() {
                        f.finish(None);
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Ok(b))) => {
                    if b.is_empty() {
                        continue;
                    }
                    if let Some(f) = this.fin.as_mut() {
                        f.record.bytes_down += b.len() as u64;
                        f.last_yield = Some(Instant::now());
                        if let Some(s) = f.sniff.as_mut() {
                            s.push(&b);
                        }
                    }
                    return Poll::Ready(Some(Ok(Buffer::from(b))));
                }
                Poll::Ready(Some(Err(err))) => {
                    let temporary = is_temporary_error(&err);
                    let err = err.without_url();
                    let msg = format!("response body: {err}");
                    if let Some(f) = this.fin.take() {
                        f.finish(Some(msg));
                    }
                    return Poll::Ready(Some(Err(Error::new(
                        ErrorKind::Unexpected,
                        "read data from http response",
                    )
                    .with_operation("reqwest::fetch")
                    .with_context("url", this.uri.clone())
                    .with_temporary(temporary)
                    .set_source(err))));
                }
            }
        }
    }
}

impl<S> Drop for TimedResponseBody<S> {
    fn drop(&mut self) {
        if let Some(mut f) = self.fin.take() {
            let got = f.record.bytes_down;
            let short = self.expected.filter(|&n| got < n);
            if short.is_some() {
                let d = f.record.detail.get_or_insert_with(Default::default);
                d.aborted = true;
                d.temporary = Some(true);
            }
            f.finish(short.map(|n| format!("response body dropped after {got} of {n} bytes")));
        }
    }
}

impl HttpTransport for FidelityTransport {
    async fn fetch(&self, req: Request<Buffer>) -> Result<Response<HttpBody>> {
        let now = Instant::now();
        let scope = OpScope::current();
        // Include any time spent parked on an outer concurrency gate (e.g. OpenDAL's
        // `ConcurrentLimitLayer::with_http_concurrent_limit`) before reaching the transport.
        let t_start = scope
            .as_ref()
            .and_then(|s| s.take_pre_fetch_park())
            .map_or(now, |p| p.min(now));
        let recorder = self.shared.recorder.clone();

        // --- identical to opendal-http-transport-reqwest ---------------------------------
        let uri = req.uri().clone();
        let is_head = req.method() == Method::HEAD;
        let (mut parts, body) = req.into_parts();
        let original = uri.to_string();
        let target = parts
            .extensions
            .get::<HttpRedirect>()
            .map(|redirect| redirect.uri().original_uri())
            .unwrap_or(&original);
        let url = reqwest::Url::parse(target).map_err(|err| {
            Error::new(ErrorKind::Unexpected, "request url is invalid")
                .with_operation("reqwest::fetch")
                .with_context("url", uri.to_string())
                .set_source(err)
        })?;
        if parts.extensions.get::<HttpRedirect>().is_some() {
            let original = reqwest::Url::parse(&uri.to_string()).map_err(|err| {
                Error::new(ErrorKind::Unexpected, "original request url is invalid").set_source(err)
            })?;
            if original.origin() != url.origin() {
                for name in [
                    "authorization",
                    "proxy-authorization",
                    "cookie",
                    "cookie2",
                    "www-authenticate",
                ] {
                    parts.headers.insert(
                        http::header::HeaderName::from_static(name),
                        http::HeaderValue::from_static(""),
                    );
                }
            }
        }
        // ---------------------------------------------------------------------------------

        let method = parts.method.clone();
        // The exact API the service named (OpenDAL `ServiceOperation`); the URL heuristic only
        // when the service attached none.
        let (api, service_op) = crate::Api::resolve(
            parts.extensions.get::<ServiceOperation>().map(|o| o.0),
            &method,
            url.path(),
            url.query(),
        );
        let bytes_up = body.len() as u64;
        // What the service asked for: the byte range's length (over-read, range sizes) and the
        // OpenDAL operation (`read`, `write`, …: the `operation` label of exported metrics).
        let range_bytes = parts
            .headers
            .get(http::header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(super::range_len);
        let operation = parts
            .extensions
            .get::<opendal_core::raw::Operation>()
            .map(|o| o.into_static());
        let start_offset_us = scope
            .as_ref()
            .map(|s| t_start.saturating_duration_since(s.started).as_micros() as u64)
            .unwrap_or(0);
        let mut cancel_guard = CancelGuard::new(
            api,
            service_op,
            method.to_string(),
            bytes_up,
            start_offset_us,
            t_start,
            scope.clone(),
            recorder.clone(),
        );
        let mut record = RequestRecord {
            operation,
            api,
            method: method.to_string(),
            status: 0,
            protocol: Protocol::Http1,
            reused: true,
            conn_id: 0,
            start_offset_us,
            timing: RequestTiming::default(),
            bytes_up,
            bytes_down: 0,
            request_id: None,
            retry_after_ms: None,
            error: None,
            remote: None,
            service_op,
            tls: None,
            tcp: None,
            detail: None,
        };

        let permit = match &self.limiter {
            Some(sem) => Some(
                sem.clone()
                    .acquire_owned()
                    .instrument_await("http::wait_inflight_permit")
                    .await
                    .expect("never closed"),
            ),
            None => None,
        };
        let inflight = InflightGuard::start(recorder.clone());
        let t_ready = Instant::now();
        record.timing.queue_us = t_ready.saturating_duration_since(t_start).as_micros() as u64;

        let end_us = Arc::new(AtomicU64::new(0));
        let body_notify = (bytes_up > 0 && self.shared.tcp_info && tcpinfo::SUPPORTED)
            .then(|| Arc::new(tokio::sync::Notify::new()));
        let mut rb = self
            .client
            .request(parts.method, url)
            .headers(parts.headers)
            .version(parts.version);
        if !body.is_empty() {
            rb = rb.body(reqwest::Body::wrap(TimedBody {
                buf: body,
                pending: Bytes::new(),
                start: t_start,
                end_us: end_us.clone(),
                notify: body_notify.clone(),
            }));
        }

        // Static span name: no allocation per request. For uploads on platforms with kernel TCP
        // inspection, watch the socket's send buffer drain once the last frame enters the kernel
        // so `send_us` / `upload_acked_us` reflect peer ACK rather than socket-buffer copy.
        let acked_us = Arc::new(AtomicU64::new(0));
        let sent = if let Some(notify) = body_notify {
            let shared = self.shared.clone();
            let end_ref = end_us.clone();
            let ack_ref = acked_us.clone();
            let watch = async move {
                notify.notified().await;
                let Some(probe) = shared.single_probe() else {
                    return;
                };
                // Only track kernel send-queue drain (`notsent_bytes > 0`); checking `unacked`
                // would include the peer's 40 ms TCP Delayed-ACK timer inside `send_us` when a
                // straggler server delays its HTTP response headers.
                if !probe.peek().is_some_and(|r| r.notsent_bytes > 0) {
                    return;
                }
                let mut wait = Duration::from_micros(250);
                loop {
                    tokio::time::sleep(wait).await;
                    if !probe.peek().is_some_and(|r| r.notsent_bytes > 0) {
                        let us = t_start.elapsed().as_micros() as u64;
                        if us > end_ref.load(Ordering::Acquire) {
                            ack_ref.store(us.max(1), Ordering::Release);
                        }
                        return;
                    }
                    wait = (wait * 2).min(Duration::from_millis(2));
                }
            };
            let send_fut = rb
                .send()
                .instrument_await(service_op.unwrap_or_else(|| api.label()));
            tokio::pin!(watch);
            tokio::pin!(send_fut);
            let mut watch_done = false;
            loop {
                tokio::select! {
                    biased;
                    res = &mut send_fut => break res,
                    _ = &mut watch, if !watch_done => {
                        watch_done = true;
                    }
                }
            }
        } else {
            rb.send()
                .instrument_await(service_op.unwrap_or_else(|| api.label()))
                .await
        };
        let t_headers = Instant::now();
        cancel_guard.disarm();
        let mut resp = match sent {
            Ok(r) => r,
            Err(err) => {
                let temporary = is_temporary_error(&err);
                // Never the URL: a presigned or SAS query (`?sig=`) would land in the trace.
                let err = err.without_url();
                let msg = format!("send http request: {err}");
                record.error = Some(msg);
                // Whether a pooled connection was involved is unknown; a failed connect is
                // by far the common case, and counting it as warm would inflate reuse.
                record.reused = false;
                record.timing.ttfb_us =
                    t_headers.saturating_duration_since(t_ready).as_micros() as u64;
                record.timing.total_us =
                    t_headers.saturating_duration_since(t_start).as_micros() as u64;
                Finisher::emit(record, &scope, &recorder);
                return Err(Error::new(ErrorKind::Unexpected, "send http request")
                    .with_operation("reqwest::send")
                    .with_context("url", uri.to_string())
                    .with_temporary(temporary)
                    .set_source(err));
            }
        };

        // Connection identity: the first request seen on a local address is the cold one and
        // carries that connection's dial.
        let addr = resp
            .extensions()
            .get::<HttpInfo>()
            .map(|i| (i.local_addr(), i.remote_addr()));
        let mut dial = None;
        let mut dial_end = None;
        let mut sock = Sock::Missing;
        let mut stream_guard = StreamGuard(None);
        let mut open_streams = 0u32;
        record.remote = addr.map(|(_, remote)| remote);
        if let (Some(addr), Ok(mut c)) = (addr, self.shared.conns.lock()) {
            let c = &mut *c;
            c.seq += 1;
            let seq = c.seq;
            let entry = c.by_addr.entry(addr).or_insert_with(|| ConnEntry {
                id: seq,
                active_streams: Arc::new(AtomicU32::new(0)),
                last_seq: seq,
                ..Default::default()
            });
            entry.last_seq = seq;
            record.conn_id = entry.id;
            dial = entry.dial.take();
            dial_end = entry.dial_end;
            sock = entry.sock.clone();
            open_streams = entry.active_streams.fetch_add(1, Ordering::AcqRel) + 1;
            stream_guard = StreamGuard(Some(entry.active_streams.clone()));
        }
        record.protocol = if resp.version() == http::Version::HTTP_2 {
            Protocol::Http2
        } else {
            Protocol::Http1
        };
        // Time on the wire starts when the connection was ready.
        let mut wire_from = t_ready;
        let mut waited_dial_us = None;
        match dial {
            // The connection was dialed while this request waited: its phases are this
            // request's.
            Some(d) if d.end >= t_ready => {
                record.reused = false;
                record.timing.dns_us = d.dns_us;
                record.timing.tcp_us = d.tcp_us;
                record.timing.proxy_us = d.proxy_us;
                record.timing.tls_us = d.tls_us;
                record.tls = d.tls.map(Box::new);
                if d.end <= t_headers {
                    wire_from = d.end;
                }
            }
            // Finished before this request was even ready: a dial that lost hyper's race
            // against an idle connection and went to the pool. Count the connection, but this
            // request found it ready (warm) and did not wait for it.
            Some(d) => {
                if let Some(r) = &recorder {
                    r.record_unclaimed_dial(d.dns_us, d.tcp_us, d.proxy_us, d.tls_us);
                }
            }
            None => {
                // Concurrent HTTP/2 stream that waited while another request dialed this conn.
                if let Some(d_end) = dial_end
                    && d_end > t_ready
                    && d_end <= t_headers
                {
                    waited_dial_us = Some(d_end.saturating_duration_since(t_ready).as_micros() as u64);
                }
            }
        }
        let buffered_at = {
            let end = end_us.load(Ordering::Acquire);
            let at = t_start + Duration::from_micros(end);
            if end > 0 && at >= wire_from && at <= t_headers {
                at
            } else {
                wire_from
            }
        };
        let ack_at = {
            let ack = acked_us.load(Ordering::Acquire);
            let at = t_start + Duration::from_micros(ack);
            if ack > 0 && at >= buffered_at && at <= t_headers {
                Some(at)
            } else {
                None
            }
        };
        let sent_at = ack_at.unwrap_or(buffered_at);
        record.timing.send_us = sent_at.saturating_duration_since(wire_from).as_micros() as u64;
        record.timing.ttfb_us = t_headers.saturating_duration_since(sent_at).as_micros() as u64;
        record.status = resp.status().as_u16();
        let header = |n: &str| {
            resp.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        record.request_id = REQUEST_ID_HEADERS.iter().find_map(|h| header(h));
        let mut detail = super::signals::from_headers(resp.headers(), record.status);
        detail.range_bytes = range_bytes;
        if record.protocol == Protocol::Http2 && open_streams > 0 {
            detail.streams_count = Some(open_streams);
        }
        if let Some(w) = waited_dial_us.filter(|&w| w > 0) {
            detail.dial_wait_us = Some(w);
        }
        if bytes_up > 0 && ack_at.is_some() {
            detail.send_buffered_us =
                Some(buffered_at.saturating_duration_since(wire_from).as_micros() as u64);
            detail.upload_acked_us =
                Some(sent_at.saturating_duration_since(t_start).as_micros() as u64);
        }
        record.detail = Some(Box::new(detail));
        let sniff = (record.status >= 400).then(super::signals::ErrorSniffer::default);
        if record.status >= 400 {
            record.retry_after_ms = super::signals::retry_after_ms(resp.headers());
        }

        // --- identical to opendal-http-transport-reqwest ---------------------------------
        let content_length = if is_head || parse_content_encoding(resp.headers())?.is_some() {
            None
        } else {
            parse_content_length(resp.headers())?
        };
        let mut hr = Response::builder()
            .status(resp.status())
            .extension(uri.clone());
        if let Ok(target) = resp.url().as_str().parse::<http::Uri>()
            && target != uri
        {
            let redirect = parts
                .extensions
                .get::<HttpRedirect>()
                .filter(|redirect| redirect.uri().original_uri() == resp.url().as_str())
                .cloned()
                .unwrap_or_else(|| HttpRedirect::new(target));
            hr = hr.extension(redirect);
        }
        hr = hr.version(resp.version());
        std::mem::swap(hr.headers_mut().expect("fresh builder"), resp.headers_mut());
        // ---------------------------------------------------------------------------------

        let fin = Finisher {
            record,
            conn: addr,
            sock,
            shared: self.shared.clone(),
            t_start,
            t_headers,
            stall: Duration::ZERO,
            // Until the consumer first polls the body, the bytes wait on it, not the network.
            last_yield: Some(t_headers),
            scope,
            recorder,
            _permit: permit,
            _inflight: inflight,
            _stream: stream_guard,
            sniff,
        };
        let stream = TimedResponseBody {
            inner: Box::pin(resp.bytes_stream()),
            fin: Some(fin),
            uri: uri.to_string(),
            expected: content_length,
        };
        Ok(hr
            .body(HttpBody::new(stream, content_length))
            .expect("response must build"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            recorder: None,
            conns: Mutex::new(Conns::default()),
            proxy: None,
            proxy_addrs: Vec::new(),
            tcp_info: false,
        })
    }

    /// Fidelity mode measures OpenDAL's default client (`reqwest::Client::new()`): none of the
    /// microscope pool's limits (10 s connect timeout, 256 idle connections per host) leak in.
    #[test]
    fn fidelity_client_keeps_opendal_reqwest_defaults() {
        let cfg = TransportConfig {
            proxy: Some(None),
            ..TransportConfig::default()
        };
        let alpn = vec![b"http/1.1".to_vec()];
        let ours = format!("{:?}", client_builder(&cfg, true, alpn, shared()).unwrap());
        let opendal = format!("{:?}", reqwest::Client::builder().http1_only().no_proxy());
        assert!(!ours.contains("connect_timeout"), "{ours}");
        assert!(!ours.contains("timeout"), "{ours}");
        assert!(ours.contains("http1_only"), "{ours}");
        // Everything reqwest reports about its configuration matches OpenDAL's HTTP/1.1 client.
        let strip_tls = |s: &str| s.split(", tls").next().unwrap_or(s).to_owned();
        assert_eq!(strip_tls(&ours), strip_tls(&opendal));
    }

    struct Errors(Mutex<Vec<String>>);

    impl crate::Exporter for Errors {
        fn on_request(&self, r: &RequestRecord) {
            self.0.lock().unwrap().extend(r.error.clone());
        }
    }

    /// A failed request records why, never the URL: an Azure SAS or presigned query carries a
    /// signature (`?sig=`), and the trace is a file people share.
    #[tokio::test]
    async fn a_failed_request_never_records_its_url() {
        let recorder = Recorder::always_on(64);
        let errors = Arc::new(Errors(Mutex::new(Vec::new())));
        recorder.set_exporter(errors.clone());
        let cfg = TransportConfig {
            proxy: Some(None),
            ..TransportConfig::default()
        };
        let t = FidelityTransport::new(cfg, false, Some(recorder.clone())).unwrap();
        // Nothing listens on port 1: the connect fails.
        let req = Request::get("http://127.0.0.1:1/c/k?sv=2024&sig=SECRET")
            .body(Buffer::new())
            .unwrap();
        assert!(t.fetch(req).await.is_err());
        let errors = errors.0.lock().unwrap().clone();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("send http request"), "{errors:?}");
        let rows = format!("{:?}", recorder.take_trace());
        for text in errors.iter().chain([&rows]) {
            assert!(!text.contains("SECRET"), "{text}");
            assert!(!text.contains("127.0.0.1:1/"), "{text}");
        }
    }
}
