//! Report aggregates of the per-request TLS and TCP details (`network.tls.handshakes`,
//! `network.tcp.kernel`), computed from the trace rows (exact unless the run was sampled).

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::rows::ReqRow;

/// TLS handshakes of the window (one per dialed connection).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct TlsHandshakeStats {
    pub handshakes_count: u64,
    pub full_count: u64,
    pub resumed_count: u64,
    /// Handshakes that needed a HelloRetryRequest (an extra round trip: the client's first key
    /// share was not one the server accepts).
    pub hrr_count: u64,
    /// resumed / handshakes.
    pub resumption_ratio: f64,
    pub versions_count: BTreeMap<String, u64>,
    pub ciphers_count: BTreeMap<String, u64>,
    pub groups_count: BTreeMap<String, u64>,
}

/// Kernel TCP statistics (`TCP_INFO`) over the requests of the window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct TcpKernelStats {
    /// Requests that carried kernel statistics.
    pub samples_count: u64,
    pub connections_count: u64,
    pub rtt_p50_us: u64,
    pub rtt_p99_us: u64,
    pub rttvar_p50_us: u64,
    /// Segments retransmitted, summed over connections (their last sample).
    pub retrans_count: u64,
    /// Connections that retransmitted at least once.
    pub retransmitting_connections_count: u64,
    pub cwnd_p50_count: u64,
    pub delivery_rate_p50_bits_per_s: u64,
    /// Share of the busy (sending) time limited by the receive window / send buffer.
    pub rwnd_limited_ratio: f64,
    pub sndbuf_limited_ratio: f64,
}

pub fn tls_handshakes(reqs: &[ReqRow]) -> TlsHandshakeStats {
    let mut s = TlsHandshakeStats::default();
    for t in reqs.iter().filter_map(|r| r.tls.as_deref()) {
        s.handshakes_count += 1;
        if t.resumed {
            s.resumed_count += 1;
        } else {
            s.full_count += 1;
        }
        if t.hrr {
            s.hrr_count += 1;
        }
        *s.versions_count.entry(t.version.clone()).or_default() += 1;
        if let Some(c) = &t.cipher {
            *s.ciphers_count.entry(c.clone()).or_default() += 1;
        }
        if let Some(g) = &t.group {
            *s.groups_count.entry(g.clone()).or_default() += 1;
        }
    }
    if s.handshakes_count > 0 {
        s.resumption_ratio = s.resumed_count as f64 / s.handshakes_count as f64;
    }
    s
}

fn pct(v: &mut [u64], q: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// `None` when no request carried kernel statistics (in-process backends, Windows).
pub fn tcp_kernel(reqs: &[ReqRow]) -> Option<TcpKernelStats> {
    let rows: Vec<(&ReqRow, &crate::rows::TcpRow)> = reqs
        .iter()
        .filter_map(|r| r.tcp.as_deref().map(|t| (r, t)))
        .collect();
    if rows.is_empty() {
        return None;
    }
    let mut rtt: Vec<u64> = rows.iter().map(|(_, t)| t.rtt_us).collect();
    let mut rttvar: Vec<u64> = rows.iter().map(|(_, t)| t.rttvar_us).collect();
    let mut cwnd: Vec<u64> = rows.iter().map(|(_, t)| t.cwnd).collect();
    let mut rate: Vec<u64> = rows
        .iter()
        .map(|(_, t)| t.delivery_rate_bps)
        .filter(|&r| r > 0)
        .collect();
    // Counters are cumulative per connection: keep each connection's latest sample.
    let mut last: HashMap<u64, (i64, &crate::rows::TcpRow)> = HashMap::new();
    for (r, t) in &rows {
        let e = last.entry(r.conn_id).or_insert((r.t_us, t));
        if r.t_us >= e.0 {
            *e = (r.t_us, t);
        }
    }
    let busy: u64 = last.values().map(|(_, t)| t.busy_us).sum();
    let ratio = |v: u64| {
        if busy > 0 {
            v as f64 / busy as f64
        } else {
            0.0
        }
    };
    Some(TcpKernelStats {
        samples_count: rows.len() as u64,
        connections_count: last.len() as u64,
        rtt_p50_us: pct(&mut rtt, 0.5),
        rtt_p99_us: pct(&mut rtt, 0.99),
        rttvar_p50_us: pct(&mut rttvar, 0.5),
        retrans_count: last.values().map(|(_, t)| t.retrans).sum(),
        retransmitting_connections_count: last.values().filter(|(_, t)| t.retrans > 0).count()
            as u64,
        cwnd_p50_count: pct(&mut cwnd, 0.5),
        delivery_rate_p50_bits_per_s: pct(&mut rate, 0.5),
        rwnd_limited_ratio: ratio(last.values().map(|(_, t)| t.rwnd_limited_us).sum()),
        sndbuf_limited_ratio: ratio(last.values().map(|(_, t)| t.sndbuf_limited_us).sum()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{TcpRow, TlsRow};

    fn row(conn: u64, t: i64) -> ReqRow {
        crate::log::req_row(
            crate::RequestRecord {
                operation: None,
                api: crate::Api::GetObject,
                method: "GET".into(),
                status: 200,
                protocol: crate::Protocol::Http1,
                reused: true,
                conn_id: conn,
                start_offset_us: 0,
                timing: Default::default(),
                bytes_up: 0,
                bytes_down: 0,
                request_id: None,
                retry_after_ms: None,
                error: None,
                remote: None,
                server: None,
                service_op: Some("GetObject"),
                service: None,
                tls: None,
                tcp: None,
                detail: None,
            },
            1,
            t,
            1,
        )
    }

    #[test]
    fn aggregates_handshakes_and_kernel_stats() {
        let mut a = row(1, 0);
        a.tls = Some(Box::new(TlsRow {
            resumed: false,
            version: "TLS1.3".into(),
            cipher: Some("TLS13_AES_128_GCM_SHA256".into()),
            hrr: true,
            group: Some("X25519".into()),
        }));
        a.tcp = Some(Box::new(TcpRow {
            rtt_us: 100,
            retrans: 1,
            cwnd: 10,
            busy_us: 1000,
            rwnd_limited_us: 500,
            ..Default::default()
        }));
        let mut b = row(1, 5);
        b.tcp = Some(Box::new(TcpRow {
            rtt_us: 300,
            retrans: 2,
            cwnd: 20,
            busy_us: 2000,
            rwnd_limited_us: 1000,
            ..Default::default()
        }));
        let mut c = row(2, 1);
        c.tls = Some(Box::new(TlsRow {
            resumed: true,
            version: "TLS1.3".into(),
            ..Default::default()
        }));
        let h = tls_handshakes(&[a.clone(), b.clone(), c.clone()]);
        assert_eq!(
            (h.handshakes_count, h.resumed_count, h.hrr_count),
            (2, 1, 1)
        );
        assert_eq!(h.resumption_ratio, 0.5);
        assert_eq!(h.versions_count["TLS1.3"], 2);
        let k = tcp_kernel(&[a, b, c]).unwrap();
        assert_eq!(k.samples_count, 2);
        assert_eq!(k.connections_count, 1);
        // The connection's latest counters, not a sum of cumulative samples.
        assert_eq!(k.retrans_count, 2);
        assert_eq!(k.rtt_p99_us, 300);
        assert_eq!(k.rwnd_limited_ratio, 0.5);
        assert!(tcp_kernel(&[row(3, 0)]).is_none());
        assert_eq!(row(3, 0).api, crate::Api::GetObject);
        assert_eq!(row(3, 0).api_name(), "GetObject");
    }
}
