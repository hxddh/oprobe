//! The Requests table: its columns, the filter language of its search box, sorting and export.
//! Works on indices into the run's rows, so 100k+ requests filter and sort without copies.

use std::cmp::Ordering;

use crate::model::{ReqRow, WireProtocol};

use super::fmt;

/// A column of the Requests table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Col {
    T,
    Api,
    Status,
    Total,
    Queue,
    Connect,
    Send,
    Ttfb,
    Body,
    Down,
    Up,
    Conn,
    Remote,
    Proto,
    Attempt,
    Op,
    Error,
    RequestId,
}

impl Col {
    pub const ALL: [Col; 18] = [
        Col::T,
        Col::Api,
        Col::Status,
        Col::Total,
        Col::Queue,
        Col::Connect,
        Col::Send,
        Col::Ttfb,
        Col::Body,
        Col::Down,
        Col::Up,
        Col::Conn,
        Col::Remote,
        Col::Proto,
        Col::Attempt,
        Col::Op,
        Col::Error,
        Col::RequestId,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Col::T => "t",
            Col::Api => "api",
            Col::Status => "status",
            Col::Total => "total",
            Col::Queue => "queue",
            Col::Connect => "connect",
            Col::Send => "send",
            Col::Ttfb => "ttfb",
            Col::Body => "body",
            Col::Down => "down",
            Col::Up => "up",
            Col::Conn => "conn",
            Col::Remote => "remote",
            Col::Proto => "proto",
            Col::Attempt => "attempt",
            Col::Op => "op",
            Col::Error => "error",
            Col::RequestId => "request_id",
        }
    }

    pub fn from_key(k: &str) -> Option<Col> {
        Col::ALL.into_iter().find(|c| c.key() == k)
    }

    pub fn title(self) -> &'static str {
        match self {
            Col::T => "Start",
            Col::Api => "API",
            Col::Status => "Status",
            Col::Total => "Total",
            Col::Queue => "Queue",
            Col::Connect => "Connect",
            Col::Send => "Send",
            Col::Ttfb => "TTFB",
            Col::Body => "Body",
            Col::Down => "Down",
            Col::Up => "Up",
            Col::Conn => "Conn",
            Col::Remote => "Remote",
            Col::Proto => "Proto",
            Col::Attempt => "Try",
            Col::Op => "Op",
            Col::Error => "Error",
            Col::RequestId => "Request id",
        }
    }

    /// Width in points.
    pub fn width(self) -> f32 {
        match self {
            Col::T => 84.0,
            Col::Api => 150.0,
            Col::Status => 64.0,
            Col::Remote => 150.0,
            Col::Error => 180.0,
            Col::RequestId => 200.0,
            Col::Proto | Col::Attempt => 56.0,
            Col::Conn | Col::Op => 64.0,
            _ => 80.0,
        }
    }

    /// Numbers align at the end.
    pub fn numeric(self) -> bool {
        !matches!(
            self,
            Col::Api | Col::Remote | Col::Proto | Col::Error | Col::RequestId
        )
    }

    /// The cell text.
    pub fn cell(self, r: &ReqRow) -> String {
        match self {
            Col::T => format!("{:.3} s", r.t_us as f64 / 1e6),
            Col::Api => r.api.to_string(),
            Col::Status => {
                if r.status == 0 {
                    "—".into()
                } else {
                    r.status.to_string()
                }
            }
            Col::Total => fmt::dur_us(r.total_us as f64),
            Col::Queue => stage(r.queue_us),
            Col::Connect => stage(r.connect_us()),
            Col::Send => stage(r.send_us),
            Col::Ttfb => stage(r.ttfb_us),
            Col::Body => stage(r.body_us),
            Col::Down => bytes(r.bytes_down),
            Col::Up => bytes(r.bytes_up),
            Col::Conn => format!("#{}", r.conn_id),
            Col::Remote => r.remote.map(|a| a.to_string()).unwrap_or_default(),
            Col::Proto => proto(r.protocol).into(),
            Col::Attempt => r.attempt.to_string(),
            Col::Op => r.op_id.to_string(),
            Col::Error => r
                .error_code
                .map(|c| c.as_str().to_owned())
                .or_else(|| r.error.clone())
                .unwrap_or_default(),
            Col::RequestId => r.request_id.clone().unwrap_or_default(),
        }
    }

    fn cmp(self, a: &ReqRow, b: &ReqRow) -> Ordering {
        match self {
            Col::T => a.t_us.cmp(&b.t_us),
            Col::Api => a.api.cmp(&b.api),
            Col::Status => a.status.cmp(&b.status),
            Col::Total => a.total_us.cmp(&b.total_us),
            Col::Queue => a.queue_us.cmp(&b.queue_us),
            Col::Connect => a.connect_us().cmp(&b.connect_us()),
            Col::Send => a.send_us.cmp(&b.send_us),
            Col::Ttfb => a.ttfb_us.cmp(&b.ttfb_us),
            Col::Body => a.body_us.cmp(&b.body_us),
            Col::Down => a.bytes_down.cmp(&b.bytes_down),
            Col::Up => a.bytes_up.cmp(&b.bytes_up),
            Col::Conn => a.conn_id.cmp(&b.conn_id),
            Col::Remote => a.remote.cmp(&b.remote),
            Col::Proto => proto(a.protocol).cmp(proto(b.protocol)),
            Col::Attempt => a.attempt.cmp(&b.attempt),
            Col::Op => a.op_id.cmp(&b.op_id),
            Col::Error => self.cell(a).cmp(&self.cell(b)),
            Col::RequestId => a.request_id.cmp(&b.request_id),
        }
    }
}

