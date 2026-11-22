//! CSV of a run's request rows or operation rows (RFC 4180 quoting, a header line).

use std::fmt::Write as _;

use crate::model::Run;

fn field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

/// One line per request.
pub fn requests(run: &Run) -> String {
    let mut o = String::from(
        "t_us,op_id,attempt,api,method,status,protocol,reused,conn_id,remote,queue_us,dns_us,tcp_us,proxy_us,tls_us,send_us,ttfb_us,body_us,stall_us,total_us,bytes_up,bytes_down,request_id,retry_after_ms,error_code,tcp_rtt_us,tcp_retrans\n",
    );
    for q in &run.reqs {
        let _ = writeln!(
            o,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            q.t_us,
            q.op_id,
            q.attempt,
            field(&q.api),
            field(&q.method),
            q.status,
            serde_json::to_value(q.protocol)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            q.reused,
            q.conn_id,
            q.remote.map(|a| a.to_string()).unwrap_or_default(),
            q.queue_us,
            q.dns_us,
            q.tcp_us,
            q.proxy_us,
            q.tls_us,
            q.send_us,
            q.ttfb_us,
            q.body_us,
            q.stall_us,
            q.total_us,
            q.bytes_up,
            q.bytes_down,
            field(q.request_id.as_deref().unwrap_or("")),
            q.retry_after_ms.map(|v| v.to_string()).unwrap_or_default(),
            q.error_code.map(|c| c.as_str()).unwrap_or(""),
            q.tcp
                .as_ref()
                .map(|t| t.rtt_us.to_string())
                .unwrap_or_default(),
            q.tcp
                .as_ref()
                .map(|t| t.retrans.to_string())
                .unwrap_or_default(),
        );
    }
    o
}

/// One line per operation.
pub fn operations(run: &Run) -> String {
    let mut o = String::from(
        "op_id,verb,key,t_us,latency_us,bytes,ok,error_code,throttled,attempts,requests,queue_us,cache_us,backend_us,retry_us,client_us\n",
    );
    for x in &run.ops {
        let b = &x.breakdown;
        let _ = writeln!(
            o,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            x.op_id,
            field(&x.verb),
            field(&x.key),
            x.t_us,
            x.latency_us,
            x.bytes,
            x.ok,
            x.error_code.map(|c| c.as_str()).unwrap_or(""),
            x.throttled,
            x.attempts,
            x.requests,
            b.queue_us,
            b.cache_us,
            b.backend_us,
            b.retry_us,
            b.client_us
        );
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::simulate::tests::straggler_run;

    #[test]
    fn csv_has_a_header_and_one_line_per_row() {
        let mut run = straggler_run();
        run.ops[0].key = "a,\"b\"".into();
        let r = requests(&run);
        assert_eq!(r.lines().count(), run.reqs.len() + 1);
        let cols = r.lines().next().unwrap().split(',').count();
        assert_eq!(r.lines().nth(1).unwrap().split(',').count(), cols);
        let o = operations(&run);
        assert!(o.lines().nth(1).unwrap().contains("\"a,\"\"b\"\"\""));
    }
}
