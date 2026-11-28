//! Kernel TCP statistics of a live connection at request end: Linux `TCP_INFO`, macOS
//! `TCP_CONNECTION_INFO`; other platforms report nothing.
//!
//! # Safety of querying a socket oprobe does not own
//!
//! The microscope transport owns its `TcpStream`; OpenDAL's reqwest client does not expose its
//! sockets. In both cases oprobe only remembers a *descriptor number* together with the
//! connection's identity (local, remote address) and never owns, closes or writes to it:
//!
//! - every query is read-only (`getsockopt`, `getsockname`, `getpeername`): a descriptor that
//!   was closed meanwhile makes them fail (`EBADF`) and the query yields `None`;
//! - a descriptor *number* reused by an unrelated file or socket is detected: the statistics
//!   are only returned when, *after* reading them, the descriptor still names a TCP socket
//!   with exactly the remembered local and remote address. A live 4-tuple identifies one
//!   connection, so the numbers belong to the connection the request used (or, in the
//!   theoretical case of the 4-tuple itself being reused between the two calls, to a
//!   connection indistinguishable from it by every attribute a request row records).
//!
//! No descriptor is ever duplicated, so no connection is kept open by oprobe.
//!
//! Cost: three syscalls per sample (≈1–3 µs), once per request (retransmits are attributed
//! per request), and once more at the response headers of an upload whose connection was
//! known while it sent. The descriptor of a connection inside reqwest is found by scanning the
//! process's descriptor table: [`SockFinder`] does that on the runtime's blocking pool, never
//! on a worker thread, and one scan answers every connection dialed meanwhile.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::rows::TcpRow;

/// A connection oprobe may query: a descriptor number and the identity it must still have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockId {
    pub fd: i64,
    pub local: SocketAddr,
    pub remote: SocketAddr,
}

impl SockId {
    /// TCP statistics of this connection now, if it is still alive and the platform has them.
    pub fn query(&self) -> Option<TcpRow> {
        imp::query(self)
    }
}

/// Who a connection's retransmits are charged to. The kernel counts retransmitted segments
/// per connection, and an HTTP/2 connection carries many requests at once, so the count is
/// read at the events of its requests and what it grew by since the previous reading is
/// charged to the requests in flight at that moment:
///
/// - to the ones **sending a request body**, when there are any: what a client retransmits is
///   what it sends, and next to an upload a request that only waits for its response sends
///   nothing. A request counts as sending from the moment the transport knows which
///   connection carries its body ([`Self::open_sending`]) until its response headers arrive
///   ([`Self::sent`]);
/// - else evenly to all of them.
///
/// The indivisible rest goes to the request the reading belongs to (to the first one charged
/// when that one is not among them). Every retransmit is charged exactly once: the charges of
/// a connection's requests add up to the growth of its counter. On HTTP/1.1 one request is in
/// flight at a time and is charged everything since the previous request ended.
///
/// What remains: an upload whose connection the transport cannot tell while it sends (several
/// uploads at once, or several connections with none holding unsent bytes) joins at its
/// response headers. On HTTP/2 a request that ends on the same connection before that is
/// charged the upload's retransmits so far.
#[derive(Debug, Default)]
pub struct RetransLedger {
    /// The connection's counter at the previous reading (`None`: never read).
    seen: Option<u64>,
    /// The requests in flight.
    open: Vec<LedgerStream>,
    next_stream: u64,
}

#[derive(Debug)]
struct LedgerStream {
    id: u64,
    /// Retransmits charged to it so far.
    charged: u64,
    /// Its request body is on its way.
    sending: bool,
}

impl RetransLedger {
    /// A ledger for a connection whose counter reads `total` now (`None`: unreadable; the
    /// first reading then only sets the baseline).
    pub fn seeded(total: Option<u64>) -> Self {
        Self {
            seen: total,
            ..Self::default()
        }
    }

    /// The ledger of a connection found while it may already be carrying its first request
    /// (the socket lookup runs off the worker thread and can finish mid-flight). A connection
    /// this transport dialed (`warm` false) has retransmitted nothing before it: its counter
    /// starts at zero, so what its first request retransmitted before the lookup finished is
    /// charged to that request rather than taken for a baseline (9.3.2 seeded it with the
    /// counter as read then, and lost them). The SYN retransmits of its handshake go with
    /// the first request too, whose connect they slowed. A pooled connection found again
    /// (`warm`) starts from its counter now: what it retransmitted before belongs to nobody
    /// still in flight.
    pub fn for_connection(now: Option<u64>, warm: bool) -> Self {
        Self::seeded(if warm { now } else { Some(0) })
    }

    /// A request starts using the connection (its response headers arrived): its stream.
    pub fn open(&mut self) -> u64 {
        self.join(false)
    }

    /// A request starts sending its body on the connection: its stream.
    pub fn open_sending(&mut self) -> u64 {
        self.join(true)
    }

    fn join(&mut self, sending: bool) -> u64 {
        self.next_stream += 1;
        self.open.push(LedgerStream {
            id: self.next_stream,
            charged: 0,
            sending,
        });
        self.next_stream
    }

    fn position(&self, stream: u64) -> Option<usize> {
        self.open.iter().position(|s| s.id == stream)
    }