fn stage(us: u64) -> String {
    if us == 0 {
        String::new()
    } else {
        fmt::dur_us(us as f64)
    }
}

fn bytes(b: u64) -> String {
    if b == 0 {
        String::new()
    } else {
        fmt::bytes(b as f64)
    }
}

pub fn proto(p: WireProtocol) -> &'static str {
    match p {
        WireProtocol::H1 => "h1",
        WireProtocol::H2 => "h2",
        WireProtocol::Inproc => "local",
    }
}

/// What the filter box asks for. Tokens: `api:Get`, `status:503`, `status:5xx`, `remote:10.0.`,
/// `conn:12`, `op:42`, `error` / `error:code`, `retry`, `cold`, `min:100ms` (or `>100ms`),
/// `t:2..5` (seconds); other words match the API, remote, error or request id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    pub api: Option<String>,
    pub status: Option<String>,
    pub remote: Option<String>,
    pub conn: Option<u64>,
    pub op: Option<u64>,
    pub error: Option<String>,
    pub errors_only: bool,
    pub retries_only: bool,
    pub cold_only: bool,
    pub min_total_us: Option<u64>,
    pub window_us: Option<(i64, i64)>,
    pub words: Vec<String>,
    /// Tokens that did not parse (shown to the user).
    pub invalid: Vec<String>,
}

/// `100ms`, `1.5s`, `250us`, `250µs`, bare numbers = ms.
pub fn parse_latency_us(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = if let Some(v) = s.strip_suffix("ms") {
        (v, 1e3)
    } else if let Some(v) = s.strip_suffix("us").or_else(|| s.strip_suffix("µs")) {
        (v, 1.0)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1e6)
    } else {
        (s, 1e3)
    };
    let v: f64 = num.trim().parse().ok()?;
    (v >= 0.0).then(|| (v * mul) as u64)
}

impl Filter {
    pub fn parse(q: &str) -> Self {
        let mut f = Filter::default();
        for tok in q.split_whitespace() {
            let lower = tok.to_ascii_lowercase();
            if let Some(v) = tok.strip_prefix('>') {
                match parse_latency_us(v) {
                    Some(us) => f.min_total_us = Some(us),
                    None => f.invalid.push(tok.into()),
                }
                continue;
            }
            match lower.as_str() {
                "error" | "errors" | "failed" => {
                    f.errors_only = true;
                    continue;
                }
                "retry" | "retries" => {
                    f.retries_only = true;
                    continue;
                }
                "cold" => {
                    f.cold_only = true;
                    continue;
                }
                _ => {}
            }
            let Some((k, v)) = tok.split_once(':') else {
                f.words.push(lower);
                continue;
            };
            let ok = match k.to_ascii_lowercase().as_str() {
                "api" => {
                    f.api = Some(v.to_ascii_lowercase());
                    true
                }
                "status" => {
                    f.status = Some(v.to_ascii_lowercase());
                    true
                }
                "remote" | "ip" => {
                    f.remote = Some(v.into());
                    true
                }
                "conn" => v
                    .trim_start_matches('#')
                    .parse()
                    .map(|c| f.conn = Some(c))
                    .is_ok(),
                "op" => v.parse().map(|o| f.op = Some(o)).is_ok(),
                "error" => {
                    f.error = Some(v.to_ascii_lowercase());
                    true
                }
                "min" | "total" => {
                    parse_latency_us(v.trim_start_matches(">=").trim_start_matches('>'))
                        .map(|us| f.min_total_us = Some(us))
                        .is_some()
                }
                "t" => parse_window(v).map(|w| f.window_us = Some(w)).is_some(),
                _ => false,
            };
            if !ok {
                f.invalid.push(tok.into());
            }
        }
        f
    }

