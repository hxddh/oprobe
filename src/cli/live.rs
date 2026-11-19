//! Single-line live progress on stderr for `oprobe run`.

use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::engine::Progress;
use crate::report::style::{dim, red, yellow};
use crate::trace::Phase;
use crate::util::fmt_ms;

pub struct LiveLine {
    total: Duration,
    started: Instant,
    state: Mutex<(usize, bool)>,
}

const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

impl LiveLine {
    pub fn new(total: Duration) -> Self {
        Self {
            total,
            started: Instant::now(),
            state: Mutex::new((0, false)),
        }
    }

    fn width() -> usize {
        // Shells export COLUMNS; without it stay within a classic 80-column terminal so the
        // single status line never wraps.
        std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.trim().parse::<usize>().ok())
            .unwrap_or(80)
            .max(40)
    }

    pub fn update(&self, p: Progress) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut err = std::io::stderr().lock();
        match p {
            Progress::Message(m) => {
                let _ = write!(err, "\r\x1b[2K{} {m}\n", dim("·"));
            }
            Progress::Snapshot(s) => {
                if st.1 {
                    return;
                }
                st.0 += 1;
                let spin = SPIN[st.0 % SPIN.len()];
                let line = match s.phase {
                    Phase::Warmup => format!(
                        "{spin} warming up… {:.1}s",
                        self.started.elapsed().as_secs_f64()
                    ),
                    Phase::Measure | Phase::Done | Phase::Verify => {
                        let frac =
                            (s.elapsed_s / self.total.as_secs_f64().max(1e-3)).clamp(0.0, 1.0);
                        let bar = crate::report::style::bar(frac, 12);
                        let sm = &s.summary;
                        let mut l = format!(
                            "{spin} {} {:>5.1}/{:.0}s  {:>8.1} ops/s  {:>7.1} MiB/s  p50 {:>7}  p99 {:>7}  in-flight {:>3}",
                            dim(&bar),
                            s.elapsed_s,
                            self.total.as_secs_f64(),
                            sm.ops_per_s,
                            sm.mib_per_s,
                            fmt_ms(sm.latency.p50),
                            fmt_ms(sm.latency.p99),
                            sm.inflight_ops
                        );
                        if sm.failed_ops > 0 {
                            l.push_str(&format!("  {}", red(&format!("errors {}", sm.failed_ops))));
                        }
                        if s.resilience.throttled_responses > 0 {
                            l.push_str(&format!(
                                "  {}",
                                yellow(&format!("429/503 {}", s.resilience.throttled_responses))
                            ));
                        }
                        l
                    }
                    other => format!("{spin} {}", other.label()),
                };
                let w = Self::width();
                let visible = crate::report::style::visible_width(&line);
                let line = if visible > w {
                    line.chars().take(w.saturating_sub(1)).collect::<String>() + "\x1b[0m"
                } else {
                    line
                };
                let _ = write!(err, "\r\x1b[2K{line}");
            }
        }
        let _ = err.flush();
    }

    pub fn finish(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.1 = true;
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K");
        let _ = err.flush();
    }
}