    /// The counter reads `total` at an event of the stream at `at`: charge what it grew by.
    /// Readings may arrive out of order (two requests ending at once): the counter never
    /// goes back, so a stale reading settles nothing.
    fn settle(&mut self, at: usize, total: u64) {
        let grown = self.seen.map_or(0, |seen| total.saturating_sub(seen));
        self.seen = Some(self.seen.map_or(total, |seen| seen.max(total)));
        if grown == 0 {
            return;
        }
        let uploading = self.open.iter().any(|s| s.sending);
        let charged = |s: &LedgerStream| s.sending || !uploading;
        let n = self.open.iter().filter(|s| charged(s)).count() as u64;
        for s in self.open.iter_mut().filter(|s| charged(s)) {
            s.charged += grown / n;
        }
        let rest = if charged(&self.open[at]) {
            at
        } else {
            self.open.iter().position(charged).unwrap_or(at)
        };
        self.open[rest].charged += grown % n;
    }

    /// The response headers of `stream` arrived while the counter reads `total` (`None`:
    /// unreadable now): its body is sent, and what the counter grew by while it was sending
    /// is settled.
    pub fn sent(&mut self, stream: u64, total: Option<u64>) {
        let Some(at) = self.position(stream) else {
            return;
        };
        if let Some(total) = total {
            self.settle(at, total);
        }
        self.open[at].sending = false;
    }

    /// `stream` ends while the connection's counter reads `total` (`None`: unreadable now):
    /// settle the growth among the streams in flight and return what `stream` was charged.
    pub fn close(&mut self, stream: u64, total: Option<u64>) -> u64 {
        let Some(at) = self.position(stream) else {
            return 0;
        };
        if let Some(total) = total {
            self.settle(at, total);
        }
        self.open.swap_remove(at).charged
    }

    /// `stream` was not on this connection after all (the transport's guess at send was
    /// wrong): what it was charged is owed again, to whoever is in flight at the next
    /// reading.
    pub fn abandon(&mut self, stream: u64) {
        let Some(at) = self.position(stream) else {
            return;
        };
        let charged = self.open.swap_remove(at).charged;
        self.seen = self.seen.map(|seen| seen.saturating_sub(charged));
    }
}

/// Sampling of one connection's statistics, with its retransmits charged to the requests in
/// flight ([`RetransLedger`]).
#[derive(Debug)]
pub struct TcpProbe {
    pub id: SockId,
    retrans: Mutex<RetransLedger>,
    /// Bytes received and acknowledged at the previous sample (wire bytes since then).
    last_rx: AtomicU64,
    last_tx: AtomicU64,
}

impl TcpProbe {
    /// Create a probe for `id`. When `warm` is false (a freshly dialed connection), `last_rx`
    /// and `last_tx` start at 0 so handshake bytes count once, and so does the retransmit
    /// ledger ([`RetransLedger::for_connection`]), so retransmits on the first request are
    /// seen even when the probe is created after they happened.
    /// When `warm` is true (an already-pooled connection re-discovered after table eviction),
    /// `last_rx` and `last_tx` are seeded from the socket's current totals so prior bytes are
    /// never double-counted.
    pub fn new_seeded(id: SockId, warm: bool) -> Self {
        let initial = id.query();
        let (last_rx, last_tx) = match (&initial, warm) {
            (Some(r), true) => (r.bytes_received, r.bytes_acked),
            _ => (0, 0),
        };
        Self {
            id,
            retrans: Mutex::new(RetransLedger::for_connection(
                initial.map(|r| r.retrans),
                warm,
            )),
            last_rx: AtomicU64::new(last_rx),
            last_tx: AtomicU64::new(last_tx),
        }
    }

    /// A request starts using the connection (its response headers arrived): the stream to
    /// hand back to [`Self::close_stream`].
    pub fn open_stream(&self) -> u64 {
        self.ledger().open()
    }

    /// A request starts sending its body on the connection: the stream to hand to
    /// [`Self::stream_sent`] when its response headers arrive, or back to
    /// [`Self::abandon_stream`] when they arrive on another connection.
    pub fn open_sending_stream(&self) -> u64 {
        self.ledger().open_sending()
    }

    /// The response headers of the sending `stream` arrived (one reading of the connection).
    pub fn stream_sent(&self, stream: u64) {
        let total = self.id.query().map(|r| r.retrans);
        self.ledger().sent(stream, total);
    }

    /// `stream` was not on this connection.
    pub fn abandon_stream(&self, stream: u64) {
        self.ledger().abandon(stream);
    }