    pub fn is_empty(&self) -> bool {
        *self
            == Filter {
                invalid: self.invalid.clone(),
                ..Default::default()
            }
    }

    pub fn matches(&self, r: &ReqRow) -> bool {
        if let Some(a) = &self.api
            && !r.api.to_ascii_lowercase().contains(a.as_str())
        {
            return false;
        }
        if let Some(s) = &self.status
            && !status_matches(s, r.status)
        {
            return false;
        }
        if let Some(rem) = &self.remote
            && !r
                .remote
                .is_some_and(|a| a.to_string().contains(rem.as_str()))
        {
            return false;
        }
        if self.conn.is_some_and(|c| c != r.conn_id) || self.op.is_some_and(|o| o != r.op_id) {
            return false;
        }
        if self.errors_only && !r.is_error() {
            return false;
        }
        if self.retries_only && r.attempt <= 1 {
            return false;
        }
        if self.cold_only && r.reused {
            return false;
        }
        if let Some(e) = &self.error {
            let code = r.error_code.map(|c| c.as_str()).unwrap_or("");
            let detail = r.error.as_deref().unwrap_or("").to_ascii_lowercase();
            if !code.contains(e.as_str()) && !detail.contains(e.as_str()) {
                return false;
            }
        }
        if self.min_total_us.is_some_and(|m| r.total_us < m) {
            return false;
        }
        if let Some((a, b)) = self.window_us
            && (r.t_us < a || r.t_us >= b)
        {
            return false;
        }
        self.words.iter().all(|w| {
            r.api.to_ascii_lowercase().contains(w.as_str())
                || r.remote.is_some_and(|a| a.to_string().contains(w.as_str()))
                || r.error_code
                    .is_some_and(|c| c.as_str().contains(w.as_str()))
                || r.error
                    .as_deref()
                    .is_some_and(|e| e.to_ascii_lowercase().contains(w.as_str()))
                || r.request_id
                    .as_deref()
                    .is_some_and(|id| id.to_ascii_lowercase().contains(w.as_str()))
                || r.status.to_string() == *w
        })
    }

    /// Indices of the matching rows, in run order.
    pub fn apply(&self, reqs: &[ReqRow]) -> Vec<u32> {
        reqs.iter()
            .enumerate()
            .filter(|(_, r)| self.matches(r))
            .map(|(i, _)| i as u32)
            .collect()
    }
}

/// `2..5` or `2.5..` (seconds) → µs.
fn parse_window(v: &str) -> Option<(i64, i64)> {
    let (a, b) = v.split_once("..")?;
    let a: f64 = if a.is_empty() {
        0.0
    } else {
        a.trim_end_matches('s').parse().ok()?
    };
    let b: f64 = if b.is_empty() {
        f64::MAX / 1e7
    } else {
        b.trim_end_matches('s').parse().ok()?
    };
    Some(((a * 1e6) as i64, (b * 1e6) as i64))
}

fn status_matches(pat: &str, status: u16) -> bool {
    if let Some(class) = pat.strip_suffix("xx") {
        return class.parse::<u16>().is_ok_and(|c| status / 100 == c);
    }
    if pat == "0" || pat == "none" {
        return status == 0;
    }
    pat.parse::<u16>().is_ok_and(|s| s == status)
}

