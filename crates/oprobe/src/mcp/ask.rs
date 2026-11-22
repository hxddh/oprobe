//! `ask_run`: a question in natural language over a stored run, answered by deterministic
//! retrieval over its report (design v5 §6). There is no language model inside: the question's
//! words select topics, each topic reads fixed report fields, and every answer cites the JSON
//! pointers of the fields it quotes, so an agent can check it against `get_run` (detail=full)
//! or the `oprobe://runs/{id}/report` resource.

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

/// One report field quoted by an answer.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Evidence {
    /// JSON pointer into the report, e.g. `/summary/p99_ms`.
    pub field: String,
    pub value: Value,
}

/// The answer of one topic.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Answer {
    pub topic: String,
    /// The answer in one sentence, made only of the evidence below.
    pub statement: String,
    pub evidence: Vec<Evidence>,
}

/// Result of `ask_run`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AskOut {
    pub run_id: String,
    pub question: String,
    /// Topics the question matched (the verdict when none did).
    pub topics: Vec<String>,
    pub answers: Vec<Answer>,
    /// Topics a question can ask about.
    pub available_topics: Vec<String>,
    pub next: Vec<crate::output::NextStep>,
}

struct Topic {
    name: &'static str,
    words: &'static [&'static str],
    /// Report subtrees read, with the leaf-name fragments kept (empty: every leaf).
    fields: &'static [(&'static str, &'static [&'static str])],
}

const TOPICS: &[Topic] = &[
    Topic {
        name: "verdict",
        words: &[
            "verdict", "sla", "pass", "passed", "fail", "failed", "ok", "healthy", "summary",
            "result", "overall",
        ],
        fields: &[(
            "/verdict",
            &["level", "headline", "exit_code", "bottleneck", "reason"],
        )],
    },
    Topic {
        name: "latency",
        words: &[
            "latency",
            "latencies",
            "slow",
            "slower",
            "slowest",
            "p50",
            "p90",
            "p99",
            "p999",
            "tail",
            "fast",
            "ttfb",
            "time",
            "ms",
            "wait",
        ],
        fields: &[
            (
                "/summary",
                &["p50", "p90", "p99", "p999", "max", "mean", "ttfb"],
            ),
            ("/stages", &["p50", "p99", "share", "ratio"]),
        ],
    },
    Topic {
        name: "throughput",
        words: &[
            "throughput",
            "ops",
            "rate",
            "bandwidth",
            "mib",
            "mb",
            "gbps",
            "speed",
            "fast",
            "faster",
            "requests",
            "qps",
            "goodput",
        ],
        fields: &[(
            "/summary",
            &["per_s", "ops_count", "requests_count", "bytes", "duration"],
        )],
    },
    Topic {
        name: "errors",
        words: &[
            "error",
            "errors",
            "fail",
            "failure",
            "failures",
            "5xx",
            "500",
            "503",
            "throttle",
            "throttled",
            "throttling",
            "slowdown",
            "retry",
            "retries",
            "timeout",
            "timeouts",
            "reset",
        ],
        fields: &[
            ("/summary", &["error", "throttl", "retr", "fail"]),
            ("/resilience", &[]),
        ],
    },
    Topic {
        name: "bottleneck",
        words: &[
            "why",
            "cause",
            "causes",
            "bottleneck",
            "limit",
            "limited",
            "ceiling",
            "capacity",
            "saturated",
            "saturation",
            "bound",
            "problem",
            "wrong",
        ],
        fields: &[
            ("/verdict", &["bottleneck", "headline"]),
            (
                "/capacity",
                &["ceiling", "limit", "inflight", "kind", "confidence"],
            ),
        ],
    },
    Topic {
        name: "findings",
        words: &[
            "finding",
            "findings",
            "advice",
            "fix",
            "change",
            "recommend",
            "recommendation",
            "should",
            "improve",
            "tune",
            "tuning",
            "suggest",
            "what",
        ],
        fields: &[(
            "/tune/recommendation",
            &["overrides", "summary", "delta", "metric"],
        )],
    },
    Topic {
        name: "cost",
        words: &[
            "cost",
            "costs",
            "price",
            "prices",
            "usd",
            "dollar",
            "dollars",
            "money",
            "expensive",
            "cheap",
            "bill",
            "$",
        ],
        fields: &[("/cost", &["usd", "provider", "per_month", "requests"])],
    },
    Topic {
        name: "connections",
        words: &[
            "connection",
            "connections",
            "tls",
            "dns",
            "tcp",
            "handshake",
            "reuse",
            "pool",
            "keepalive",
            "http2",
            "h2",
            "network",
            "dial",
        ],
        fields: &[
            ("/connections", &[]),
            ("/network", &["dns", "tcp", "tls", "rtt", "http"]),
        ],
    },
    Topic {
        name: "configuration",
        words: &[
            "config",
            "configuration",
            "concurrency",
            "knob",
            "knobs",
            "setting",
            "settings",
            "chunk",
            "parallelism",
            "client",
        ],
        fields: &[("/knobs", &[]), ("/client", &["kind", "name", "version"])],
    },
    Topic {
        name: "workload",
        words: &[
            "workload", "preset", "objects", "object", "size", "sizes", "dataset", "duration",
            "mix", "read", "write", "reads", "writes",
        ],
        fields: &[("/workload", &[])],
    },
];

