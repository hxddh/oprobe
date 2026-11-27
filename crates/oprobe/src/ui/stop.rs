//! The signals that stop the app: SIGTERM (`kill`, a supervisor, a logout), SIGHUP (the
//! terminal or SSH session closed) and SIGINT (Ctrl-C in the shell that started a window; in
//! the terminal app raw mode makes Ctrl+C a key, and SIGINT only comes from `kill -INT`).
//!
//! Their default action kills the process where it stands: a run or a prove in progress left
//! its test data and its multipart uploads on the target, and the terminal app left the
//! terminal in raw mode. 9.3.2 handled them in the commands only. Now the app stops as it
//! does on Quit: the first signal stops every job, which removes its test data
//! (`STOP_GRACE` at most), and the second leaves at once. The exit code is 130, as for a
//! command.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Stop signals received so far.
static SEEN: AtomicUsize = AtomicUsize::new(0);

/// How many stop signals the app has received.
pub fn seen() -> usize {
    SEEN.load(Ordering::SeqCst)
}

/// Handle the stop signals from now on, on a thread of their own: each is counted, then
/// `on_signal` runs there with the count. A signal that cannot be handled keeps its default.
pub fn watch(on_signal: impl Fn(usize) + Send + 'static) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        // Installed before this returns: a signal from now on is handled.
        let streams = rt.block_on(async {
            [
                SignalKind::terminate(),
                SignalKind::hangup(),
                SignalKind::interrupt(),
            ]
            .map(|k| signal(k).ok())
        });
        let spawned = std::thread::Builder::new()
            .name("oprobe-signals".into())
            .spawn(move || {
                rt.block_on(async move {
                    let [mut term, mut hup, mut int] = streams;
                    async fn recv(s: &mut Option<tokio::signal::unix::Signal>) -> Option<()> {
                        match s {
                            Some(s) => s.recv().await,
                            None => std::future::pending().await,
                        }
                    }
                    loop {
                        let got = tokio::select! {
                            r = recv(&mut term) => r,
                            r = recv(&mut hup) => r,
                            r = recv(&mut int) => r,
                        };
                        if got.is_none() {
                            return;
                        }
                        let n = SEEN.fetch_add(1, Ordering::SeqCst) + 1;
                        on_signal(n);
                    }
                })
            });
        drop(spawned);
    }
    #[cfg(not(unix))]
    drop(on_signal);
}

/// The exit code of an app a signal stopped.
pub const EXIT_SIGNALLED: u8 = 130;
