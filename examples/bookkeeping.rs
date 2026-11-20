//! Cost of oprobe's per-operation bookkeeping without any I/O.
use oprobe::trace::*;
use std::time::Instant;

fn main() {
    let rec = Recorder::new();
    rec.set_phase(Phase::Measure);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let n = 200_000u64;
    let t = Instant::now();
    rt.block_on(async {
        for _ in 0..n {
            let s0 = Instant::now();
            let scope = OpScope::new(true);
            rec.op_started(scope.id);
            scope
                .run(async {
                    let api = classify_http(&http::Method::GET, "/bench/overhead/16384/7", None);
                    rec.request_started();
                    let r = RequestRecord {
                        api,
                        method: "GET".to_string(),
                        status: 200,
                        protocol: Protocol::Http1,
                        reused: true,
                        conn_id: 3,
                        start_offset_us: 1,
                        timing: RequestTiming {
                            send_us: 3,
                            ttfb_us: 500,
                            body_us: 20,
                            total_us: 530,
                            ..Default::default()
                        },
                        bytes_up: 0,
                        bytes_down: 16384,
                        request_id: Some("oprobe-s3-req-000123".into()),
                        retry_after_ms: None,
                        error: None,
                        remote: None,
                    };
                    rec.request_finished();
                    OpScope::current().unwrap().push_request(r);
                })
                .await;
            rec.complete_op(
                true,
                OpCompletion {
                    verb: "read.full",
                    op_id: scope.id,
                    started: s0,
                    latency_us: 600,
                    logical_read: 16384,
                    logical_write: 0,
                    error: None,
                    key: String::new(),
                    data: scope.take(),
                },
            );
        }
    });
    let ns = t.elapsed().as_nanos() as f64 / n as f64;
    println!("bookkeeping per operation (1 request): {ns:.0} ns");
}
