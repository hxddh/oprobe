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
//! Cost: three syscalls per request (≈1–3 µs), plus, in OpenDAL mode, one scan of the
//! process's descriptor table the first time a connection finishes a request.

use std::net::SocketAddr;

use crate::model::TcpRow;

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

/// The identity of an owned stream (microscope transport).
pub fn sock_id_of(s: &tokio::net::TcpStream) -> Option<SockId> {
    imp::sock_id_of(s)
}

/// Find the descriptor of the connection `local → remote` in this process (OpenDAL mode: the
/// socket lives inside reqwest). `None` when not found or not supported.
pub fn find(local: SocketAddr, remote: SocketAddr) -> Option<SockId> {
    imp::find(local, remote)
}

/// Whether this platform reports TCP statistics.
pub const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod imp {
    use std::mem::{MaybeUninit, size_of};
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
    use std::os::fd::{AsRawFd, RawFd};

    use super::SockId;
    use crate::model::TcpRow;

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

    pub fn find(local: SocketAddr, remote: SocketAddr) -> Option<SockId> {
        // `/dev/fd` lists this process's descriptors on Linux (→ /proc/self/fd) and macOS.
        let dir = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        let mut fds: Vec<RawFd> = std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        // Newest descriptors first: the connection was just opened.
        fds.sort_unstable_by(|a, b| b.cmp(a));
        fds.into_iter()
            .find(|&fd| identity(fd).is_some_and(|(l, r)| same(l, local) && same(r, remote)))
            .map(|fd| SockId {
                fd: fd as i64,
                local,
                remote,
            })
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
            ..Default::default()
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use std::net::SocketAddr;

    use super::SockId;
    use crate::model::TcpRow;

    pub fn sock_id_of(_: &tokio::net::TcpStream) -> Option<SockId> {
        None
    }
    pub fn find(_: SocketAddr, _: SocketAddr) -> Option<SockId> {
        None
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
        if !SUPPORTED {
            assert!(id.is_none());
            return;
        }
        let id = id.unwrap();
        // Found by identity like the OpenDAL-mode lookup does.
        let found = find(id.local, id.remote).expect("socket found by its addresses");
        assert_eq!(found.fd, id.fd);
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
}
