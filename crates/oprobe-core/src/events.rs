//! Events (`oprobe.events/1`, design §7): what a running command reports while it works.
//!
//! One [`Event`] enum feeds every in-process consumer (the app, Prove's live pairs). Producers emit
//! into an [`EventBus`]; [`EventBus::progress_fn`] adapts the engine's progress callback
//! ([`crate::engine::ProgressFn`]) into `phase` / `progress` / `interval` / `log` events.

use std::sync::{Arc, Mutex};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::engine::{Progress, ProgressFn};
use crate::errors::ErrorBody;
use crate::model::{Finding, Report, TrialRow};
use crate::trace::Phase;

/// Schema id of the event stream.
pub const EVENTS_SCHEMA: &str = "oprobe.events/1";

/// Log level of a `log` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// The payload of an event (`type` + fields).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// First line of every stream.
    Version {
        schema: String,
        oprobe: String,
    },
    Phase {
        phase: Phase,
    },
    Progress {
        elapsed_s: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_s: Option<f64>,
        #[serde(rename = "ops_count", alias = "ops")]
        ops: u64,
        #[serde(rename = "requests_count", alias = "requests")]
        requests: u64,
    },
    /// One second of the measurement window.
    Interval {
        /// Start of the second, seconds from the window start.
        t_s: f64,
        ops_per_s: f64,
        mib_per_s: f64,
        p50_ms: f64,
        p99_ms: f64,
        #[serde(rename = "errors_count", alias = "errors")]
        errors: u64,
        #[serde(rename = "throttled_count", alias = "throttled")]
        throttled: u64,
    },
    Finding {
        #[serde(flatten)]
        finding: Finding,
    },
    Trial {
        #[serde(flatten)]
        trial: TrialRow,
    },
    Log {
        level: LogLevel,
        message: String,
    },
    /// The command's result (last line of a successful stream).
    Result {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        /// Run file, when the command saved one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report: Option<Box<Report>>,
        /// The typed result of commands that do not produce a run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    Error {
        error: ErrorBody,
    },
}

/// One event: a timestamp (RFC 3339) and its payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    pub ts: String,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    pub fn now(kind: EventKind) -> Self {
        Self {
            ts: now_rfc3339_ms(),
            kind,
        }
    }

    /// The `version` event that starts every stream.
    pub fn version() -> Self {
        Self::now(EventKind::Version {
            schema: EVENTS_SCHEMA.to_owned(),
            oprobe: env!("CARGO_PKG_VERSION").to_owned(),
        })
    }

    pub fn log(level: LogLevel, message: impl Into<String>) -> Self {
        Self::now(EventKind::Log {
            level,
            message: message.into(),
        })
    }
}

/// RFC 3339 UTC with milliseconds.
fn now_rfc3339_ms() -> String {
    let now = std::time::SystemTime::now();
    let base = crate::util::fmt_rfc3339(now);
    let ms = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis())
        .unwrap_or(0);
    format!("{}.{ms:03}Z", base.trim_end_matches('Z'))
}

/// A subscriber.
pub type EventFn = Arc<dyn Fn(&Event) + Send + Sync>;

/// What an in-process front end sees of a measurement as it happens, beyond the serialized
/// events: the engine's full snapshots (the App's live view draws them), the cleanup stage, and
/// the finished run (a budget stop included, which the command returns as an error). Never
/// serialized; CLI and MCP output are unaffected.
#[derive(Clone, Debug)]
pub enum Live {
    /// The engine's progress: messages and full [`crate::trace::Snapshot`]s.
    Progress(Progress),
    /// The measurement window has ended; test data is being removed.
    CleaningUp,
    /// The run was built (and saved when `path` is set), before any budget error is returned.
    Finished {
        run: Box<crate::model::Run>,
        path: Option<std::path::PathBuf>,
    },
}

/// A [`Live`] subscriber.
pub type LiveFn = Arc<dyn Fn(&Live) + Send + Sync>;