/// Leaf values under `v` (depth ≤ 3, at most `max`), whose name contains one of `frags`.
fn leaves(v: &Value, ptr: &str, frags: &[&str], out: &mut Vec<Evidence>, depth: usize, max: usize) {
    if out.len() >= max || depth > 3 {
        return;
    }
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let p = format!("{ptr}/{}", k.replace('~', "~0").replace('/', "~1"));
                if x.is_object() || x.is_array() {
                    leaves(x, &p, frags, out, depth + 1, max);
                } else if !x.is_null() && (frags.is_empty() || frags.iter().any(|f| k.contains(f)))
                {
                    out.push(Evidence {
                        field: p,
                        value: x.clone(),
                    });
                }
                if out.len() >= max {
                    return;
                }
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().take(5).enumerate() {
                leaves(x, &format!("{ptr}/{i}"), frags, out, depth + 1, max);
            }
        }
        _ => {}
    }
}

fn words(q: &str) -> Vec<String> {
    q.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '$'))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

fn fmt_value(v: &Value) -> String {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() != 0.0 => format!("{f:.3}")
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned(),
            _ => n.to_string(),
        },
        Value::String(s) => super::security::untrusted(s, 160),
        other => other.to_string(),
    }
}

fn statement(topic: &str, ev: &[Evidence]) -> String {
    if ev.is_empty() {
        return format!("the report holds no {topic} fields for this run");
    }
    let parts: Vec<String> = ev
        .iter()
        .take(6)
        .map(|e| {
            let name = e.field.trim_start_matches('/').replace('/', ".");
            format!("{name} = {}", fmt_value(&e.value))
        })
        .collect();
    format!("{topic}: {}", parts.join("; "))
}

/// Findings, most severe first: code, severity and the cause/advice sentences.
fn findings(report: &Value, out: &mut Vec<Evidence>) {
    let Some(list) = report.get("findings").and_then(Value::as_array) else {
        return;
    };
    for (i, f) in list.iter().take(3).enumerate() {
        for k in ["code", "severity", "title", "cause", "advice"] {
            if let Some(v) = f.get(k).filter(|v| !v.is_null()) {
                out.push(Evidence {
                    field: format!("/findings/{i}/{k}"),
                    value: v.clone(),
                });
            }
        }
    }
}

