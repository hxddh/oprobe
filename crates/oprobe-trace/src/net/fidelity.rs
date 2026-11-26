//! Production-fidelity transport: OpenDAL's own HTTP client (reqwest 0.13 + rustls/aws-lc-rs,
//! exactly as `opendal-http-transport-reqwest` builds it), observed only through reqwest's
//! public extension points:
//!
//! - a timed DNS resolver (`ClientBuilder::dns_resolver`, same `getaddrinfo` path as reqwest's
//!   default resolver),
//! - a connector layer (`ClientBuilder::connector_layer`) that times every dial and learns the
//!   connection's local/remote address,
//! - a delegating TLS session store that timestamps the ClientHello, splitting TCP from TLS,
//! - the response's `HttpInfo` (local and remote address = connection identity), which tells
//!   the request which connection served it: the request that started a connection's dial is
//!   its cold request and carries the dial ([`DialClaim`]),
//! - the same rustls crypto provider and certificate verifier with delegating wrappers that
//!   report the negotiated cipher suite, key-exchange group, HelloRetryRequests and whether the
//!   handshake was resumed (`net::tlsinfo`),
//! - the kernel's TCP statistics of the connection at request end (`net::tcpinfo`: the socket
//!   is found by its addresses when the connection is dialed, off the runtime's threads, and
//!   only queried, never owned).
//!
//! What this transport cannot see: time waiting inside reqwest's pool for an
//! idle connection or HTTP/2 stream is part of TTFB.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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

use super::tcpinfo::{self, ConnAddr, SockFinder, TcpProbe};
use super::tls::{InsecureCertVerifier, crypto_provider};
use super::tlsinfo::{HandshakeMarks, ObservedVerifier, TlsEvent, observed_provider};
use super::{HttpVersion, TransportConfig};
use crate::record::{Protocol, RequestRecord, RequestTiming, ServiceLabels};
use crate::recorder::DialRecord;
use crate::rows::TlsRow;
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

/// The dials one request started, until the request is answered. hyper dials while a request
/// waits for a connection, but the connection does not have to serve that request: on HTTP/2
/// other requests share it and may be answered first, and on HTTP/1.1 an idle connection may
/// turn up before the dial ends. The dial belongs to the request that started it:
///
/// - answered on the connection it dialed, that request is the connection's cold request and
///   carries the dial's DNS, TCP, proxy and TLS time;
/// - answered elsewhere, failed or dropped, its dials are connections nobody waited for
///   ([`Recorder::record_unclaimed_dial`]): counted, with their handshake phases, and warm for
///   whoever uses them;
/// - a request answered on a connection another request dialed never takes that dial, whoever
///   is answered first: it waited for the connection (`dial_wait_us`) or found it warm.
#[derive(Debug, Default)]
struct DialClaim {
    state: Mutex<ClaimState>,
}

#[derive(Debug, Default)]
struct ClaimState {
    /// The request was answered (or gave up): a dial finishing from now on is unclaimed.
    settled: bool,
    /// The connections its dials made, each still holding its [`Dialed`].
    dialed: Vec<ConnAddr>,
}

tokio::task_local! {
    /// The claim of the request being sent (while reqwest's `send` future is polled, which is
    /// where hyper calls the connector).
    static CLAIM: Arc<DialClaim>;
}

/// Settles a request's dials when its `fetch` ends without an answer (an error, or the future
/// dropped by a timeout); a no-op after [`Shared::settle`].
struct ClaimGuard {
    claim: Arc<DialClaim>,
    shared: Arc<Shared>,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        self.shared.settle(&self.claim, None);
    }
}

/// A finished dial waiting for the request that started it.
#[derive(Clone, Debug)]
struct Dialed {
    dns_us: u64,
    tcp_us: u64,
    proxy_us: u64,
    tls_us: u64,
    end: Instant,
    tls: Option<TlsRow>,
    /// The request that started it.
    by: Arc<DialClaim>,
}

/// Where the kernel statistics of a connection come from.
#[derive(Clone, Debug, Default)]
enum Sock {
    /// A connection this table did not see dialed (its entry was evicted): looked up by the
    /// next request on it.
    #[default]
    Unknown,
    /// Not found or not supported: never looked up again.
    Missing,
    Found(Arc<TcpProbe>),
    /// Its socket is being looked up off the runtime's threads ([`Shared::find_socket`]).
    Finding(Lookup),
}

/// The answer of one lookup of a connection's socket: unset while it runs, then the probe
/// (`None`: not found). Nothing ever waits for it: a request reads it when its response
/// headers arrive and again when it ends.
type Lookup = Arc<OnceLock<Option<Arc<TcpProbe>>>>;

struct ConnEntry {
    id: u64,
    dial: Option<Dialed>,
    dial_end: Option<Instant>,
    sock: Sock,
    active_streams: Arc<AtomicU32>,
    recent_ends: [Option<Instant>; 64],
    recent_idx: usize,
    last_seq: u64,
}

impl Default for ConnEntry {
    fn default() -> Self {
        Self {
            id: 0,
            dial: None,
            dial_end: None,
            sock: Sock::Unknown,
            active_streams: Arc::new(AtomicU32::new(0)),
            recent_ends: [None; 64],
            recent_idx: 0,
            last_seq: 0,
        }
    }
}

/// What a dial measured.
struct DialTimes {
    dns_us: u64,
    tcp_us: u64,
    proxy_us: u64,
    tls_us: u64,
    end: Instant,
    tls: Option<TlsRow>,
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
    /// Counts the table's events (dials and answered requests): what was used last.
    seq: u64,
    /// The connections seen so far: a connection's id is its rank among them, so the ids of
    /// a client that dialed 25 connections are 1 to 25.
    ids: u64,
    /// Connection identity (local, remote address) -> (id, dial awaiting its request).
    /// The local address alone is not unique: the kernel reuses a local port for connections
    /// to different peers, and object stores answer from many addresses.
    by_addr: HashMap<(SocketAddr, SocketAddr), ConnEntry>,
}

struct Shared {
    recorder: Option<Arc<Recorder>>,
    conns: Mutex<Conns>,
    /// Finds the sockets of new connections off the runtime's threads.
    finder: Arc<SockFinder>,
    /// Concurrent uploads currently sending body frames (`> 1` disables shared socket send-queue attribution).
    active_uploads: AtomicU32,
    /// Configured egress proxy (display form) and its addresses: a dial whose peer is one of
    /// them went through the proxy (reqwest applies NO_PROXY itself).
    proxy: Option<String>,
    proxy_addrs: Vec<SocketAddr>,
    /// Query `TCP_INFO` at request end.
    tcp_info: bool,
}

struct UploadGuard(Option<Arc<Shared>>);