/// In-process broadcast: every subscriber sees every event, in emission order.
#[derive(Clone, Default)]
pub struct EventBus {
    subs: Arc<Mutex<Vec<EventFn>>>,
    live: Arc<Mutex<Vec<LiveFn>>>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus").finish_non_exhaustive()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self, f: EventFn) {
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).push(f);
    }

    /// Subscribe a channel; events are cloned into it.
    pub fn channel(&self) -> std::sync::mpsc::Receiver<Event> {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        self.subscribe(Arc::new(move |e| {
            let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).send(e.clone());
        }));
        rx
    }

    /// Subscribe to the in-process [`Live`] stream (snapshots, cleanup, the finished run).
    pub fn subscribe_live(&self, f: LiveFn) {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).push(f);
    }

    /// Whether anyone listens to the [`Live`] stream (producers skip cloning snapshots if not).
    pub fn has_live(&self) -> bool {
        !self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Send `e` to the [`Live`] subscribers.
    pub fn emit_live(&self, e: &Live) {
        let subs = self.live.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for s in subs {
            s(e);
        }
    }

    pub fn emit_event(&self, e: &Event) {
        // Clone the list so a subscriber may emit (or subscribe) without deadlocking.
        let subs = self.subs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for s in subs {
            s(e);
        }
    }

    pub fn emit(&self, kind: EventKind) {
        self.emit_event(&Event::now(kind));
    }

    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        self.emit_event(&Event::log(level, message));
    }

    /// An engine progress callback that turns progress into events: messages become `log`,
    /// snapshots become `phase` (on change), `progress` and one `interval` per finished
    /// second. `total_s` is the planned window, when known.
    pub fn progress_fn(&self, total_s: Option<f64>) -> ProgressFn {
        let bus = self.clone();
        let state = Mutex::new((None::<Phase>, 0usize));
        Arc::new(move |p| match p {
            Progress::Message(m) => bus.log(LogLevel::Info, m),
            Progress::Snapshot(s) => {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                if st.0 != Some(s.phase) {
                    st.0 = Some(s.phase);
                    if s.phase == Phase::Measure {
                        st.1 = 0;
                    }
                    bus.emit(EventKind::Phase { phase: s.phase });
                }
                // Every second but the last (still filling) is final.
                // The window is over (`cleanup` follows it): every second is final.
                let done = if matches!(s.phase, Phase::Done | Phase::Cleanup) {
                    s.timeline.len()
                } else {
                    s.timeline.len().saturating_sub(1)
                };
                for t in s.timeline.iter().take(done).skip(st.1) {
                    bus.emit(EventKind::Interval {
                        t_s: t.t_s,
                        ops_per_s: t.ops_per_s,
                        mib_per_s: t.mib_per_s,
                        p50_ms: t.p50_ms,
                        p99_ms: t.p99_ms,
                        errors: t.errors,
                        throttled: t.throttled,
                    });
                }
                st.1 = st.1.max(done);
                drop(st);
                bus.emit(EventKind::Progress {
                    elapsed_s: s.elapsed_s,
                    total_s,
                    ops: s.summary.ops,
                    requests: s.connections.requests,
                });
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{Snapshot, TimelinePoint};

    #[test]
    fn events_serialize_with_type_and_ts() {
        let bus = EventBus::new();
        let buf: Arc<Mutex<String>> = Default::default();
        let b2 = buf.clone();
        bus.subscribe(Arc::new(move |e| {
            let mut b = b2.lock().unwrap();
            b.push_str(&serde_json::to_string(e).unwrap());
            b.push('\n');
        }));
        bus.emit_event(&Event::version());
        let progress = bus.progress_fn(Some(10.0));
        progress(Progress::Message("preparing".into()));
        let mut s = Snapshot {
            phase: Phase::Measure,
            elapsed_s: 2.5,
            ..Default::default()
        };
        s.timeline = (1..=3)
            .map(|i| TimelinePoint {
                t_s: i as f64,
                ops_per_s: 10.0,
                ..Default::default()
            })
            .collect();
        progress(Progress::Snapshot(Box::new(s.clone())));
        // The same snapshot again emits no duplicate intervals.
        progress(Progress::Snapshot(Box::new(s)));
        bus.emit(EventKind::Error {
            error: ErrorBody::new(crate::errors::ErrorCode::NetTimeout, "slow"),
        });
        let text = buf.lock().unwrap().clone();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["type"], "version");
        assert_eq!(lines[0]["schema"], EVENTS_SCHEMA);
        for l in &lines {
            assert!(l["type"].is_string());
            assert!(l["ts"].as_str().unwrap().ends_with('Z'));
        }
        let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "version", "log", "phase", "interval", "interval", "progress", "progress", "error"
            ]
        );
        assert_eq!(lines[2]["phase"], "measure");
        assert_eq!(lines.last().unwrap()["error"]["code"], "net.timeout");
        // Round trip.
        for l in text.lines() {
            let e: Event = serde_json::from_str(l).unwrap();
            assert!(!e.ts.is_empty());
        }
    }
}