/// Sort `idx` by `col`, stable, descending when asked.
pub fn sort(idx: &mut [u32], reqs: &[ReqRow], col: Col, desc: bool) {
    idx.sort_by(|a, b| {
        let o = col.cmp(&reqs[*a as usize], &reqs[*b as usize]);
        if desc { o.reverse() } else { o }
    });
}

/// The rows `idx` as CSV (machine units, every column).
pub fn to_csv(reqs: &[ReqRow], idx: &[u32]) -> String {
    let mut out = String::from(
        "t_us,op_id,attempt,api,method,status,protocol,reused,conn_id,remote,queue_us,dns_us,tcp_us,proxy_us,tls_us,send_us,ttfb_us,body_us,stall_us,total_us,bytes_up,bytes_down,request_id,error_code,error\n",
    );
    let q = |s: &str| {
        if s.contains([',', '"', '\n']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_owned()
        }
    };
    for &i in idx {
        let r = &reqs[i as usize];
        out += &format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.t_us,
            r.op_id,
            r.attempt,
            q(&r.api),
            r.method,
            r.status,
            proto(r.protocol),
            r.reused,
            r.conn_id,
            r.remote.map(|a| a.to_string()).unwrap_or_default(),
            r.queue_us,
            r.dns_us,
            r.tcp_us,
            r.proxy_us,
            r.tls_us,
            r.send_us,
            r.ttfb_us,
            r.body_us,
            r.stall_us,
            r.total_us,
            r.bytes_up,
            r.bytes_down,
            q(r.request_id.as_deref().unwrap_or("")),
            r.error_code.map(|c| c.as_str()).unwrap_or(""),
            q(r.error.as_deref().unwrap_or("")),
        );
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn req(t_ms: i64, api: &'static str, status: u16, total_ms: u64) -> ReqRow {
        ReqRow {
            t_us: t_ms * 1000,
            op_id: t_ms as u64,
            attempt: 1,
            api: api.into(),
            method: "GET".into(),
            status,
            protocol: WireProtocol::H1,
            reused: true,
            conn_id: 1,
            remote: Some("10.0.0.1:443".parse().unwrap()),
            queue_us: 0,
            dns_us: 0,
            tcp_us: 0,
            proxy_us: 0,
            tls_us: 0,
            send_us: 10,
            ttfb_us: total_ms * 800,
            body_us: total_ms * 200 - 10,
            stall_us: 0,
            total_us: total_ms * 1000,
            bytes_up: 0,
            bytes_down: 1024,
            request_id: Some(format!("req-{t_ms}")),
            retry_after_ms: None,
            error_code: None,
            error: None,
            tls: None,
            tcp: None,
        }
    }

    #[test]
    fn filter_tokens() {
        let rows = vec![
            req(1, "GetObject", 200, 5),
            req(2, "GetObject", 503, 50),
            req(3, "PutObject", 200, 500),
        ];
        assert_eq!(Filter::parse("api:get").apply(&rows), vec![0, 1]);
        assert_eq!(Filter::parse("status:5xx").apply(&rows), vec![1]);
        assert_eq!(Filter::parse("error").apply(&rows), vec![1]);
        assert_eq!(Filter::parse(">40ms").apply(&rows), vec![1, 2]);
        assert_eq!(Filter::parse("min:1s").apply(&rows), Vec::<u32>::new());
        assert_eq!(Filter::parse("put").apply(&rows), vec![2]);
        assert_eq!(Filter::parse("remote:10.0.0.1").apply(&rows).len(), 3);
        assert_eq!(Filter::parse("t:0.0015..0.0025").apply(&rows), vec![1]);
        assert_eq!(Filter::parse("conn:x").invalid, vec!["conn:x".to_owned()]);
        assert!(Filter::parse("  ").is_empty());
    }

    #[test]
    fn sorting_is_by_value_not_text() {
        let rows = vec![
            req(1, "A", 200, 9),
            req(2, "B", 200, 10),
            req(3, "C", 200, 100),
        ];
        let mut idx = vec![0, 1, 2];
        sort(&mut idx, &rows, Col::Total, true);
        assert_eq!(idx, vec![2, 1, 0]);
    }

    #[test]
    fn csv_has_every_row() {
        let rows = vec![req(1, "GetObject", 200, 5)];
        let csv = to_csv(&rows, &[0]);
        assert_eq!(csv.lines().count(), 2);
        assert!(csv.contains("GetObject"));
    }
}