impl Drop for UploadGuard {
    fn drop(&mut self) {
        if let Some(s) = &self.0 {
            s.active_uploads.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Shared {
    /// Start looking up the kernel probe of connection `addr` (see `net::tcpinfo` for why
    /// querying it is safe): fresh from its handshake, or (`warm`) in use before this table
    /// knew it, so its bytes so far are not counted again. The lookup runs in a task of its
    /// own, off the runtime's worker threads, and puts its answer into the returned `Lookup`
    /// and into the connection table.
    ///
    /// Neither a dial nor a request waits for it. A connection goes to hyper as soon as its
    /// handshake ends, and a request reads the answer if it is there. 9.2 awaited the lookup
    /// when the response headers arrived: with the runtime's blocking pool busy, that held
    /// the response back (and read as time the consumer stalled), and with the lookup's task
    /// lost in a runtime shutdown it never returned.
    fn find_socket(self: &Arc<Self>, addr: ConnAddr, warm: bool) -> Sock {
        if !self.tcp_info {
            return Sock::Missing;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return Sock::Missing;
        };
        let lookup = Lookup::default();
        let (shared, answer) = (self.clone(), lookup.clone());
        runtime.spawn(async move {
            let probe = shared.finder.find(addr).await;
            let probe = probe.map(|id| Arc::new(TcpProbe::new_seeded(id, warm)));
            let sock = probe.clone().map_or(Sock::Missing, Sock::Found);
            let _ = answer.set(probe);
            if let Ok(mut c) = shared.conns.lock()
                && let Some(e) = c.by_addr.get_mut(&addr)
                && matches!(&e.sock, Sock::Finding(l) if Arc::ptr_eq(l, &answer))
            {
                e.sock = sock;
            }
        });
        Sock::Finding(lookup)
    }

    /// A dial started by the request behind `claim` made connection `addr`. The dial waits in
    /// the connection table for that request's answer; when the request is already settled
    /// (or the dial was started outside any request), nobody waited for the connection.
    fn dialed(&self, claim: Option<Arc<DialClaim>>, addr: ConnAddr, times: DialTimes, sock: Sock) {
        let DialTimes {
            dns_us,
            tcp_us,
            proxy_us,
            tls_us,
            end,
            tls,
        } = times;
        // Lock order: a claim, then the connection table.
        let mut claimed = claim.as_ref().and_then(|c| c.state.lock().ok());
        let dial = match (&claim, claimed.as_deref_mut()) {
            (Some(by), Some(state)) if !state.settled => {
                state.dialed.push(addr);
                Some(Dialed {
                    dns_us,
                    tcp_us,
                    proxy_us,
                    tls_us,
                    end,
                    tls,
                    by: by.clone(),
                })
            }
            _ => {
                if let Some(r) = &self.recorder {
                    r.record_unclaimed_dial(dns_us, tcp_us, proxy_us, tls_us);
                }
                None
            }
        };
        let Ok(mut c) = self.conns.lock() else {
            return;
        };
        c.seq += 1;
        c.ids += 1;
        let (seq, id) = (c.seq, c.ids);
        if c.by_addr.len() >= MAX_TRACKED_CONNS {
            let keep_after = seq.saturating_sub(MAX_TRACKED_CONNS as u64 / 2);
            c.by_addr.retain(|_, e| {
                e.dial.is_some()
                    || e.active_streams.load(Ordering::Relaxed) > 0
                    || e.last_seq >= keep_after
            });
        }
        c.by_addr.insert(
            addr,
            ConnEntry {
                id,
                dial,
                dial_end: Some(end),
                sock,
                last_seq: seq,
                ..Default::default()
            },
        );
    }

    /// The request behind `claim` was answered on connection `used` (`None`: it got no
    /// answer). Returns the dial of `used` when this request started it; its other dials made
    /// connections nobody waited for.
    fn settle(&self, claim: &Arc<DialClaim>, used: Option<ConnAddr>) -> Option<Dialed> {
        let dialed = {
            let mut state = claim.state.lock().ok()?;
            state.settled = true;
            std::mem::take(&mut state.dialed)
        };
        if dialed.is_empty() {
            return None;
        }
        let mut own = None;
        let mut unclaimed = Vec::new();
        if let Ok(mut c) = self.conns.lock() {
            for addr in dialed {
                // The entry may be another connection by now (the 4-tuple was reused).
                let Some(dial) = c
                    .by_addr
                    .get_mut(&addr)
                    .and_then(|e| e.dial.take_if(|d| Arc::ptr_eq(&d.by, claim)))
                else {
                    continue;
                };
                if Some(addr) == used {
                    own = Some(dial);
                } else {
                    unclaimed.push(dial);
                }
            }
        }
        if let Some(r) = &self.recorder {
            for d in unclaimed {
                r.record_unclaimed_dial(d.dns_us, d.tcp_us, d.proxy_us, d.tls_us);
            }
        }
        own
    }

    /// The kernel probe of the connection the one upload in flight is being sent on, when
    /// that is unambiguous: the only connection alive, or the only one with unsent bytes in
    /// the kernel's send queue (`notsent_bytes > 0`) when idle connections exist beside it.
    /// `None` with several uploads in flight. The guess can still be wrong (a connection
    /// whose socket is not found yet is not among the candidates): the response headers say
    /// which connection it was ([`SendingStream`]).
    ///
    /// The connection table is only locked to list the probes: each one is then read with
    /// three system calls, which 9.2 made under the lock that every request takes.
    fn uploading_probe(&self) -> Option<Arc<TcpProbe>> {
        if self.active_uploads.load(Ordering::Acquire) > 1 {
            return None;
        }
        let probes: Vec<Arc<TcpProbe>> = {
            let c = self.conns.lock().ok()?;
            let found = c.by_addr.values().filter_map(|e| match &e.sock {
                Sock::Found(p) => Some(p.clone()),
                Sock::Unknown | Sock::Missing | Sock::Finding(_) => None,
            });
            found.collect()
        };
        let mut alive = 0usize;
        let mut last_alive = None;
        let mut with_unsent = None;
        for p in &probes {
            let Some(row) = p.peek() else {
                continue;
            };
            alive += 1;
            last_alive = Some(p);
            if row.notsent_bytes > 0 {
                if with_unsent.is_some() {
                    return None;
                }
                with_unsent = Some(p);
            }
        }
        with_unsent
            .or(last_alive.filter(|_| alive == 1))
            .cloned()
    }
}

/// The retransmit stream of an upload on the connection it is being sent on
/// ([`Shared::uploading_probe`]), from the moment that connection is known until the request
/// is answered or gives up: retransmits while the body is on its way are the upload's own
/// (`net::tcpinfo::RetransLedger`).
#[derive(Default)]
struct SendingStream(Mutex<Option<(Arc<TcpProbe>, u64)>>);

impl SendingStream {
    /// The upload is being sent on the connection of `probe`.
    fn open(&self, probe: &Arc<TcpProbe>) {
        if let Ok(mut s) = self.0.lock()
            && s.is_none()
        {
            *s = Some((probe.clone(), probe.open_sending_stream()));
        }
    }

    fn take(&self) -> Option<(Arc<TcpProbe>, u64)> {
        self.0.lock().ok()?.take()
    }

    /// The response headers arrived on connection `addr` (`None`: unknown): the stream, now
    /// past its send, when that is the connection it was opened on.
    fn answered(&self, addr: Option<ConnAddr>) -> Option<(Arc<TcpProbe>, u64)> {
        let (probe, stream) = self.take()?;
        if addr.is_some_and(|addr| probe.is_of(addr)) {
            probe.stream_sent(stream);
            Some((probe, stream))
        } else {
            probe.abandon_stream(stream);
            None
        }
    }

    /// The request ends without an answer (an error, a timeout): what its connection
    /// retransmitted while it sent is on its record.
    fn unanswered(&self, record: &mut RequestRecord) {
        let Some((probe, stream)) = self.take() else {
            return;
        };
        let (row, retrans) = probe.close_stream(stream);
        record.tcp = row.map(Box::new);
        if retrans > 0 {
            record
                .detail
                .get_or_insert_with(Default::default)
                .retrans_count = Some(retrans);
        }
    }
}

impl Drop for SendingStream {
    fn drop(&mut self) {
        if let Some((probe, stream)) = self.take() {
            probe.abandon_stream(stream);
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
    S::Response: Connection + Send,
    S::Error: Send,
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
        // hyper calls the connector while it polls the request that needs a connection.
        let claim = CLAIM.try_with(Arc::clone).ok();
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
            let made = match &res {
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
                    info.map(|i| {
                        let times = DialTimes {
                            dns_us,
                            tcp_us,
                            proxy_us,
                            tls_us,
                            end,
                            tls,
                        };
                        ((i.local_addr(), i.remote_addr()), times)
                    })
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
                    None
                }
            };
            if let Some((addr, times)) = made {
                // The socket is looked up off the runtime's threads, starting now; the
                // connection goes to hyper at once.
                let sock = shared.find_socket(addr, false);
                shared.dialed(claim, addr, times, sock);
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
            finder: Arc::default(),
            active_uploads: AtomicU32::new(0),
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
    /// Told when the first frame is handed over (hyper has chosen the connection) and when
    /// the last one is.
    notify: Option<Arc<tokio::sync::Notify>>,
    started: bool,
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
        let first = !std::mem::replace(&mut this.started, true);
        let last = this.remaining() == 0;
        if last {
            let us = (this.start.elapsed().as_micros() as u64).max(1);
            this.end_us.store(us, Ordering::Release);
        }
        if let Some(n) = this.notify.as_ref().filter(|_| first || last) {
            n.notify_one();
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
    /// The request as it entered the transport; `None` once it records itself.
    record: Option<RequestRecord>,
    t_start: Instant,
    t_ready: Option<Instant>,
    scope: Option<Arc<OpScope>>,
    recorder: Option<Arc<Recorder>>,
    /// The retransmit stream of the upload being sent, if its connection is known.
    sending: Option<Arc<SendingStream>>,
}

impl CancelGuard {
    pub(super) fn set_ready(&mut self, t_ready: Instant) {
        self.t_ready = Some(t_ready);
    }

    /// The request reached a point where it records itself.
    pub(super) fn disarm(&mut self) {
        self.record = None;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        let Some(mut record) = self.record.take() else {
            return;
        };
        let now = Instant::now();
        let total_us = now.saturating_duration_since(self.t_start).as_micros() as u64;
        let (queue_us, ttfb_us) = match self.t_ready {
            Some(ready) => {
                let q = ready.saturating_duration_since(self.t_start).as_micros() as u64;
                (q, total_us.saturating_sub(q))
            }
            None => (total_us, 0),
        };
        record.reused = false;
        record.timing = RequestTiming {
            queue_us,
            ttfb_us,
            total_us,
            ..Default::default()
        };
        record.error = Some("cancelled before a response (timeout or abandoned)".into());
        record.detail = Some(Box::new(crate::rows::ReqDetail {
            aborted: true,
            temporary: Some(true),
            ..Default::default()
        }));
        if let Some(sending) = &self.sending {
            sending.unanswered(&mut record);
        }
        Finisher::emit(record, &self.scope, &self.recorder);
    }
}

/// The connection a request was answered on, for what is read when the request ends.
pub(super) struct ConnUse {
    addr: ConnAddr,
    shared: Arc<Shared>,
    probe: Probing,
    _stream: StreamGuard,
}

/// What a request knows of its connection's kernel probe when its response headers arrive.
enum Probing {
    /// Not found, or not looked for.
    Missing,
    /// The probe, and this request's retransmit stream on it.
    Stream(Arc<TcpProbe>, u64),
    /// Still being looked up: the request reads the answer when it ends. If there is one by
    /// then, the request is sampled like any other, its retransmit stream running from that
    /// moment; if there is none, the request has no kernel statistics, and what the
    /// connection sent, received and retransmitted meanwhile counts with its next request.
    Finding(Lookup),
}

pub(super) struct Finisher {
    pub(super) record: RequestRecord,
    /// `None` for a request sent through a transport oprobe does not own (`net::wrapped`).
    pub(super) conn: Option<ConnUse>,
    pub(super) t_start: Instant,
    pub(super) t_headers: Instant,
    pub(super) scope: Option<Arc<OpScope>>,
    pub(super) recorder: Option<Arc<Recorder>>,
    pub(super) _inflight: InflightGuard,
    pub(super) _permit: Option<OwnedSemaphorePermit>,
    /// The first bytes of an error response (its provider error code).
    pub(super) sniff: Option<super::signals::ErrorSniffer>,
}

impl Finisher {
    pub(super) fn emit(
        record: RequestRecord,
        scope: &Option<Arc<OpScope>>,
        rec: &Option<Arc<Recorder>>,
    ) {
        match scope {
            Some(s) => s.push_request(record),
            None => {
                if let Some(r) = rec {
                    r.record_unscoped_request(&record);
                }
            }
        }
    }

    /// `stall`: how long the body waited on its consumer.
    fn finish(mut self, stall: Duration, error: Option<String>) {
        let now = Instant::now();
        let body_total = now.saturating_duration_since(self.t_headers);
        let stall = stall.min(body_total);
        let t = &mut self.record.timing;
        t.stall_us = stall.as_micros() as u64;
        t.body_us = (body_total - stall).as_micros() as u64;
        t.total_us = now.saturating_duration_since(self.t_start).as_micros() as u64;
        if error.is_some() && self.record.error.is_none() {
            self.record.error = error;
        }
        self.record.detail = super::signals::finish(self.record.detail.take(), self.sniff.as_ref());
        // After the clock stopped: the query is not part of the request's time.
        if let Some(conn) = self.conn.take() {
            if let Ok(mut c) = conn.shared.conns.lock()
                && let Some(e) = c.by_addr.get_mut(&conn.addr)
            {
                e.recent_ends[e.recent_idx] = Some(now);
                e.recent_idx = (e.recent_idx + 1) % e.recent_ends.len();
            }
            let sampled = match conn.probe {
                Probing::Stream(probe, stream) => Some((probe, stream)),
                Probing::Finding(lookup) => lookup.get().cloned().flatten().map(|probe| {
                    let stream = probe.open_stream();
                    (probe, stream)
                }),
                Probing::Missing => None,
            };
            if let Some((probe, stream)) = sampled {
                let (row, retrans) = probe.close_stream(stream);
                if let Some(row) = row {
                    // The bytes on the wire since the connection's previous sample: every
                    // request is sampled, so the deltas add up to the connection's traffic.
                    if let Some(r) = &conn.shared.recorder {
                        let (rx, tx) = probe.wire_since_last(&row);
                        r.add_wire(rx, tx);
                    }
                    self.record.tcp = Some(Box::new(row));
                }
                if retrans > 0 {
                    self.record
                        .detail
                        .get_or_insert_with(Default::default)
                        .retrans_count = Some(retrans);
                }
            }
        }
        Self::emit(self.record, &self.scope, &self.recorder);
    }
}

/// A response body that times its transfer and its consumer, and records the request when it
/// ends (read to its end, failed, or dropped part-way).
pub(super) struct TimedResponseBody<S> {
    inner: Pin<Box<S>>,
    fin: Option<Finisher>,
    /// Time the body waited on its consumer so far.
    stall: Duration,
    last_yield: Option<Instant>,
    /// Declared body length, to tell a body read to its end from one dropped part-way.
    expected: Option<u64>,
}

impl<S> TimedResponseBody<S> {
    pub(super) fn new(inner: S, fin: Finisher, expected: Option<u64>) -> Self {
        Self {
            inner: Box::pin(inner),
            // Until the consumer first polls the body, the bytes wait on it, not the network.
            last_yield: Some(fin.t_headers),
            fin: Some(fin),
            stall: Duration::ZERO,
            expected,
        }
    }

    fn finish(&mut self, error: Option<String>) {
        if let Some(f) = self.fin.take() {
            let waiting = self.last_yield.take().map_or(Duration::ZERO, |t| t.elapsed());
            f.finish(self.stall + waiting, error);
        }
    }
}

impl<S: Stream<Item = Result<Buffer>>> Stream for TimedResponseBody<S> {
    type Item = Result<Buffer>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(t) = this.last_yield.take() {
            this.stall += t.elapsed();
        }
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.finish(None);
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Ok(b))) => {
                    if b.is_empty() {
                        continue;
                    }
                    if let Some(f) = this.fin.as_mut() {
                        f.record.bytes_down += b.len() as u64;
                        this.last_yield = Some(Instant::now());
                        if let Some(s) = f.sniff.as_mut() {
                            s.push(&b.to_bytes());
                        }
                    }
                    return Poll::Ready(Some(Ok(b)));
                }
                Poll::Ready(Some(Err(err))) => {
                    // The transport's own error (reqwest's, without its URL) when there is
                    // one; the row never needs OpenDAL's context around it.
                    let cause = std::error::Error::source(&err)
                        .map_or_else(|| err.to_string(), |source| source.to_string());
                    this.finish(Some(format!("response body: {cause}")));
                    return Poll::Ready(Some(Err(err)));
                }
            }
        }
    }
}

impl<S> Drop for TimedResponseBody<S> {
    fn drop(&mut self) {
        let Some(f) = self.fin.as_mut() else {
            return;
        };
        let got = f.record.bytes_down;
        let abort_msg = match self.expected {
            Some(n) if got < n => Some(format!("response body dropped after {got} of {n} bytes")),
            None if f.record.status < 400
                && f.record.method != "HEAD"
                && !matches!(f.record.status, 100..=199 | 204 | 304) =>
            {
                Some(format!("response body dropped before EOF after {got} bytes"))
            }
            _ => None,
        };
        if abort_msg.is_some() {
            let d = f.record.detail.get_or_insert_with(Default::default);
            d.aborted = true;
            d.temporary = Some(true);
        }
        self.finish(abort_msg);
    }
}

/// What a transport notes when a request enters it.
pub(super) struct Entered {
    /// When the request was enqueued: at the outer concurrency gate when `QueueStampLayer`
    /// stamped it (around `ConcurrentLimitLayer::with_http_concurrent_limit`), else now.
    pub(super) t_start: Instant,
    pub(super) scope: Option<Arc<OpScope>>,
    pub(super) record: RequestRecord,
    pub(super) cancel: CancelGuard,
    /// The length of the byte range asked for (over-read, range sizes).
    pub(super) range_bytes: Option<u64>,
}

impl Entered {
    /// `now` is when the transport was entered; `path`, `query` and `server` (host, port) are
    /// where the request goes.
    pub(super) fn new(
        now: Instant,
        parts: &http::request::Parts,
        path: &str,
        query: Option<&str>,
        server: Option<(Arc<str>, u16)>,
        bytes_up: u64,
        recorder: &Option<Arc<Recorder>>,
    ) -> Self {
        let scope = OpScope::current();
        let t_start = parts
            .extensions
            .get::<crate::layer::EnqueuedAt>()
            .map_or(now, |e| e.0.min(now));
        // The exact API the service named (OpenDAL `ServiceOperation`); the URL heuristic only
        // when the service attached none.
        let (api, service_op) = crate::Api::resolve(
            parts.extensions.get::<ServiceOperation>().map(|o| o.0),
            &parts.method,
            path,
            query,
        );
        let range_bytes = parts
            .headers
            .get(http::header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(super::range_len);
        // The OpenDAL operation (`read`, `write`, …: the `operation` label of exported
        // metrics).
        let operation = parts
            .extensions
            .get::<opendal_core::raw::Operation>()
            .map(|o| o.into_static());
        let start_offset_us = scope
            .as_ref()
            .map(|s| t_start.saturating_duration_since(s.started).as_micros() as u64)
            .unwrap_or(0);
        let record = RequestRecord {
            operation,
            api,
            method: parts.method.to_string(),
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
            server,
            service_op,
            service: parts.extensions.get::<Arc<ServiceLabels>>().cloned(),
            tls: None,
            tcp: None,
            detail: None,
        };
        let cancel = CancelGuard {
            record: Some(record.clone()),
            t_start,
            t_ready: None,
            scope: scope.clone(),
            recorder: recorder.clone(),
            sending: None,
        };
        Self {
            t_start,
            scope,
            record,
            cancel,
            range_bytes,
        }
    }
}

/// The response headers arrived: note the status, the protocol and what the server said.
/// Returns the sniffer of an error body.
pub(super) fn answered(
    record: &mut RequestRecord,
    range_bytes: Option<u64>,
    status: http::StatusCode,
    version: http::Version,
    headers: &http::HeaderMap,
) -> Option<super::signals::ErrorSniffer> {
    record.protocol = if version == http::Version::HTTP_2 {
        Protocol::Http2
    } else {
        Protocol::Http1
    };
    record.status = status.as_u16();
    record.request_id = REQUEST_ID_HEADERS.iter().find_map(|h| {
        headers
            .get(*h)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    });
    let mut detail = super::signals::from_headers(headers, record.status);
    detail.range_bytes = range_bytes;
    record.detail = Some(Box::new(detail));
    if record.status < 400 {
        return None;
    }
    record.retry_after_ms = super::signals::retry_after_ms(headers);
    Some(super::signals::ErrorSniffer::default())
}

/// `uri` as an error context may quote it: without the signature or token of a presigned or
/// SAS URL (OpenDAL's own transport quotes the URL whole).
fn public_url(uri: &http::Uri) -> String {
    crate::util::strip_url_credentials(&uri.to_string())
}

impl HttpTransport for FidelityTransport {
    async fn fetch(&self, req: Request<Buffer>) -> Result<Response<HttpBody>> {
        let now = Instant::now();
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
                .with_context("url", public_url(&uri))
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

        let bytes_up = body.len() as u64;
        let server = url
            .host_str()
            .map(|host| (Arc::from(host), url.port_or_known_default().unwrap_or(0)));
        let Entered {
            t_start,
            scope,
            mut record,
            cancel: mut cancel_guard,
            range_bytes,
        } = Entered::new(
            now,
            &parts,
            url.path(),
            url.query(),
            server,
            bytes_up,
            &recorder,
        );
        let (api, service_op) = (record.api, record.service_op);

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
        cancel_guard.set_ready(t_ready);
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
                started: false,
            }));
        }

        // The dials hyper starts while it polls this request are this request's.
        let claim = Arc::<DialClaim>::default();
        let _settle_dials = ClaimGuard {
            claim: claim.clone(),
            shared: self.shared.clone(),
        };
        // Static span name: no allocation per request.
        let send_fut = CLAIM
            .scope(claim.clone(), rb.send())
            .instrument_await(service_op.unwrap_or_else(|| api.label()));

        // For uploads on platforms with kernel TCP inspection, watch the socket's send buffer
        // drain once the last frame enters the kernel so `send_us` / `upload_acked_us` reflect
        // send-queue drain rather than buffer copy.
        let acked_us = Arc::new(AtomicU64::new(0));
        let upload_guard = (bytes_up > 0).then(|| {
            self.shared.active_uploads.fetch_add(1, Ordering::AcqRel);
            UploadGuard(Some(self.shared.clone()))
        });
        let sent = if let Some(notify) = body_notify {
            let shared = self.shared.clone();
            let end_ref = end_us.clone();
            let ack_ref = acked_us.clone();
            let sending = Arc::<SendingStream>::default();
            cancel_guard.sending = Some(sending.clone());
            let watch = async move {
                // The first frame is handed over: hyper has chosen the connection. From now
                // on the connection's retransmits are this upload's, if it can be told.
                notify.notified().await;
                let mut probe = shared.uploading_probe();
                if end_ref.load(Ordering::Acquire) == 0 {
                    if let Some(probe) = &probe {
                        sending.open(probe);
                    }
                    // The last frame is handed over.
                    notify.notified().await;
                }
                // With bytes waiting in one send queue, the connection is told for certain.
                if probe.is_none() {
                    probe = shared.uploading_probe();
                }
                let Some(probe) = probe else {
                    return;
                };
                sending.open(&probe);
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
            send_fut.await
        };
        let t_headers = Instant::now();
        drop(upload_guard);
        cancel_guard.disarm();
        let sending = cancel_guard.sending.take();
        let mut resp = match sent {
            Ok(r) => r,
            Err(err) => {
                if let Some(sending) = &sending {
                    sending.unanswered(&mut record);
                }
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
                    .with_context("url", public_url(&uri))
                    .with_temporary(temporary)
                    .set_source(err));
            }
        };

        // Connection identity: which connection answered, and whether this request is the
        // one that dialed it.
        let addr = resp
            .extensions()
            .get::<HttpInfo>()
            .map(|i| (i.local_addr(), i.remote_addr()));
        let own_dial = self.shared.settle(&claim, addr);
        let sniff = answered(
            &mut record,
            range_bytes,
            resp.status(),
            resp.version(),
            resp.headers(),
        );
        let mut dial_end = None;
        let mut sock = Sock::Missing;
        let mut stream_guard = StreamGuard(None);
        let mut open_streams = 0u32;
        record.remote = addr.map(|(_, remote)| remote);
        if let (Some(addr), Ok(mut c)) = (addr, self.shared.conns.lock()) {
            let c = &mut *c;
            c.seq += 1;
            let seq = c.seq;
            let ids = &mut c.ids;
            let entry = c.by_addr.entry(addr).or_insert_with(|| {
                *ids += 1;
                ConnEntry {
                    id: *ids,
                    ..Default::default()
                }
            });
            entry.last_seq = seq;
            record.conn_id = entry.id;
            dial_end = entry.dial_end;
            if matches!(entry.sock, Sock::Unknown) {
                // A connection the table did not see dialed (its entry was evicted): its
                // socket is looked up from now on, for this request's end and the next ones.
                entry.sock = self.shared.find_socket(addr, true);
            }
            sock = entry.sock.clone();
            open_streams = entry.active_streams.fetch_add(1, Ordering::AcqRel) + 1;
            stream_guard = StreamGuard(Some(entry.active_streams.clone()));
        }
        // Time on the wire starts when the connection was ready.
        let mut wire_from = t_ready;
        let mut waited_dial_us = None;
        match own_dial {
            // This request started the connection's dial and waited for it: the dial's
            // phases are this request's.
            Some(d) => {
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
            None => {
                // Another request dialed this connection while this one waited in the
                // client's pool (a concurrent HTTP/2 stream, or an HTTP/1.1 request taking
                // a connection its dialer no longer needed): the wait is `queue_us`, and
                // the wire time starts once the connection was ready.
                if let Some(d_end) = dial_end
                    && d_end > t_ready
                    && d_end <= t_headers
                {
                    let wait = d_end.saturating_duration_since(t_ready).as_micros() as u64;
                    waited_dial_us = Some(wait);
                    record.timing.queue_us += wait;
                    wire_from = d_end;
                }
            }
        }
        if record.protocol == Protocol::Http2
            && let (Some(addr), Ok(c)) = (addr, self.shared.conns.lock())
            && let Some(e) = c.by_addr.get(&addr)
        {
            let overlapped = e
                .recent_ends
                .iter()
                .flatten()
                .filter(|&&end| end > wire_from)
                .count() as u32;
            open_streams = open_streams.saturating_add(overlapped);
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
        let detail = record.detail.get_or_insert_with(Default::default);
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
                Some(sent_at.saturating_duration_since(wire_from).as_micros() as u64);
        }

        // --- identical to opendal-http-transport-reqwest ---------------------------------
        let content_length = parse_content_encoding(resp.headers()).and_then(|encoding| {
            if is_head || encoding.is_some() {
                Ok(None)
            } else {
                parse_content_length(resp.headers())
            }
        });
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

        // The upload's stream when it was opened on this connection while it sent; else the
        // request joins the connection's retransmit ledger here, if its probe is there.
        let sent_stream = sending.and_then(|sending| sending.answered(addr));
        let conn = addr.map(|addr| ConnUse {
            addr,
            shared: self.shared.clone(),
            probe: match (sent_stream, sock) {
                (Some((probe, stream)), _) => Probing::Stream(probe, stream),
                (None, Sock::Found(probe)) => {
                    let stream = probe.open_stream();
                    Probing::Stream(probe, stream)
                }
                (None, Sock::Finding(lookup)) => match lookup.get() {
                    Some(Some(probe)) => Probing::Stream(probe.clone(), probe.open_stream()),
                    Some(None) => Probing::Missing,
                    None => Probing::Finding(lookup),
                },
                (None, Sock::Unknown | Sock::Missing) => Probing::Missing,
            },
            _stream: stream_guard,
        });
        let fin = Finisher {
            record,
            conn,
            t_start,
            t_headers,
            scope,
            recorder,
            _permit: permit,
            _inflight: inflight,
            sniff,
        };
        let content_length = match content_length {
            Ok(declared) => declared,
            // A response OpenDAL cannot use is a failed request, not a missing one (until 9.2
            // this returned before anything was recorded: the operation failed with no request
            // in the trace). The same response would fail again: not temporary.
            Err(err) => {
                let mut fin = fin;
                fin.record
                    .detail
                    .get_or_insert_with(Default::default)
                    .temporary = Some(false);
                fin.finish(Duration::ZERO, Some(format!("response headers: {err}")));
                return Err(err);
            }
        };
        let chunks = futures::StreamExt::map(resp.bytes_stream(), move |chunk| {
            chunk.map(Buffer::from).map_err(|err| {
                let temporary = is_temporary_error(&err);
                Error::new(ErrorKind::Unexpected, "read data from http response")
                    .with_operation("reqwest::fetch")
                    .with_context("url", public_url(&uri))
                    .with_temporary(temporary)
                    .set_source(err.without_url())
            })
        });
        let stream = TimedResponseBody::new(chunks, fin, content_length);
        Ok(hr
            .body(HttpBody::new(stream, content_length))
            .expect("response must build"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::testserver;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            recorder: None,
            conns: Mutex::new(Conns::default()),
            finder: Arc::default(),
            proxy: None,
            proxy_addrs: Vec::new(),
            tcp_info: false,
            active_uploads: AtomicU32::new(0),
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

    /// A failed request records why, never the URL, and the error it returns quotes the URL
    /// without its credentials: a presigned query or an Azure SAS is a bearer credential, the
    /// trace is a file people share, and the error is printed and logged (9.1 kept the whole
    /// URL in the returned error's context).
    #[tokio::test]
    async fn a_failed_request_never_leaks_its_presigned_url() {
        let recorder = Recorder::always_on(64);
        let errors = Arc::new(Errors(Mutex::new(Vec::new())));
        recorder.set_exporter(errors.clone());
        let cfg = TransportConfig {
            proxy: Some(None),
            ..TransportConfig::default()
        };
        let t = FidelityTransport::new(cfg, false, Some(recorder.clone())).unwrap();
        // Nothing listens on port 1: the connect fails.
        let secrets = ["SIGSECRET", "STSSECRET", "AKIASECRET", "SASSECRET"];
        let req = Request::get(
            "http://127.0.0.1:1/c/k?X-Amz-Expires=900&X-Amz-Credential=AKIASECRET%2Fs3\
             &X-Amz-Security-Token=STSSECRET&X-Amz-Signature=SIGSECRET&sv=2024&sig=SASSECRET",
        )
        .body(Buffer::new())
        .unwrap();
        let Err(err) = t.fetch(req).await else {
            panic!("the connect cannot succeed");
        };
        // The error the application sees, as it prints and logs it.
        let shown = [format!("{err}"), format!("{err:?}"), format!("{err:#?}")];
        assert!(
            shown[0].contains("http://127.0.0.1:1/c/k?X-Amz-Expires=900&sv=2024"),
            "{err}"
        );
        // What the exporter saw, and the rows.
        let errors = errors.0.lock().unwrap().clone();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("send http request"), "{errors:?}");
        // The saved profile, byte for byte.
        let dir = std::env::temp_dir().join(format!("oprobe-trace-presigned-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("failed.oprobe");
        let snapshot = recorder.snapshot();
        let rows = recorder.take_trace();
        assert_eq!(rows.reqs.len(), 1);
        crate::file::write(&path, None, &snapshot, Some(&rows)).unwrap();
        let file = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(file.contains("send http request"), "{file}");
        let rows = format!("{rows:?}");
        for text in shown.iter().chain(&errors).chain([&rows, &file]) {
            for secret in secrets {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
        for text in errors.iter().chain([&rows, &file]) {
            assert!(!text.contains("127.0.0.1:1/"), "{text}");
        }
    }

    async fn drain(resp: Response<HttpBody>) {
        let (_, mut body) = resp.into_parts();
        body.to_buffer().await.unwrap();
    }

    fn transport(http: HttpVersion, recorder: &Arc<Recorder>) -> FidelityTransport {
        let cfg = TransportConfig {
            proxy: Some(None),
            http,
            ..TransportConfig::default()
        };
        FidelityTransport::new(cfg, false, Some(recorder.clone())).unwrap()
    }

    /// Two requests share one HTTP/2 connection. The first one dials it; the second waits for
    /// that dial and is answered first. The dial still is the first request's: it is the cold
    /// one, and the second one found the connection without dialing (9.1 gave the dial to
    /// whichever request was answered first).
    #[tokio::test]
    async fn the_request_that_dialed_carries_the_dial_whoever_is_answered_first() {
        // `/first` is answered only when the test says so.
        let release = Arc::new(tokio::sync::Notify::new());
        let held = release.clone();
        let server = testserver::serve(move |req| {
            let held = held.clone();
            async move {
                if req.uri().path() == "/first" {
                    held.notified().await;
                }
                testserver::respond(200, "ok")
            }
        })
        .await;
        let recorder = Recorder::always_on(64);
        let t = transport(HttpVersion::H2, &recorder);
        let get = |path: &str| {
            Request::get(format!("http://{}/{path}", server.addr))
                .body(Buffer::new())
                .unwrap()
        };
        // Polled in this order: the first request starts the dial, the second finds the
        // connection being dialed and waits for it.
        let first = async { drain(t.fetch(get("first")).await.unwrap()).await };
        let second = async {
            drain(t.fetch(get("second")).await.unwrap()).await;
            release.notify_one();
        };
        tokio::join!(first, second);

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.connections.dialed, 1, "{:?}", snapshot.connections);
        let mut rows = recorder.take_trace().reqs;
        assert_eq!(rows.len(), 2, "{rows:?}");
        // The first request started first and ended last.
        rows.sort_by_key(|r| std::cmp::Reverse(r.total_us));
        let (first, second) = (&rows[0], &rows[1]);
        assert_eq!(first.conn_id, second.conn_id, "{rows:?}");
        assert_eq!(first.protocol, crate::rows::WireProtocol::H2, "{rows:?}");
        assert!(!first.reused, "the dialing request is the cold one: {rows:?}");
        assert!(first.tcp_us > 0, "{rows:?}");
        assert!(second.reused, "the request answered first did not dial: {rows:?}");
        assert_eq!(second.connect_us(), 0, "{rows:?}");
    }

    fn conn(port: u16) -> ConnAddr {
        (
            SocketAddr::from(([127, 0, 0, 1], port)),
            SocketAddr::from(([127, 0, 0, 1], 443)),
        )
    }

    fn dial_times() -> DialTimes {
        DialTimes {
            dns_us: 10,
            tcp_us: 20,
            proxy_us: 0,
            tls_us: 30,
            end: Instant::now(),
            tls: None,
        }
    }

    fn probing(recorder: Option<Arc<Recorder>>) -> Arc<Shared> {
        Arc::new(Shared {
            recorder,
            conns: Mutex::new(Conns::default()),
            finder: Arc::default(),
            proxy: None,
            proxy_addrs: Vec::new(),
            active_uploads: AtomicU32::new(0),
            tcp_info: true,
        })
    }

    /// A connected loopback socket pair (client, server) and the client's connection identity.
    async fn socket_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream, ConnAddr) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, server) = tokio::join!(
            tokio::net::TcpStream::connect(l.local_addr().unwrap()),
            l.accept()
        );
        let client = client.unwrap();
        let addr = (client.local_addr().unwrap(), client.peer_addr().unwrap());
        (client, server.unwrap().0, addr)
    }

    /// The answer of a socket lookup, once it is there.
    async fn answered(lookup: &Lookup) -> Option<Arc<TcpProbe>> {
        loop {
            if let Some(answer) = lookup.get() {
                return answer.clone();
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// A new connection's socket is looked up once, in a task of its own: the lookup's answer
    /// is the probe the connection table keeps, for every request of the connection.
    #[tokio::test]
    async fn a_new_connections_socket_lookup_answers_into_the_table() {
        if !tcpinfo::SUPPORTED {
            return;
        }
        let (_client, _server, addr) = socket_pair().await;
        let shared = probing(None);
        let Sock::Finding(lookup) = shared.find_socket(addr, false) else {
            panic!("the lookup starts");
        };
        shared.dialed(None, addr, dial_times(), Sock::Finding(lookup.clone()));
        let found = answered(&lookup).await.expect("the socket of a live connection");
        // The lookup's task has the table's lock right after it answered.
        tokio::task::yield_now().await;
        let c = shared.conns.lock().unwrap();
        let Sock::Found(kept) = &c.by_addr[&addr].sock else {
            panic!("the table keeps the probe");
        };
        assert!(Arc::ptr_eq(kept, &found), "one probe for the connection");
    }

    /// Holds the only thread of a runtime's blocking pool until dropped: whatever is spawned
    /// on the pool meanwhile (a socket lookup's descriptor scan) stays queued.
    struct BusyBlockingPool(Option<std::sync::mpsc::Sender<()>>);

    impl BusyBlockingPool {
        fn of(runtime: &tokio::runtime::Runtime) -> Self {
            let (release, held) = std::sync::mpsc::channel::<()>();
            let (running, started) = std::sync::mpsc::channel::<()>();
            runtime.spawn_blocking(move || {
                running.send(()).unwrap();
                // Returns when the sender is dropped.
                let _ = held.recv();
            });
            started.recv().unwrap();
            Self(Some(release))
        }
    }

    impl Drop for BusyBlockingPool {
        fn drop(&mut self) {
            self.0.take();
        }
    }

    fn one_blocking_thread() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
    }

    /// A test must fail, not hang, where 9.2 waited for ever. Nothing here depends on how
    /// long the machine takes: the bound is only reached when the future never completes.
    async fn never_stuck<T>(what: &str, f: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(120), f)
            .await
            .unwrap_or_else(|_| panic!("{what} never completed"))
    }

    async fn get_ok(t: &FidelityTransport, server: &testserver::TestServer) {
        let req = Request::get(format!("http://{}/k", server.addr))
            .body(Buffer::new())
            .unwrap();
        drain(t.fetch(req).await.unwrap()).await;
    }

    /// A request never waits for the lookup of its connection's socket. With the runtime's
    /// blocking pool busy the lookup cannot answer: the request is served all the same,
    /// without kernel statistics, and the connection's next request has them once the lookup
    /// did answer (9.2 awaited the lookup when the response headers arrived: the response was
    /// held back for as long as the pool was busy, and the wait read as consumer stall).
    #[test]
    fn a_request_does_not_wait_for_a_slow_socket_lookup() {
        let runtime = one_blocking_thread();
        let busy = BusyBlockingPool::of(&runtime);
        runtime.block_on(async {
            let server = testserver::serve(|_| async { testserver::respond(200, "ok") }).await;
            let recorder = Recorder::always_on(64);
            let t = transport(HttpVersion::H1, &recorder);
            never_stuck("the request", get_ok(&t, &server)).await;
            let first = recorder.take_trace().reqs;
            assert_eq!(first.len(), 1, "{first:?}");
            assert_eq!((first[0].status, first[0].reused), (200, false), "{first:?}");
            assert!(first[0].tcp.is_none(), "the lookup cannot have answered: {first:?}");
            if !tcpinfo::SUPPORTED {
                return;
            }
            let lookup = {
                let c = t.shared.conns.lock().unwrap();
                let Some(Sock::Finding(lookup)) = c.by_addr.values().next().map(|e| e.sock.clone())
                else {
                    panic!("the connection's socket is still being looked up");
                };
                lookup
            };
            // The pool is free again: the lookup answers, and the next request reads it.
            drop(busy);
            assert!(answered(&lookup).await.is_some());
            get_ok(&t, &server).await;
            let second = recorder.take_trace().reqs;
            assert_eq!(second.len(), 1, "{second:?}");
            assert_eq!(second[0].conn_id, first[0].conn_id, "{second:?}");
            assert!(second[0].reused && second[0].tcp.is_some(), "{second:?}");
        });
    }

    /// An operator outlives the runtime it was first used on (a shared operator in tests that
    /// each build a runtime, an application that restarts its runtime). The first runtime is
    /// shut down with a socket lookup queued on its blocking pool; requests on the second
    /// runtime are served (9.2: the lookup of the first runtime never answered, the finder
    /// stayed busy with it, and every request on a new connection waited for ever).
    #[test]
    fn an_operator_shared_across_runtimes_outlives_a_lookup_queued_on_the_first() {
        // The server lives on a runtime of its own.
        let serving = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let server =
            serving.block_on(testserver::serve(|_| async { testserver::respond(200, "ok") }));
        let recorder = Recorder::always_on(64);
        // No connection is reused: the ones of the first runtime die with it.
        let cfg = TransportConfig {
            proxy: Some(None),
            keep_alive: false,
            ..TransportConfig::default()
        };
        let t = FidelityTransport::new(cfg, false, Some(recorder.clone())).unwrap();

        let first = one_blocking_thread();
        let busy = BusyBlockingPool::of(&first);
        first.block_on(never_stuck("the request on the first runtime", get_ok(&t, &server)));
        first.shutdown_background();

        let second = one_blocking_thread();
        second.block_on(async {
            for _ in 0..3 {
                never_stuck("a request on the second runtime", get_ok(&t, &server)).await;
            }
        });
        drop(busy);
        let rows = recorder.take_trace().reqs;
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(rows.iter().all(|r| r.status == 200), "{rows:?}");
        // Four connections, numbered as they were dialed.
        let ids: Vec<u64> = rows.iter().map(|r| r.conn_id).collect();
        assert_eq!(ids, [1, 2, 3, 4]);
    }

    /// A response OpenDAL cannot use (a `Content-Encoding` that is not text) fails the
    /// operation, and is in the trace as the failed request it is: with its status, its
    /// connection, an error, and not to be retried (until 9.2 the transport returned before
    /// recording anything: the operation failed with no request to show for it).
    #[tokio::test]
    async fn a_response_with_unusable_headers_is_recorded_as_a_failed_request() {
        let server = testserver::serve(|_| async {
            http::Response::builder()
                .status(200)
                .header("content-encoding", http::HeaderValue::from_bytes(b"\xff").unwrap())
                .body(http_body_util::Full::new(Bytes::from_static(b"hello")))
                .unwrap()
        })
        .await;
        let recorder = Recorder::always_on(64);
        let t = transport(HttpVersion::H1, &recorder);
        let req = Request::get(format!("http://{}/k", server.addr))
            .body(Buffer::new())
            .unwrap();
        let Err(err) = t.fetch(req).await else {
            panic!("the response cannot be used");
        };
        assert!(!err.is_temporary(), "{err}");
        let snapshot = recorder.snapshot();
        let rows = recorder.take_trace().reqs;
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!((row.status, row.conn_id, row.reused), (200, 1, false), "{row:?}");
        let error = row.error.as_deref().expect("a failed request");
        assert!(error.starts_with("response headers: "), "{error}");
        assert_eq!(row.detail.as_ref().and_then(|d| d.temporary), Some(false), "{row:?}");
        assert!(row.total_us >= row.ttfb_us && row.ttfb_us > 0, "{row:?}");
        let api = &snapshot.apis[0];
        assert_eq!((api.count, api.errors), (1, 1), "{api:?}");
    }

    /// The upload in flight is told its connection while it sends, and its retransmit stream
    /// opens there: on the only connection alive (a closed one beside it does not make it
    /// ambiguous), and not at all with two uploads in flight. A request that fails or is
    /// dropped before any answer still carries what the kernel says of its connection.
    #[tokio::test]
    async fn an_uploads_retransmit_stream_opens_while_it_sends() {
        if !tcpinfo::SUPPORTED {
            return;
        }
        let shared = probing(None);
        let (client, _server, addr) = socket_pair().await;
        let (closed, closed_server, closed_addr) = socket_pair().await;
        for addr in [addr, closed_addr] {
            let Sock::Finding(lookup) = shared.find_socket(addr, false) else {
                panic!("the lookup starts");
            };
            shared.dialed(None, addr, dial_times(), Sock::Finding(lookup.clone()));
            assert!(answered(&lookup).await.is_some());
        }
        tokio::task::yield_now().await;
        drop((closed, closed_server));

        shared.active_uploads.store(2, Ordering::Release);
        assert!(shared.uploading_probe().is_none(), "two uploads: whose connection?");
        shared.active_uploads.store(1, Ordering::Release);
        let probe = shared.uploading_probe().expect("the only connection alive");
        assert!(probe.is_of(addr));

        // Answered on that connection: the stream stays, past its send.
        let sending = SendingStream::default();
        sending.open(&probe);
        let (kept, stream) = sending.answered(Some(addr)).expect("its connection");
        assert!(Arc::ptr_eq(&kept, &probe));
        assert_eq!(probe.close_stream(stream).1, 0);
        // Answered elsewhere: the guess is withdrawn.
        sending.open(&probe);
        assert!(sending.answered(Some(closed_addr)).is_none());
        // Never answered: the record has the connection's kernel statistics.
        sending.open(&probe);
        let recorder = Recorder::always_on(8);
        let parts = Request::put("http://h/k").body(()).unwrap().into_parts().0;
        let mut entered =
            Entered::new(Instant::now(), &parts, "/k", None, None, 5, &Some(recorder.clone()));
        entered.cancel.sending = Some(Arc::new(sending));
        drop(entered);
        let rows = recorder.take_trace().reqs;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].tcp.is_some() && rows[0].error.is_some(), "{rows:?}");
        drop(client);
    }

    /// A dial belongs to the request that started it. When that request is answered on
    /// another connection, fails or is dropped, the connection is one nobody waited for: it
    /// is counted once, and no request is charged its handshake.
    #[test]
    fn a_dial_its_request_did_not_use_is_counted_and_charged_to_nobody() {
        let recorder = Recorder::always_on(64);
        let shared = Arc::new(Shared {
            recorder: Some(recorder.clone()),
            conns: Mutex::new(Conns::default()),
            finder: Arc::default(),
            proxy: None,
            proxy_addrs: Vec::new(),
            tcp_info: false,
            active_uploads: AtomicU32::new(0),
        });
        let dialed = || recorder.snapshot().connections.dialed;
        let claim = Arc::<DialClaim>::default;

        // Another request is answered on the new connection first: the dial is not its own.
        let (dialer, other) = (claim(), claim());
        shared.dialed(Some(dialer.clone()), conn(1), dial_times(), Sock::Missing);
        assert!(shared.settle(&other, Some(conn(1))).is_none());
        assert_eq!(dialed(), 0);
        // The dialer is answered on a connection that turned up meanwhile: unclaimed.
        assert!(shared.settle(&dialer, Some(conn(2))).is_none());
        assert_eq!(dialed(), 1);
        // A dial that ends after its request was answered is unclaimed at once.
        shared.dialed(Some(dialer), conn(3), dial_times(), Sock::Missing);
        assert_eq!(dialed(), 2);
        let c = shared.conns.lock().unwrap();
        assert!(c.by_addr.values().all(|e| e.dial.is_none()));
        assert_eq!(c.by_addr.len(), 2);
        drop(c);

        // Answered on the connection it dialed: the dial is this request's (and is counted
        // with the request, not here).
        let dialer = claim();
        shared.dialed(Some(dialer.clone()), conn(4), dial_times(), Sock::Missing);
        let own = shared.settle(&dialer, Some(conn(4))).expect("its own dial");
        assert_eq!((own.dns_us, own.tcp_us, own.tls_us), (10, 20, 30));
        assert_eq!(dialed(), 2);

        // A request dropped before any answer (a timeout): its dial is unclaimed.
        let dialer = claim();
        let guard = ClaimGuard {
            claim: dialer.clone(),
            shared: shared.clone(),
        };
        shared.dialed(Some(dialer), conn(5), dial_times(), Sock::Missing);
        assert_eq!(dialed(), 2);
        drop(guard);
        assert_eq!(dialed(), 3);
    }

    /// Eight connections dialed at once: each one's socket is found (the kernel statistics
    /// are on every request once its lookup has answered: no request waits for it), and the
    /// runtime's worker thread never scans the descriptor table for it (9.1 scanned the table
    /// once per connection, on the worker).
    #[tokio::test]
    async fn new_connections_never_scan_descriptors_on_the_worker_thread() {
        const CONNS: usize = 8;
        // Every request is held until all have arrived, so none can reuse a connection.
        let all_arrived = Arc::new(tokio::sync::Barrier::new(CONNS));
        let server = testserver::serve(move |_| {
            let all_arrived = all_arrived.clone();
            async move {
                all_arrived.wait().await;
                testserver::respond(200, "ok")
            }
        })
        .await;
        let recorder = Recorder::always_on(64);
        let t = transport(HttpVersion::H1, &recorder);
        let scans = || tcpinfo::SCANS.with(|n| n.get());
        let before = scans();
        let at_once = || futures::future::join_all((0..CONNS).map(|_| get_ok(&t, &server)));
        at_once().await;
        // This (current-thread) runtime's only worker is the test thread.
        assert_eq!(scans(), before);

        let rows = recorder.take_trace().reqs;
        let conns: std::collections::BTreeSet<u64> = rows.iter().map(|r| r.conn_id).collect();
        assert_eq!((rows.len(), conns.len()), (CONNS, CONNS), "{rows:?}");
        // Numbered as they were dialed: the highest id is how many there are.
        assert_eq!(conns.last(), Some(&(CONNS as u64)), "{rows:?}");
        assert!(rows.iter().all(|r| !r.reused), "{rows:?}");
        if !tcpinfo::SUPPORTED {
            return;
        }
        // Every lookup answers, and from then on each request has its connection's kernel
        // statistics.
        let socks: Vec<Sock> = {
            let c = t.shared.conns.lock().unwrap();
            c.by_addr.values().map(|e| e.sock.clone()).collect()
        };
        assert_eq!(socks.len(), CONNS);
        for sock in socks {
            match sock {
                Sock::Finding(lookup) => assert!(answered(&lookup).await.is_some()),
                Sock::Found(_) => {}
                Sock::Unknown | Sock::Missing => panic!("the socket is looked up"),
            }
        }
        at_once().await;
        assert_eq!(scans(), before);
        let rows = recorder.take_trace().reqs;
        assert_eq!(rows.len(), CONNS, "{rows:?}");
        assert!(rows.iter().all(|r| r.reused && r.tcp.is_some()), "{rows:?}");
    }
}