/// Answer `question` over `report` (a serialized Report v5).
pub fn ask(run_id: &str, question: &str, report: &Value) -> AskOut {
    let w = words(question);
    let mut scored: Vec<(usize, &Topic)> = TOPICS
        .iter()
        .map(|t| {
            (
                w.iter().filter(|x| t.words.contains(&x.as_str())).count(),
                t,
            )
        })
        .filter(|(n, _)| *n > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let mut chosen: Vec<&Topic> = scored.iter().take(3).map(|(_, t)| *t).collect();
    if chosen.is_empty() {
        chosen.push(&TOPICS[0]);
    }
    let answers = chosen
        .iter()
        .map(|t| {
            let mut ev = Vec::new();
            if t.name == "findings" {
                findings(report, &mut ev);
            }
            for (ptr, frags) in t.fields {
                if let Some(sub) = report.pointer(ptr) {
                    if sub.is_object() || sub.is_array() {
                        leaves(sub, ptr, frags, &mut ev, 0, 12);
                    } else if !sub.is_null() {
                        ev.push(Evidence {
                            field: (*ptr).to_owned(),
                            value: sub.clone(),
                        });
                    }
                }
            }
            if t.name == "findings" && ev.is_empty() {
                return Answer {
                    topic: t.name.into(),
                    statement: "findings: none (the run raised no finding)".into(),
                    evidence: vec![Evidence {
                        field: "/findings".into(),
                        value: Value::Array(vec![]),
                    }],
                };
            }
            Answer {
                topic: t.name.into(),
                statement: statement(t.name, &ev),
                evidence: ev,
            }
        })
        .collect();
    use crate::output::NextStep;
    AskOut {
        run_id: run_id.to_owned(),
        question: super::security::untrusted(question, 500),
        topics: chosen.iter().map(|t| t.name.to_owned()).collect(),
        answers,
        available_topics: TOPICS.iter().map(|t| t.name.to_owned()).collect(),
        next: vec![
            NextStep::new("explain the run", format!("oprobe explain {run_id}"))
                .with_mcp("explain_run", serde_json::json!({ "run": run_id })),
            NextStep::new(
                "read the whole report",
                format!("oprobe runs show {run_id}"),
            )
            .with_mcp(
                "get_run",
                serde_json::json!({ "run": run_id, "detail": "full" }),
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_cite_report_fields() {
        let report = serde_json::json!({
            "verdict": {"level": "warn", "headline": "throttled above 8 in flight"},
            "summary": {"ops_per_s": 315.25, "p50_ms": 4.5, "p99_ms": 41.0, "error_ratio": 0.02},
            "findings": [{"code": "throttling", "severity": "warn", "cause": "503 SlowDown", "advice": "lower concurrency"}],
            "cost": {"estimated_usd": 0.01}
        });
        let a = ask("r1", "Why is my p99 so slow?", &report);
        assert!(a.topics.contains(&"latency".to_owned()), "{:?}", a.topics);
        assert!(
            a.topics.contains(&"bottleneck".to_owned()),
            "{:?}",
            a.topics
        );
        let lat = a.answers.iter().find(|x| x.topic == "latency").unwrap();
        assert!(
            lat.evidence
                .iter()
                .any(|e| e.field == "/summary/p99_ms" && e.value == 41.0)
        );
        assert!(
            lat.statement.contains("summary.p99_ms = 41"),
            "{}",
            lat.statement
        );
        let f = ask("r1", "what should I change?", &report);
        let ans = f.answers.iter().find(|x| x.topic == "findings").unwrap();
        assert!(ans.evidence.iter().any(|e| e.field == "/findings/0/code"));
        // Deterministic, and the verdict when nothing matches.
        assert_eq!(
            serde_json::to_value(ask("r1", "hmm", &report)).unwrap(),
            serde_json::to_value(ask("r1", "hmm", &report)).unwrap()
        );
        assert_eq!(ask("r1", "hmm", &report).topics, ["verdict"]);
    }
}
