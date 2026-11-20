//! Observability depth against the local lab over real sockets: exact API names from OpenDAL's
//! `ServiceOperation`, TLS handshake details and kernel TCP statistics in both transports.

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
    p.workload.preset = Preset::Mixed;
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
    p.knobs.concurrency = 2;
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let knobs = s.profile.knobs.clone();
    s.run(&knobs, &opts(ms), None).await.unwrap();
    let trace = s.take_trace().unwrap();
    s.close().await;
    trace.reqs
}

async fn depth_in(mode: TransportMode) {
    let lab = LabServer::spawn("127.0.0.1:0", LabConfig::clean_https_h2())
        .await
        .unwrap();
    let mut p = lab.profile("bkt");
    p.knobs.transport = mode;
    p.knobs.http_version = oprobe::net::HttpVersion::H1;
    // Every request dials: handshakes are full first, resumed afterwards.
    p.knobs.keep_alive = false;
    let reqs = rows(p, 600).await;
    assert!(!reqs.is_empty());
    let apis: std::collections::BTreeSet<&str> = reqs.iter().map(|r| r.api.as_ref()).collect();
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
    let mut p = lab.profile("bkt");
    p.knobs.transport = mode;
    let reqs = rows(p, 300).await;
    if oprobe::net::tcpinfo::SUPPORTED {
        let with_tcp = reqs.iter().filter(|r| r.tcp.is_some()).count();
        assert!(
            with_tcp * 10 >= reqs.len() * 9,
            "{mode:?}: {with_tcp}/{}",
            reqs.len()
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
    depth_in(TransportMode::Opendal).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn microscope_transport_reports_api_names_tls_and_tcp() {
    depth_in(TransportMode::Microscope).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_reports_tls_chain_permissions_and_capabilities() {
    use oprobe::cli::doctor::{Access, doctor};
    let lab = LabServer::spawn("127.0.0.1:0", LabConfig::clean_https_h2())
        .await
        .unwrap();
    let t = lab.profile("bkt").target.normalized().unwrap();
    let d = doctor(t.clone(), false, "mylab").await;
    assert!(d.ready, "{:#?}", d.checks);
    let tls = d.tls.as_ref().expect("tls details");
    assert_eq!(tls.version, "TLS1.3");
    assert!(tls.resumption, "the lab resumes sessions");
    assert!(tls.group.is_some());
    let chain = tls.chain.as_ref().expect("certificate chain");
    assert_eq!(chain.certs[0].subject, "localhost");
    assert_eq!(chain.certs[0].issuer, "oprobe lab CA (generated)");
    assert!(chain.min_expiry_days().unwrap() > 20);
    assert_eq!(d.credentials.source, "options");
    assert_eq!(d.addressing.as_deref(), Some("path"));
    assert_eq!(d.dns.as_ref().unwrap().a, vec!["127.0.0.1".to_owned()]);
    assert!(d.clock_skew_s.is_some());
    let p = &d.permissions;
    for a in [p.list, p.read, p.write, p.stat, p.delete] {
        assert_eq!(a, Access::Allowed);
    }
    assert!(d.capabilities.copy && d.capabilities.read_with_suffix);
    assert_eq!(d.capabilities.delete_max_count, 1000);
    assert!(
        d.next_command.starts_with("oprobe run mylab"),
        "{}",
        d.next_command
    );
    let v = serde_json::to_value(&d).unwrap();
    assert!(v["capabilities"]["write_with_if_match"].as_bool().unwrap());

    let ro = doctor(t, true, "mylab").await;
    assert_eq!(ro.permissions.write, Access::Skipped);
    assert!(
        ro.next_command.contains("--existing"),
        "{}",
        ro.next_command
    );
}
