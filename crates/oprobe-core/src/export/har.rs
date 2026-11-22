//! HAR 1.2: one entry per backend request, timings mapped from the request stages:
//! `blocked` = queue, `dns`, `connect` = TCP + proxy + TLS (HAR counts `ssl` inside `connect`),
//! `ssl` = TLS, `send`, `wait` = TTFB, `receive` = body + consumer stall. Stages a request did
//! not have (reused connections) are `-1`, as the format requires.

use serde_json::{Value, json};

use super::{abs_ns, object_url, ops_by_id, rfc3339_ms, window_start_unix_ns};
use crate::model::{Run, WireProtocol};

fn ms(us: u64) -> f64 {
    (us as f64 / 1000.0 * 1000.0).round() / 1000.0
}

fn opt_ms(us: u64, present: bool) -> f64 {
    if present { ms(us) } else { -1.0 }
}

fn status_text(code: u16) -> &'static str {
    match code {
        0 => "",
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        412 => "Precondition Failed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// The HAR document of a run.
pub fn har(run: &Run) -> Value {
    let r = &run.report;
    let start = window_start_unix_ns(r);
    let ops = ops_by_id(run);
    let entries: Vec<Value> = run
        .reqs
        .iter()
        .map(|q| {
            let op = ops.get(&q.op_id);
            let http = match q.protocol {
                WireProtocol::H2 => "HTTP/2",
                WireProtocol::H1 => "HTTP/1.1",
                WireProtocol::Inproc => "inproc",
            };
            let dialed = !q.reused && q.connect_us() > 0;
            let receive = q.body_us + q.stall_us;
            let mut entry = json!({
                "pageref": "run",
                "startedDateTime": rfc3339_ms(abs_ns(start, q.t_us)),
                "time": ms(q.total_us),
                "request": {
                    "method": q.method,
                    "url": op.map(|o| object_url(r, &o.key)).unwrap_or_else(|| object_url(r, "")),
                    "httpVersion": http,
                    "cookies": [],
                    "headers": [],
                    "queryString": [],
                    "headersSize": -1,
                    "bodySize": q.bytes_up,
                },
                "response": {
                    "status": q.status,
                    "statusText": status_text(q.status),
                    "httpVersion": http,
                    "cookies": [],
                    "headers": [],
                    "content": { "size": q.bytes_down, "mimeType": "application/octet-stream" },
                    "redirectURL": "",
                    "headersSize": -1,
                    "bodySize": q.bytes_down,
                },
                "cache": {},
                "timings": {
                    "blocked": ms(q.queue_us),
                    "dns": opt_ms(q.dns_us, dialed && q.dns_us > 0),
                    "connect": opt_ms(q.tcp_us + q.proxy_us + q.tls_us, dialed),
                    "ssl": opt_ms(q.tls_us, dialed && q.tls_us > 0),
                    "send": ms(q.send_us),
                    "wait": ms(q.ttfb_us),
                    "receive": ms(receive),
                },
                "connection": q.conn_id.to_string(),
                "_oprobe": {
                    "api": q.api,
                    "op_id": q.op_id,
                    "attempt": q.attempt,
                    "reused": q.reused,
                    "proxy_ms": ms(q.proxy_us),
                    "stall_ms": ms(q.stall_us),
                    "request_id": q.request_id,
                    "error": q.error,
                },
            });
            if let Some(a) = q.remote {
                entry["serverIPAddress"] = json!(a.ip().to_string());
            }
            entry
        })
        .collect();
    json!({
        "log": {
            "version": "1.2",
            "creator": { "name": "oprobe", "version": env!("CARGO_PKG_VERSION") },
            "pages": [{
                "startedDateTime": rfc3339_ms(start),
                "id": "run",
                "title": format!("oprobe run {} · {}", r.run.id, crate::util::display_url(&r.target.display)),
                "pageTimings": {},
            }],
            "entries": entries,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::simulate::tests::straggler_run;

    #[test]
    fn entries_map_stages_to_har_timings() {
        let mut run = straggler_run();
        run.report.run.ended_at = "2026-11-20T14:30:10Z".into();
        run.report.run.window_s = 5.0;
        run.reqs[0].reused = false;
        run.reqs[0].dns_us = 2000;
        run.reqs[0].tcp_us = 3000;
        run.reqs[0].tls_us = 4000;
        run.reqs[0].queue_us = 500;
        let v = har(&run);
        let e = &v["log"]["entries"];
        assert_eq!(e.as_array().unwrap().len(), run.reqs.len());
        let t = &e[0]["timings"];
        assert_eq!(t["blocked"], 0.5);
        assert_eq!(t["dns"], 2.0);
        assert_eq!(t["connect"], 7.0);
        assert_eq!(t["ssl"], 4.0);
        assert_eq!(e[1]["timings"]["connect"], -1.0);
        assert_eq!(e[0]["startedDateTime"], "2026-11-20T14:30:05.000Z");
        assert_eq!(e[0]["request"]["url"], "s3://bkt/x/k1");
        assert_eq!(v["log"]["version"], "1.2");
    }
}
