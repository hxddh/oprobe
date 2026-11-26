//! Observability depth against the local lab over real sockets: exact API names from OpenDAL's
//! `ServiceOperation`, TLS handshake details and kernel TCP statistics in the fidelity transport.

use std::sync::Arc;
use std::time::Duration;

use oprobe::config::*;
use oprobe::engine::{Cancel, RunOptions, Session};
use oprobe::lab::{LabConfig, LabServer};
use oprobe::model::ReqRow;
use oprobe::util::ByteSize;

fn quiet() -> oprobe::engine::ProgressFn {
    Arc::new(|_| {})
}

fn opts(ms: u64) -> RunOptions {
    RunOptions {
        duration: Duration::from_millis(ms),
        warmup: Duration::ZERO,
        max_ops: None,
        rate: None,
        trace_slow_ops: false,
    }
}

async fn rows(mut p: Profile, ms: u64) -> Vec<ReqRow> {
    p.workload.preset = Preset::SmallFiles;
    p.workload.dataset = Some(DatasetSpec {
        objects: 4,
        object_size: ByteSize::kib(64),
        partitions: 2,
        ..Default::default()
    });
    p.workload.mix = vec![
        OpSpec {
            weight: 2,
            op: OpKind::Read { size: None },
        },
        OpSpec {
            weight: 1,
            op: OpKind::Write {
                size: ByteSize::kib(16),
            },
        },
        OpSpec {
            weight: 1,
            op: OpKind::List { recursive: false },
        },
    ];
    p.workload.concurrency = 2;
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let knobs = oprobe::config::Knobs::of(&s.profile);
    s.run(&knobs, &opts(ms), None).await.unwrap();
    let trace = s.take_trace().unwrap();
    s.close().await;
    trace.reqs
}

async fn depth_in() {
    let mode = "opendal";
    let lab = LabServer::spawn("127.0.0.1:0", LabConfig::clean_https_h2())
        .await
        .unwrap();
    let mut p = lab.profile("bkt");
    p.operator.http.version = oprobe::net::HttpVersion::H1;
    // Every request dials: handshakes are full first, resumed afterwards.
    p.operator.http.keep_alive = false;
    let reqs = rows(p, 600).await;
    assert!(!reqs.is_empty());
    let apis: std::collections::BTreeSet<&str> = reqs.iter().map(|r| r.api_name()).collect();
    // OpenDAL's own names, not the URL heuristic's (S3 lists with ListObjectsV2).
    assert!(apis.contains("ListObjectsV2"), "{mode:?}: {apis:?}");
    assert!(
        apis.contains("GetObject") && apis.contains("PutObject"),
        "{apis:?}"
    );

    let tls: Vec<_> = reqs.iter().filter_map(|r| r.tls.as_deref()).collect();
    assert!(!tls.is_empty(), "{mode:?}: no handshake details");
    for t in &tls {
        assert_eq!(t.version, "TLS1.3", "{t:?}");
        assert!(
            t.cipher.as_deref().is_some_and(|c| c.starts_with("TLS13_")),
            "{t:?}"
        );
        assert!(t.group.is_some(), "{t:?}");
        assert!(!t.hrr, "{t:?}");
    }
    assert!(tls.iter().any(|t| !t.resumed), "{mode:?}: a full handshake");
    assert!(
        tls.iter().any(|t| t.resumed),
        "{mode:?}: resumed handshakes"
    );
    // Only requests that dialed carry handshake details.
    assert!(reqs.iter().all(|r| r.tls.is_none() || !r.reused));

    let h = oprobe::net::stats::tls_handshakes(&reqs);
    assert!(
        h.resumption_ratio > 0.0 && h.resumption_ratio < 1.0,
        "{h:?}"
    );

    // Kernel TCP statistics at request end, on pooled connections (a connection closed with
    // its request may already be gone by then).
    let p = lab.profile("bkt");
    let reqs = rows(p, 300).await;
    if oprobe::net::tcpinfo::SUPPORTED {
        // A connection is sampled once its socket has been found, which happens beside its
        // requests (9.3: no request waits for the lookup). A connection that carried a request
        // or two before the lookup answered has none (Linux CI: one of four), so the claim is
        // about the connection that carried the most requests: it outlasts any lookup.
        let mut per_conn: std::collections::BTreeMap<u64, (usize, usize)> = Default::default();
        for r in &reqs {
            let e = per_conn.entry(r.conn_id).or_default();
            e.0 += 1;
            e.1 += usize::from(r.tcp.is_some());
        }
        let (busiest, (carried, sampled)) = per_conn
            .iter()
            .max_by_key(|(_, (n, _))| *n)
            .map(|(id, counts)| (*id, *counts))
            .unwrap();
        assert!(
            sampled > 0,
            "{mode:?}: connection {busiest} carried {carried} requests and none was sampled: {per_conn:?}"
        );
        let t = reqs.iter().find_map(|r| r.tcp.as_deref()).unwrap();
        assert!(t.cwnd > 0 && t.rtt_us > 0, "{t:?}");
        let k = oprobe::net::stats::tcp_kernel(&reqs).unwrap();
        assert!(k.connections_count >= 1 && k.rtt_p50_us > 0, "{k:?}");
    } else {
        assert!(reqs.iter().all(|r| r.tcp.is_none()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn opendal_transport_reports_api_names_tls_and_tcp() {
    depth_in().await;
}