    /// The request of `stream` ended: the connection's statistics now (`None` when it is
    /// already closed) and the retransmitted segments charged to the request (every request
    /// is sampled: the charges need every reading, design v5 §3 M9).
    pub fn close_stream(&self, stream: u64) -> (Option<TcpRow>, u64) {
        let row = self.id.query();
        let charged = self
            .ledger()
            .close(stream, row.as_ref().map(|r| r.retrans));
        (row, charged)
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, RetransLedger> {
        self.retrans.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The bytes received and sent (acknowledged) on the wire since the previous call, from
    /// `row` (the first call counts from the connection's start: its handshakes included).
    /// Concurrent requests on one connection each get a disjoint share.
    pub fn wire_since_last(&self, row: &TcpRow) -> (u64, u64) {
        let rx = self.last_rx.fetch_max(row.bytes_received, Ordering::AcqRel);
        let tx = self.last_tx.fetch_max(row.bytes_acked, Ordering::AcqRel);
        (
            row.bytes_received.saturating_sub(rx),
            row.bytes_acked.saturating_sub(tx),
        )
    }

    /// Raw statistics without touching the retransmit baseline (upload-acknowledgement polls).
    pub fn peek(&self) -> Option<TcpRow> {
        self.id.query()
    }

    /// Whether this is the probe of connection `addr`.
    pub fn is_of(&self, addr: ConnAddr) -> bool {
        (self.id.local, self.id.remote) == addr
    }
}

/// The identity of an owned stream (microscope transport).
pub fn sock_id_of(s: &tokio::net::TcpStream) -> Option<SockId> {
    imp::sock_id_of(s)
}

/// A connection's identity: its (local, remote) address.
pub type ConnAddr = (SocketAddr, SocketAddr);

#[cfg(test)]
thread_local! {
    /// Descriptor-table scans this thread made.
    pub static SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Find the descriptors of the connections `wanted` in this process, in one pass over its
/// descriptor table (the sockets live inside reqwest). `None` for one that is not found, and
/// for all where the platform has no safe lookup. It reads a directory and makes a syscall
/// per descriptor: call it through [`SockFinder`], off the runtime's worker threads.
pub fn find_all(wanted: &[ConnAddr]) -> Vec<Option<SockId>> {
    #[cfg(test)]
    SCANS.with(|n| n.set(n.get() + 1));
    imp::find_all(wanted)
}

/// Finds the sockets of new connections off the runtime's worker threads. A lookup is queued
/// and answered by a task on the blocking pool; while that task scans the descriptor table,
/// the lookups of other connections queue up and share its next scan, so a burst of new
/// connections costs a few scans, not one each, and never a blocked worker.
#[derive(Debug, Default)]
pub struct SockFinder {
    queue: Mutex<FinderQueue>,
}

#[derive(Debug, Default)]
struct FinderQueue {
    pending: Vec<(ConnAddr, tokio::sync::oneshot::Sender<Option<SockId>>)>,
    /// A blocking task is draining `pending`.
    scanning: bool,
}

impl SockFinder {
    /// The descriptor of connection `addr`; `None` when it is not found, the platform has no
    /// lookup, or the runtime is shutting down.
    pub async fn find(self: &Arc<Self>, addr: ConnAddr) -> Option<SockId> {
        if !SUPPORTED {
            return None;
        }
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        // A scan that its runtime dropped answers nobody. The lookup may have waited for the
        // scan of another runtime (the finder is its operator's): it asks once more, and its
        // own runtime scans.
        for _ in 0..2 {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let start = {
                let mut q = self.queue();
                q.pending.push((addr, tx));
                !std::mem::replace(&mut q.scanning, true)
            };
            if start {
                let mut scan = Scan {
                    finder: self.clone(),
                    done: false,
                };
                runtime.spawn_blocking(move || scan.drain());
            }
            if let Ok(found) = rx.await {
                return found;
            }
        }
        None
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, FinderQueue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The blocking task that answers a finder's queue. The runtime may drop it without running
/// it (a runtime shut down while the task waits for a thread of its blocking pool), and the
/// finder outlives the runtime when its operator does: dropped before it finished, it gives
/// the queue back, so the lookups waiting for it wake (and ask again, once) and the next
/// lookup starts a scan on its own runtime. 9.2 left `scanning` set, and every later lookup
/// waited for ever.
struct Scan {
    finder: Arc<SockFinder>,
    done: bool,
}

impl Scan {
    /// Answer every queued lookup, one scan per batch, until the queue is empty.
    fn drain(&mut self) {
        loop {
            let batch = {
                let mut q = self.finder.queue();
                if q.pending.is_empty() {
                    // Under the lock: a lookup queued from now on starts its own scan.
                    q.scanning = false;
                    self.done = true;
                    return;
                }
                std::mem::take(&mut q.pending)
            };
            let (wanted, waiting): (Vec<ConnAddr>, Vec<_>) = batch.into_iter().unzip();
            for (tx, found) in waiting.into_iter().zip(find_all(&wanted)) {
                // The dial was dropped meanwhile: nobody waits for the answer.
                let _ = tx.send(found);
            }
        }
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let waiting = {
            let mut q = self.finder.queue();
            q.scanning = false;
            std::mem::take(&mut q.pending)
        };
        // Dropping a sender wakes its lookup.
        drop(waiting);
    }
}

/// Whether the OpenDAL reqwest transport can locate and sample kernel TCP statistics at request
/// end (requires safe descriptor-table lookup by `(local, remote)` address).
pub const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// Whether owned `tokio::net::TcpStream` connections report kernel TCP statistics (`TCP_INFO`,
/// `TCP_CONNECTION_INFO`, `SIO_TCP_INFO`).
pub const OWNED_SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
));

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod imp {
    use std::mem::{MaybeUninit, size_of};
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
    use std::os::fd::{AsRawFd, RawFd};

    use super::{ConnAddr, SockId};
    use crate::rows::TcpRow;

    fn to_addr(ss: &libc::sockaddr_storage) -> Option<SocketAddr> {
        match ss.ss_family as libc::c_int {
            libc::AF_INET => {
                // SAFETY: the family says the storage holds a sockaddr_in.
                let a = unsafe { &*(ss as *const _ as *const libc::sockaddr_in) };
                let ip = Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr));
                Some(SocketAddr::V4(SocketAddrV4::new(
                    ip,
                    u16::from_be(a.sin_port),
                )))
            }
            libc::AF_INET6 => {
                // SAFETY: the family says the storage holds a sockaddr_in6.
                let a = unsafe { &*(ss as *const _ as *const libc::sockaddr_in6) };
                let ip = Ipv6Addr::from(a.sin6_addr.s6_addr);
                Some(SocketAddr::V6(SocketAddrV6::new(
                    ip,
                    u16::from_be(a.sin6_port),
                    a.sin6_flowinfo,
                    a.sin6_scope_id,
                )))
            }
            _ => None,
        }
    }

    type NameFn =
        unsafe extern "C" fn(libc::c_int, *mut libc::sockaddr, *mut libc::socklen_t) -> libc::c_int;

    fn name_of(fd: RawFd, f: NameFn) -> Option<SocketAddr> {
        let mut ss = MaybeUninit::<libc::sockaddr_storage>::zeroed();
        let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        // SAFETY: read-only query into a buffer of the advertised size.
        let rc = unsafe { f(fd, ss.as_mut_ptr() as *mut libc::sockaddr, &mut len) };
        if rc != 0 {
            return None;
        }
        // SAFETY: zero-initialized and (partly) filled by the kernel.
        to_addr(unsafe { ss.assume_init_ref() })
    }

    /// The (local, remote) identity of descriptor `fd`, if it is a connected inet socket.
    fn identity(fd: RawFd) -> Option<(SocketAddr, SocketAddr)> {
        Some((
            name_of(fd, libc::getsockname)?,
            name_of(fd, libc::getpeername)?,
        ))
    }

    fn same(a: SocketAddr, b: SocketAddr) -> bool {
        // Compare address and port only (flow info / scope id may differ in how they are read).
        a.ip() == b.ip() && a.port() == b.port()
    }

    pub fn sock_id_of(s: &tokio::net::TcpStream) -> Option<SockId> {
        Some(SockId {
            fd: s.as_raw_fd() as i64,
            local: s.local_addr().ok()?,
            remote: s.peer_addr().ok()?,
        })
    }

    pub fn find_all(wanted: &[ConnAddr]) -> Vec<Option<SockId>> {
        let mut found = vec![None; wanted.len()];
        // `/dev/fd` lists this process's descriptors on Linux (→ /proc/self/fd) and macOS.
        let dir = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        let mut fds: Vec<RawFd> = entries
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        // High descriptors first: the low ones are mostly the process's own files and
        // listeners opened at start-up (the order only affects how soon the scan stops).
        fds.sort_unstable_by(|a, b| b.cmp(a));
        let mut missing = wanted.len();
        for fd in fds {
            if missing == 0 {
                break;
            }
            // One syscall per descriptor (the local address is nearly unique); the peer is
            // only asked of a match.
            let Some(local) = name_of(fd, libc::getsockname) else {
                continue;
            };
            for (i, &(l, r)) in wanted.iter().enumerate() {
                if found[i].is_none()
                    && same(l, local)
                    && name_of(fd, libc::getpeername).is_some_and(|peer| same(peer, r))
                {
                    found[i] = Some(SockId {
                        fd: fd as i64,
                        local: l,
                        remote: r,
                    });
                    missing -= 1;
                    break;
                }
            }
        }
        found
    }

    pub fn query(id: &SockId) -> Option<TcpRow> {
        let fd = RawFd::try_from(id.fd).ok()?;
        let row = raw(fd)?;
        // Only now check the descriptor still is this connection (see the module docs).
        let (l, r) = identity(fd)?;
        (same(l, id.local) && same(r, id.remote)).then_some(row)
    }

    fn getsockopt<T>(fd: RawFd, level: libc::c_int, name: libc::c_int) -> Option<(T, usize)> {
        let mut v = MaybeUninit::<T>::zeroed();
        let mut len = size_of::<T>() as libc::socklen_t;
        // SAFETY: read-only option query into a zeroed buffer of the advertised size; the
        // kernel writes at most `len` bytes and reports how many.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                level,
                name,
                v.as_mut_ptr() as *mut libc::c_void,
                &mut len,
            )
        };
        if rc != 0 {
            return None;
        }
        // SAFETY: zeroed POD, possibly partially filled (older kernels): fields beyond `len`
        // stay zero.
        Some((unsafe { v.assume_init() }, len as usize))
    }

    /// `struct tcp_info` of `linux/tcp.h`, up to `tcpi_sndbuf_limited` (Linux 4.10).
    #[cfg(target_os = "linux")]
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TcpInfo {
        state: u8,
        ca_state: u8,
        retransmits: u8,
        probes: u8,
        backoff: u8,
        options: u8,
        wscale: u8,
        flags: u8,
        rto: u32,
        ato: u32,
        snd_mss: u32,
        rcv_mss: u32,
        unacked: u32,
        sacked: u32,
        lost: u32,
        retrans: u32,
        fackets: u32,
        last_data_sent: u32,
        last_ack_sent: u32,
        last_data_recv: u32,
        last_ack_recv: u32,
        pmtu: u32,
        rcv_ssthresh: u32,
        rtt: u32,
        rttvar: u32,
        snd_ssthresh: u32,
        snd_cwnd: u32,
        advmss: u32,
        reordering: u32,
        rcv_rtt: u32,
        rcv_space: u32,
        total_retrans: u32,
        pacing_rate: u64,
        max_pacing_rate: u64,
        bytes_acked: u64,
        bytes_received: u64,
        segs_out: u32,
        segs_in: u32,
        notsent_bytes: u32,
        min_rtt: u32,
        data_segs_in: u32,
        data_segs_out: u32,
        delivery_rate: u64,
        busy_time: u64,
        rwnd_limited: u64,
        sndbuf_limited: u64,
    }

    #[cfg(all(test, target_os = "linux"))]
    pub fn layout() -> (usize, usize, usize) {
        (
            size_of::<TcpInfo>(),
            std::mem::offset_of!(TcpInfo, total_retrans),
            std::mem::offset_of!(TcpInfo, delivery_rate),
        )
    }

    #[cfg(target_os = "linux")]
    fn raw(fd: RawFd) -> Option<TcpRow> {
        let (i, len) = getsockopt::<TcpInfo>(fd, libc::IPPROTO_TCP, libc::TCP_INFO)?;
        if len < 104 {
            return None;
        }
        Some(TcpRow {
            rtt_us: i.rtt as u64,
            rttvar_us: i.rttvar as u64,
            retrans: i.total_retrans as u64,
            cwnd: i.snd_cwnd as u64,
            delivery_rate_bps: i.delivery_rate.saturating_mul(8),
            busy_us: i.busy_time,
            rwnd_limited_us: i.rwnd_limited,
            sndbuf_limited_us: i.sndbuf_limited,
            min_rtt_us: i.min_rtt as u64,
            notsent_bytes: i.notsent_bytes as u64,
            unacked_count: i.unacked as u64,
            lost_count: i.lost as u64,
            bytes_acked: i.bytes_acked,
            pacing_rate_bps: i.pacing_rate.saturating_mul(8),
            bytes_received: i.bytes_received,
        })
    }

    /// `struct tcp_connection_info` of macOS `netinet/tcp.h`.
    #[cfg(target_os = "macos")]
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TcpConnectionInfo {
        state: u8,
        snd_wscale: u8,
        rcv_wscale: u8,
        pad1: u8,
        options: u32,
        flags: u32,
        rto: u32,
        maxseg: u32,
        snd_ssthresh: u32,
        snd_cwnd: u32,
        snd_wnd: u32,
        snd_sbbytes: u32,
        rcv_wnd: u32,
        rttcur: u32,
        srtt: u32,
        rttvar: u32,
        tfo_bits: u32,
        txpackets: u64,
        txbytes: u64,
        txretransmitbytes: u64,
        rxpackets: u64,
        rxbytes: u64,
        rxoutoforderbytes: u64,
        txretransmitpackets: u64,
    }

    #[cfg(target_os = "macos")]
    const TCP_CONNECTION_INFO: libc::c_int = 0x106;

    #[cfg(target_os = "macos")]
    fn raw(fd: RawFd) -> Option<TcpRow> {
        let (i, len) = getsockopt::<TcpConnectionInfo>(fd, libc::IPPROTO_TCP, TCP_CONNECTION_INFO)?;
        if len < 52 {
            return None;
        }
        Some(TcpRow {
            // Reported in milliseconds.
            rtt_us: i.srtt as u64 * 1000,
            rttvar_us: i.rttvar as u64 * 1000,
            retrans: i.txretransmitpackets,
            // Bytes → segments.
            cwnd: (i.snd_cwnd / i.maxseg.max(1)) as u64,
            delivery_rate_bps: 0,
            bytes_acked: i.txbytes,
            bytes_received: i.rxbytes,
            ..Default::default()
        })
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::mem::{MaybeUninit, size_of};
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
    use std::os::windows::io::AsRawSocket;

    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, SIO_TCP_INFO, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE,
        SOCKET, TCP_INFO_v0, TCP_INFO_v1, WSAIoctl, getpeername, getsockname,
    };

    use super::SockId;
    use crate::rows::TcpRow;

    fn to_addr(ss: &SOCKADDR_STORAGE) -> Option<SocketAddr> {
        match ss.ss_family {
            AF_INET => {
                // SAFETY: family checked as AF_INET.
                let a = unsafe { &*(ss as *const _ as *const SOCKADDR_IN) };
                let ip = Ipv4Addr::from(u32::from_be(unsafe { a.sin_addr.S_un.S_addr }));
                Some(SocketAddr::V4(SocketAddrV4::new(
                    ip,
                    u16::from_be(a.sin_port),
                )))
            }
            AF_INET6 => {
                // SAFETY: family checked as AF_INET6.
                let a = unsafe { &*(ss as *const _ as *const SOCKADDR_IN6) };
                let ip = Ipv6Addr::from(unsafe { a.sin6_addr.u.Byte });
                Some(SocketAddr::V6(SocketAddrV6::new(
                    ip,
                    u16::from_be(a.sin6_port),
                    a.sin6_flowinfo,
                    unsafe { a.Anonymous.sin6_scope_id },
                )))
            }
            _ => None,
        }
    }

    type NameFn = unsafe extern "system" fn(SOCKET, *mut SOCKADDR, *mut i32) -> i32;

    fn name_of(sock: SOCKET, f: NameFn) -> Option<SocketAddr> {
        let mut ss = MaybeUninit::<SOCKADDR_STORAGE>::zeroed();
        let mut len = size_of::<SOCKADDR_STORAGE>() as i32;
        // SAFETY: read-only socket address query into an initialized buffer.
        let rc = unsafe { f(sock, ss.as_mut_ptr() as *mut SOCKADDR, &mut len) };
        if rc != 0 {
            return None;
        }
        to_addr(unsafe { ss.assume_init_ref() })
    }

    fn same(a: SocketAddr, b: SocketAddr) -> bool {
        a.ip() == b.ip() && a.port() == b.port()
    }

    pub fn sock_id_of(s: &tokio::net::TcpStream) -> Option<SockId> {
        Some(SockId {
            fd: s.as_raw_socket() as i64,
            local: s.local_addr().ok()?,
            remote: s.peer_addr().ok()?,
        })
    }

    pub fn find_all(wanted: &[super::ConnAddr]) -> Vec<Option<SockId>> {
        // Probing arbitrary Win32 HANDLE values with getsockname can block on synchronous
        // named pipes; Windows queries owned sockets (`sock_id_of`) only.
        vec![None; wanted.len()]
    }

    pub fn query(id: &SockId) -> Option<TcpRow> {
        let sock = SOCKET::try_from(id.fd).ok()?;
        let row = raw_v1(sock).or_else(|| raw_v0(sock))?;
        let l = name_of(sock, getsockname)?;
        let r = name_of(sock, getpeername)?;
        (same(l, id.local) && same(r, id.remote)).then_some(row)
    }

    fn raw_v1(sock: SOCKET) -> Option<TcpRow> {
        let ver: u32 = 1;
        let mut info = MaybeUninit::<TCP_INFO_v1>::zeroed();
        let mut returned: u32 = 0;
        // SAFETY: unprivileged read-only WSAIoctl(SIO_TCP_INFO, version=1) on Windows 10 1709+.
        let rc = unsafe {
            WSAIoctl(
                sock,
                SIO_TCP_INFO,
                &ver as *const u32 as *const _,
                size_of::<u32>() as u32,
                info.as_mut_ptr() as *mut _,
                size_of::<TCP_INFO_v1>() as u32,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc != 0 || (returned as usize) < size_of::<TCP_INFO_v0>() {
            return None;
        }
        let i = unsafe { info.assume_init() };
        let mss = u64::from(i.Mss.max(1));
        Some(TcpRow {
            rtt_us: u64::from(i.RttUs),
            rttvar_us: 0,
            retrans: u64::from(i.BytesRetrans).div_ceil(mss) + u64::from(i.SynRetrans),
            cwnd: (u64::from(i.Cwnd) / mss).max(1),
            delivery_rate_bps: 0,
            busy_us: 0,
            rwnd_limited_us: u64::from(i.SndLimTimeRwin) * 1000,
            sndbuf_limited_us: u64::from(i.SndLimTimeSnd) * 1000,
            min_rtt_us: u64::from(i.MinRttUs),
            notsent_bytes: 0,
            unacked_count: u64::from(i.BytesInFlight).div_ceil(mss),
            lost_count: u64::from(i.FastRetrans),
            bytes_acked: i.BytesOut,
            pacing_rate_bps: 0,
            bytes_received: i.BytesIn,
        })
    }

    fn raw_v0(sock: SOCKET) -> Option<TcpRow> {
        let ver: u32 = 0;
        let mut info = MaybeUninit::<TCP_INFO_v0>::zeroed();
        let mut returned: u32 = 0;
        // SAFETY: unprivileged read-only WSAIoctl(SIO_TCP_INFO, version=0).
        let rc = unsafe {
            WSAIoctl(
                sock,
                SIO_TCP_INFO,
                &ver as *const u32 as *const _,
                size_of::<u32>() as u32,
                info.as_mut_ptr() as *mut _,
                size_of::<TCP_INFO_v0>() as u32,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc != 0 || (returned as usize) < size_of::<TCP_INFO_v0>() {
            return None;
        }
        let i = unsafe { info.assume_init() };
        let mss = u64::from(i.Mss.max(1));
        Some(TcpRow {
            rtt_us: u64::from(i.RttUs),
            rttvar_us: 0,
            retrans: u64::from(i.BytesRetrans).div_ceil(mss) + u64::from(i.SynRetrans),
            cwnd: (u64::from(i.Cwnd) / mss).max(1),
            min_rtt_us: u64::from(i.MinRttUs),
            unacked_count: u64::from(i.BytesInFlight).div_ceil(mss),
            lost_count: u64::from(i.FastRetrans),
            bytes_acked: i.BytesOut,
            bytes_received: i.BytesIn,
            ..Default::default()
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod imp {
    use super::SockId;
    use crate::rows::TcpRow;

    pub fn sock_id_of(_: &tokio::net::TcpStream) -> Option<SockId> {
        None
    }
    pub fn find_all(wanted: &[super::ConnAddr]) -> Vec<Option<SockId>> {
        vec![None; wanted.len()]
    }
    pub fn query(_: &SockId) -> Option<TcpRow> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn layout_matches_the_kernel() {
        // Offsets of linux/tcp.h.
        let (size, total_retrans, delivery_rate) = imp::layout();
        assert_eq!((size, total_retrans, delivery_rate), (192, 100, 160));
    }

    #[tokio::test]
    async fn reads_live_connections_and_rejects_stale_ids() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let accept = tokio::spawn(async move { l.accept().await.unwrap().0 });
        let s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let server = accept.await.unwrap();
        let id = sock_id_of(&s);
        if !OWNED_SUPPORTED {
            assert!(id.is_none());
            return;
        }
        let id = id.unwrap();
        if SUPPORTED {
            // Found by identity like the OpenDAL-mode lookup does, next to one that is not
            // there.
            let nowhere: ConnAddr = (id.local, "127.0.0.1:1".parse().unwrap());
            let found = find_all(&[nowhere, (id.local, id.remote)]);
            assert_eq!(found, [None, Some(id)]);
        }
        let row = id.query().expect("tcp info of a live connection");
        assert!(row.cwnd > 0, "{row:?}");
        // A wrong identity on the same descriptor is rejected.
        let wrong = SockId {
            remote: "127.0.0.1:1".parse().unwrap(),
            ..id
        };
        assert!(wrong.query().is_none());
        drop(s);
        drop(server);
        // The descriptor is gone (or reused by something else): nothing is reported.
        assert!(id.query().is_none());
    }

    /// Connections dialed at once are all found, and every descriptor-table scan runs on the
    /// blocking pool: the runtime's worker thread makes none (9.1 scanned once per connection,
    /// on the worker).
    #[tokio::test]
    async fn sockets_are_found_off_the_worker_thread() {
        if !SUPPORTED {
            return;
        }
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let mut held = Vec::new();
        let mut wanted = Vec::new();
        for _ in 0..16 {
            let (client, server) = tokio::join!(tokio::net::TcpStream::connect(addr), l.accept());
            let client = client.unwrap();
            wanted.push((sock_id_of(&client).unwrap(), client));
            held.push(server.unwrap().0);
        }
        let finder = Arc::new(SockFinder::default());
        let before = SCANS.with(|n| n.get());
        let found = futures::future::join_all(
            wanted
                .iter()
                .map(|(id, _)| finder.find((id.local, id.remote))),
        )
        .await;
        for ((id, _), found) in wanted.iter().zip(found) {
            assert_eq!(found, Some(*id));
        }
        // This (current-thread) runtime's only worker is the test thread.
        assert_eq!(SCANS.with(|n| n.get()), before);
        // The finder went idle: a later connection starts a new scan and is found too.
        let (client, _server) = tokio::join!(tokio::net::TcpStream::connect(addr), l.accept());
        let client = client.unwrap();
        let id = sock_id_of(&client).unwrap();
        assert_eq!(finder.find((id.local, id.remote)).await, Some(id));
        assert_eq!(SCANS.with(|n| n.get()), before);
    }

    /// A runtime that is shutting down drops the scan without running it, and the finder
    /// outlives the runtime with its operator: a lookup on the runtime that is gone is
    /// answered (nothing found), one that waited for that scan from another runtime is
    /// answered by a scan of its own, and so is every later one (9.2 left the finder
    /// scanning: none of them ever returned).
    #[test]
    fn a_scan_dropped_by_its_runtime_gives_the_queue_back() {
        if !SUPPORTED {
            return;
        }
        let runtime = || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        };
        let finder = Arc::new(SockFinder::default());
        // What is spawned on the blocking pool of a runtime that shut down is dropped.
        let gone = runtime();
        let handle = gone.handle().clone();
        drop(gone);
        let nowhere: ConnAddr = (
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
        );
        let lost = {
            let _in_its_context = handle.enter();
            futures::FutureExt::now_or_never(finder.find(nowhere))
        };
        assert_eq!(lost, Some(None), "answered when its scan was dropped");

        let alive = runtime();
        alive.block_on(async {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (client, _server) =
                tokio::join!(tokio::net::TcpStream::connect(l.local_addr().unwrap()), l.accept());
            let client = client.unwrap();
            let id = sock_id_of(&client).unwrap();
            let bound = std::time::Duration::from_secs(120);
            let never_stuck = |lookup| tokio::time::timeout(bound, lookup);

            // A lookup that waits for a scan queued on the runtime that is gone: when that
            // scan is dropped, it asks again and this runtime scans.
            let (tx, rx) = tokio::sync::oneshot::channel();
            {
                let mut q = finder.queue();
                q.pending.push((nowhere, tx));
                q.scanning = true;
            }
            let mut waiting = Box::pin(finder.find((id.local, id.remote)));
            assert!(futures::poll!(&mut waiting).is_pending());
            drop(Scan {
                finder: finder.clone(),
                done: false,
            });
            assert!(rx.await.is_err(), "the lookup of the lost runtime wakes too");
            let found = never_stuck(waiting).await.expect("the finder is not stuck");
            assert_eq!(found, Some(id));
            // And the finder is idle again: the next lookup starts its own scan.
            let found = never_stuck(Box::pin(finder.find((id.local, id.remote)))).await;
            assert_eq!(found.expect("the finder is not stuck"), Some(id));
        });
    }

    /// Whatever the order in which the requests of a shared connection start, send and end,
    /// and however the readings of its counter interleave, the retransmits charged to them
    /// add up to what the counter grew by: nothing is charged twice, nothing is lost.
    #[test]
    fn retransmits_before_a_late_lookup_go_to_the_first_request() {
        // The lookup of a fresh connection finishes while its first request is in flight,
        // after 3 retransmits: they are that request's, not a baseline.
        let mut cold = RetransLedger::for_connection(Some(3), false);
        let s = cold.open();
        assert_eq!(cold.close(s, Some(4)), 4);
        // A pooled connection found again: what it retransmitted before is nobody's.
        let mut warm = RetransLedger::for_connection(Some(3), true);
        let s = warm.open();
        assert_eq!(warm.close(s, Some(4)), 1);
    }

    #[test]
    fn retransmits_of_a_shared_connection_are_charged_exactly_once() {
        use rand::{RngExt, SeedableRng};
        let mut rng = rand::rngs::SmallRng::seed_from_u64(0x0b5e_55ed);
        for _ in 0..400 {
            let seed = rng.random_range(0..50u64);
            let mut ledger = RetransLedger::seeded(Some(seed));
            let mut kernel = seed;
            let mut open: Vec<u64> = Vec::new();
            let mut charged = 0u64;
            for _ in 0..rng.random_range(1..80) {
                kernel += rng.random_range(0..4u64);
                // Two requests ending at once may hand in their readings out of order, and
                // a connection that just closed cannot be read at all.
                let stale = rng.random_bool(0.2).then(|| rng.random_range(seed..=kernel));
                let reading = rng.random_bool(0.9).then_some(stale.unwrap_or(kernel));
                let pick = rng.random_range(0..open.len().max(1));
                match rng.random_range(0..10) {
                    _ if open.is_empty() => open.push(ledger.open()),
                    0..=2 => open.push(ledger.open()),
                    // An upload whose connection is known while it sends.
                    3..=4 => open.push(ledger.open_sending()),
                    // Response headers (an upload's, or again: nothing is settled twice).
                    5 => ledger.sent(open[pick], reading),
                    // The connection was guessed wrong: what it was charged is owed again.
                    6 => ledger.abandon(open.swap_remove(pick)),
                    _ => charged += ledger.close(open.swap_remove(pick), reading),
                }
                // At every step: what is charged, to the requests that ended and to the
                // ones in flight, is what the readings showed.
                let in_flight: u64 = ledger.open.iter().map(|s| s.charged).sum();
                assert_eq!(charged + in_flight, ledger.seen.unwrap() - seed);
            }
            // One last request reads the counter as it is: everything is charged.
            open.push(ledger.open());
            for stream in open {
                charged += ledger.close(stream, Some(kernel));
            }
            assert_eq!(charged, kernel - seed);
        }

        // An upload retransmits while a small GET on its HTTP/2 connection ends: the
        // retransmits are the upload's, not the GET's (9.2 opened a stream at its response
        // headers, so the GET, alone past its headers, was charged all five).
        let mut h2 = RetransLedger::seeded(Some(0));
        let get = h2.open();
        let put = h2.open_sending();
        assert_eq!(h2.close(get, Some(5)), 0);
        // Two more until the upload's response headers; past them it shares like any other.
        h2.sent(put, Some(7));
        let other = h2.open();
        assert_eq!(h2.close(other, Some(10)), 2);
        assert_eq!(h2.close(put, Some(10)), 5 + 2 + 1);
        // Two uploads share what is retransmitted while both send.
        let mut h2 = RetransLedger::seeded(Some(0));
        let (get, a, b) = (h2.open(), h2.open_sending(), h2.open_sending());
        assert_eq!(h2.close(get, Some(5)), 0);
        assert_eq!((h2.close(a, Some(5)), h2.close(b, Some(5))), (3, 2));
        // A stream opened on the wrong connection gives back what it was charged.
        let mut h1 = RetransLedger::seeded(Some(0));
        let (get, guess) = (h1.open(), h1.open_sending());
        h1.sent(guess, Some(4));
        h1.abandon(guess);
        assert_eq!(h1.close(get, Some(4)), 4);

        // One request at a time (HTTP/1.1): each is charged what happened since the last.
        let mut h1 = RetransLedger::seeded(Some(10));
        let a = h1.open();
        assert_eq!(h1.close(a, Some(13)), 3);
        let b = h1.open();
        assert_eq!(h1.close(b, Some(13)), 0);
        // Three streams share seven retransmits: two each, and the rest to the one ending.
        let mut h2 = RetransLedger::seeded(Some(0));
        let (a, b, c) = (h2.open(), h2.open(), h2.open());
        assert_eq!(h2.close(b, Some(7)), 3);
        // A stale reading (taken before `b`'s) settles nothing more.
        assert_eq!(h2.close(a, Some(5)), 2);
        assert_eq!(h2.close(c, Some(8)), 3);
    }
}
