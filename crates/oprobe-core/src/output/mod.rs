//! Output plumbing for every command: `-o/--output text|json`.
//!
//! A command returns a typed result implementing [`CommandOutput`] (`Serialize + JsonSchema`
//! plus a text renderer) and hands it to [`Output::finish`]:
//! - `text`: the renderer's output on stdout;
//! - `json`: exactly one JSON document on stdout.
//!
//! stdout carries data only; logs and progress go to stderr. Errors are printed by
//! [`Output::error`]: JSON on stdout in json mode, `error:` + `hint:` on stderr otherwise.
//!
//! Nothing here (or anywhere a run, a prove, cleanup or a signal handler can reach) prints
//! with `eprintln!`: that macro panics when stderr is gone (the terminal or SSH session closed,
//! `2>&1 | head`), and a panic skips cleanup. Every line for stderr goes through
//! [`stderr_line`] / [`stderr_text`], which never fail; the first write a closed stderr
//! refuses runs the hooks of [`on_stderr_closed`] (the CLI cancels its command there, as a
//! signal would), and later lines are dropped.
//! In-process consumers (the app) read the command's events from [`Output::bus`].
//!
//! Every JSON result is wrapped in the common envelope ([`Envelope`]): the result's own fields
//! plus `next` ([`NextStep`]s, each with its CLI command) and `warnings` (the result's own plus
//! every [`Output::warn`] of the command).

use std::io::Write;

use schemars::JsonSchema;
use serde::Serialize;

use crate::errors::ErrorBody;
use crate::events::{EventBus, LogLevel};
use crate::report::style::{bold, dim, red, yellow};

/// `-o/--output`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    /// Machine formats print JSON (also for errors).
    pub fn is_machine(self) -> bool {
        self == Self::Json
    }
}

/// A suggested next step: what it does and the CLI command.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize, JsonSchema)]
pub struct NextStep {
    /// What the step does, in a few words.
    pub title: String,
    /// The command line (`oprobe …`).
    pub cli: String,
}

impl NextStep {
    pub fn new(title: impl Into<String>, cli: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            cli: cli.into(),
        }
    }
}

/// The common envelope of every JSON command result: the result's fields, then `next` and
/// `warnings`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Envelope<T> {
    #[serde(flatten)]
    pub result: T,
    /// Suggested next steps.
    pub next: Vec<NextStep>,
    /// Warnings of the command (also printed on stderr in text mode).
    pub warnings: Vec<String>,
}

impl<'a, T: CommandOutput> Envelope<&'a T> {
    /// `v` with its own next steps and warnings, plus `extra_warnings`.
    pub fn of(v: &'a T, extra_warnings: Vec<String>) -> Self {
        let mut warnings = v.warnings();
        for w in extra_warnings {
            if !warnings.contains(&w) {
                warnings.push(w);
            }
        }
        Self {
            next: v.next(),
            warnings,
            result: v,
        }
    }
}

/// The JSON of `v` in its envelope (what `-o json` prints).
pub fn envelope_value<T: CommandOutput>(v: &T, extra_warnings: Vec<String>) -> serde_json::Value {
    if !v.enveloped() {
        return serde_json::to_value(v).unwrap_or_default();
    }
    serde_json::to_value(Envelope::of(v, extra_warnings)).unwrap_or_default()
}

/// The typed result of a command.
pub trait CommandOutput: Serialize + JsonSchema {
    /// Human text (may contain ANSI colors when enabled).
    fn text(&self) -> String;

    /// Exit code of the command for this result (0 unless a gate failed).
    fn exit_code(&self) -> u8 {
        0
    }

    /// Suggested next steps (`next` in the JSON envelope).
    fn next(&self) -> Vec<NextStep> {
        Vec::new()
    }

    /// Warnings that belong to the result (`warnings` in the JSON envelope; the command's
    /// `Output::warn` lines are added).
    fn warnings(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether the JSON form is wrapped in the [`Envelope`] (false only for documents printed
    /// verbatim, such as a JSON Schema).
    fn enveloped(&self) -> bool {
        true
    }
}

/// Plain text that is still a typed result (`{"text": …}` in JSON).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TextOutput {
    pub text: String,
}

impl CommandOutput for TextOutput {
    fn text(&self) -> String {
        self.text.clone()
    }
}

/// Per-command output context.
#[derive(Clone, Debug)]
pub struct Output {
    pub format: OutputFormat,
    pub quiet: bool,
    /// No prompts, no live terminal redraws (implied when stdin/stdout is not a TTY).
    pub non_interactive: bool,
    bus: EventBus,
    /// Every `warn` of the command (the JSON envelope's `warnings`).
    warned: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Output {
    /// Set up output for `format`.
    pub fn new(format: OutputFormat, quiet: bool, non_interactive: bool) -> Self {
        let bus = EventBus::new();
        if format != OutputFormat::Text {
            // Machine formats never carry ANSI colors.
            crate::report::style::set(false);
        }
        Self {
            format,
            quiet,
            non_interactive,
            bus,
            warned: Default::default(),
        }
    }

    /// An output that writes nothing to stdout (tests, in-process use).
    pub fn silent() -> Self {
        Self {
            format: OutputFormat::Json,
            quiet: true,
            non_interactive: true,
            bus: EventBus::new(),
            warned: Default::default(),
        }
    }

    /// The event bus of this command (in-process consumers subscribe to it).
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// A progress/log line on stderr (and a `log` event); nothing on stderr with `-q`. Like
    /// every line this writes, it never carries a URL's credentials (a message may quote a
    /// storage error, and storage errors quote URLs).
    pub fn info(&self, msg: impl Into<String>) {
        let msg = crate::util::strip_url_credentials(&msg.into());
        if !self.quiet {
            stderr_line(&msg);
        }
        self.bus.log(LogLevel::Info, msg);
    }

    /// A warning: stderr (even with `-q`) and a `log` event.
    pub fn warn(&self, msg: impl Into<String>) {
        let msg = crate::util::strip_url_credentials(&msg.into());
        if let Ok(mut w) = self.warned.lock() {
            w.push(crate::report::style::strip_ansi(&msg));
        }
        stderr_line(format_args!("{} {msg}", yellow("warning:")));
        self.bus.log(LogLevel::Warn, msg);
    }

    /// The warnings given so far.
    pub fn warnings(&self) -> Vec<String> {
        self.warned.lock().map(|w| w.clone()).unwrap_or_default()
    }

    /// Print a command's result and return its exit code.
    pub fn finish<T: CommandOutput>(&self, v: &T) -> anyhow::Result<u8> {
        // The progress before the result, as a terminal showing both expects.
        flush_stderr(std::time::Duration::from_secs(1));
        let mut out = std::io::stdout().lock();
        match self.format {
            OutputFormat::Text => {
                let t = v.text();
                out.write_all(t.as_bytes())?;
                if !t.is_empty() && !t.ends_with('\n') {
                    out.write_all(b"\n")?;
                }
            }
            OutputFormat::Json => {
                if v.enveloped() {
                    serde_json::to_writer_pretty(&mut out, &Envelope::of(v, self.warnings()))?;
                } else {
                    serde_json::to_writer_pretty(&mut out, v)?;
                }
                out.write_all(b"\n")?;
            }
        }
        let _ = std::io::stdout().flush();
        Ok(v.exit_code())
    }

    /// Print a classified error (JSON on stdout in json mode) and return its exit code.
    pub fn error(&self, e: &ErrorBody) -> u8 {
        match self.format {
            OutputFormat::Json => {
                let v = serde_json::json!({"type": "error", "error": e});
                flush_stderr(std::time::Duration::from_secs(1));
                let mut out = std::io::stdout().lock();
                // A closed stdout (`| head`) is not worth a second error.
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                );
                let _ = out.flush();
            }
            OutputFormat::Text => {
                stderr_line(format_args!("{} {}", red(&bold("error:")), e.message));
                if let Some(h) = &e.hint {
                    stderr_line(format_args!("{} {h}", yellow(&bold("hint:"))));
                }
                stderr_line(dim(&format!("({})", e.code)));
            }
        }
        e.exit_code()
    }
}

/// What became of the process's stderr: whether a write to it was refused, and who to tell.
#[derive(Default)]
struct ErrStream {
    closed: std::sync::atomic::AtomicBool,
    hooks: std::sync::Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
}

impl ErrStream {
    /// Write `text` to `w` (stderr) and flush it. A refused write closes the stream: the
    /// hooks run, once, and nothing is written from then on. A stream that is only full for
    /// now (`WouldBlock`: a non-blocking pipe) loses the text and stays open.
    fn write(&self, w: &mut dyn Write, text: &str) {
        use std::sync::atomic::Ordering;
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let Err(e) = w.write_all(text.as_bytes()).and_then(|()| w.flush()) else {
            return;
        };
        if e.kind() == std::io::ErrorKind::WouldBlock || self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // Taken out first: a hook runs without the lock.
        let hooks = std::mem::take(&mut *self.hooks.lock().unwrap_or_else(|e| e.into_inner()));
        for hook in hooks {
            hook();
        }
    }

    /// Run `hook` when the stream closes (at once when it already has).
    fn on_closed(&self, hook: Box<dyn Fn() + Send + Sync>) {
        let mut hooks = self.hooks.lock().unwrap_or_else(|e| e.into_inner());
        // Read under the lock: a stream closing now either takes the hook or is seen here.
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            drop(hooks);
            hook();
        } else {
            hooks.push(hook);
        }
    }
}

static STDERR: std::sync::LazyLock<ErrStream> = std::sync::LazyLock::new(ErrStream::default);

/// What the stderr writer thread is handed.
enum ErrMsg {
    Text(String),
    /// Answer once everything before it is written.
    Flush(std::sync::mpsc::SyncSender<()>),
}

/// Texts for stderr waiting for the writer thread, at most.
const STDERR_QUEUE: usize = 1024;

/// The one thread that writes to stderr, and its queue (`None`: it could not start; texts
/// are then written where they are produced).
///
/// A stderr that is open but not read (a terminal paused with Ctrl-S, a pipe nobody drains)
/// blocks the write that fills it, and every later one waits for its lock. 9.3.2 wrote where
/// the text was produced: the signal handler's "stopping…" waited behind the live line, so
/// the second Ctrl-C, SIGTERM or SIGHUP was never seen and nothing could stop the process,
/// and a cancelled run's cleanup waited too. Now only this thread waits on stderr; whoever
/// produces a text queues it, and a full queue drops it (nobody is reading).
static STDERR_QUEUE_TX: std::sync::LazyLock<Option<std::sync::mpsc::SyncSender<ErrMsg>>> =
    std::sync::LazyLock::new(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<ErrMsg>(STDERR_QUEUE);
        std::thread::Builder::new()
            .name("oprobe-stderr".into())
            .spawn(move || {
                for msg in rx {
                    match msg {
                        ErrMsg::Text(text) => STDERR.write(&mut std::io::stderr().lock(), &text),
                        ErrMsg::Flush(done) => {
                            let _ = done.try_send(());
                        }
                    }
                }
            })
            .ok()
            .map(|_| tx)
    });

/// Write `text` to stderr as it is (no newline added: a live line redraws itself). Never
/// panics, never fails and never blocks: see the module documentation and
/// [`STDERR_QUEUE_TX`].
pub fn stderr_text(text: &str) {
    match &*STDERR_QUEUE_TX {
        Some(tx) => {
            let _ = tx.try_send(ErrMsg::Text(text.to_owned()));
        }
        None => STDERR.write(&mut std::io::stderr().lock(), text),
    }
}

/// Wait until every text queued for stderr so far is written, for at most `timeout`;
/// whether it was. Called before stdout is written (a terminal shows both in order) and
/// before the process exits.
pub fn flush_stderr(timeout: std::time::Duration) -> bool {
    let Some(tx) = &*STDERR_QUEUE_TX else {
        return true;
    };
    let deadline = std::time::Instant::now() + timeout;
    let (done, written) = std::sync::mpsc::sync_channel(1);
    let mut msg = ErrMsg::Flush(done);
    loop {
        match tx.try_send(msg) {
            Ok(()) => break,
            Err(std::sync::mpsc::TrySendError::Full(back)) => {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                msg = back;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return true,
        }
    }
    written
        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        .is_ok()
}

/// Write one line to stderr. Never panics and never fails: see the module documentation.
pub fn stderr_line(line: impl std::fmt::Display) {
    stderr_text(&format!("{line}\n"));
}

/// Run `hook` once when a write to stderr is refused because its reader is gone (at once when
/// that has already happened). A command that measures stops there as it does on a signal:
/// nobody is left to read its progress, and it must still clean up.
pub fn on_stderr_closed(hook: impl Fn() + Send + Sync + 'static) {
    STDERR.on_closed(Box::new(hook));
}

/// Exit code of a command whose reader closed stdout (`oprobe runs list | head`): 128 + SIGPIPE,
/// as shells report a CLI stopped by a broken pipe.
pub const EXIT_BROKEN_PIPE: u8 = 141;

/// Whether `e` is a write to a closed pipe.
pub fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            || c.downcast_ref::<serde_json::Error>()
                .and_then(|j| j.io_error_kind())
                .is_some_and(|k| k == std::io::ErrorKind::BrokenPipe)
    })
}

/// Exit quietly (no panic message) when a `print!` hits a closed stdout.
pub fn quiet_broken_pipes() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or_default();
        if msg.starts_with("failed printing to stdout") && msg.contains("Broken pipe") {
            std::process::exit(EXIT_BROKEN_PIPE.into());
        }
        default(info);
    }));
}

/// Whether stdin and stdout are both terminals.
pub fn interactive_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A warning that quotes a storage error never prints or keeps (the JSON envelope's
    /// `warnings`, the `log` events) the signature of the URL in it.
    #[test]
    fn warnings_never_carry_a_urls_credentials() {
        let out = Output::silent();
        let events = out.bus().channel();
        out.warn("cleanup failed: GET https://b.example/k?X-Amz-Expires=60&X-Amz-Signature=SECRET");
        let kept = out.warnings();
        assert_eq!(
            kept,
            ["cleanup failed: GET https://b.example/k?X-Amz-Expires=60"]
        );
        let logged = format!("{:?}", events.try_recv());
        assert!(logged.contains("X-Amz-Expires=60"), "{logged}");
        assert!(!logged.contains("SECRET"), "{logged}");
    }

    /// A stderr whose reader is gone (a closed terminal, `2>&1 | head`) refuses the write:
    /// 9.2 printed with `eprintln!`, which panics there, and the panic skipped cleanup. The
    /// refused write is no error to the caller; it runs the hooks once, and later lines are
    /// dropped.
    #[test]
    fn a_closed_stderr_runs_its_hooks_once_and_never_fails() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Accepts `open` writes, then fails every one as `kind`.
        struct Pipe {
            open: usize,
            kind: std::io::ErrorKind,
            written: String,
            attempts: usize,
        }
        impl Write for Pipe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.attempts += 1;
                if self.open == 0 {
                    return Err(self.kind.into());
                }
                self.open -= 1;
                self.written.push_str(&String::from_utf8_lossy(buf));
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let pipe = |open, kind| Pipe {
            open,
            kind,
            written: String::new(),
            attempts: 0,
        };
        let counter = |stream: &ErrStream| {
            let fired = Arc::new(AtomicUsize::new(0));
            let f = fired.clone();
            stream.on_closed(Box::new(move || {
                f.fetch_add(1, Ordering::SeqCst);
            }));
            fired
        };

        let stream = ErrStream::default();
        let fired = counter(&stream);
        let mut w = pipe(1, std::io::ErrorKind::BrokenPipe);
        stream.write(&mut w, "uploading\n");
        assert_eq!(fired.load(Ordering::SeqCst), 0);
        stream.write(&mut w, "cleaning up\n");
        assert_eq!(fired.load(Ordering::SeqCst), 1, "the refused write");
        stream.write(&mut w, "more\n");
        stream.write(&mut w, "and more\n");
        assert_eq!(fired.load(Ordering::SeqCst), 1, "once");
        assert_eq!(w.written, "uploading\n");
        assert_eq!(w.attempts, 2, "a closed stream is not written to again");
        // A hook registered after the stream closed runs at once.
        assert_eq!(counter(&stream).load(Ordering::SeqCst), 1);

        // A terminal that hung up answers `EIO`, not `EPIPE`: closed all the same.
        let stream = ErrStream::default();
        let fired = counter(&stream);
        stream.write(&mut pipe(0, std::io::ErrorKind::Other), "x\n");
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        // A pipe that is only full loses the line and stays open.
        let stream = ErrStream::default();
        let fired = counter(&stream);
        stream.write(&mut pipe(0, std::io::ErrorKind::WouldBlock), "x\n");
        assert_eq!(fired.load(Ordering::SeqCst), 0);
        let mut w = pipe(1, std::io::ErrorKind::BrokenPipe);
        stream.write(&mut w, "y\n");
        assert_eq!(w.written, "y\n");
    }

    #[test]
    fn formats_parse() {
        for (s, f) in [("text", OutputFormat::Text), ("JSON", OutputFormat::Json)] {
            assert_eq!(OutputFormat::parse(s), Some(f));
        }
        assert!(OutputFormat::parse("xml").is_none());
        assert!(OutputFormat::parse("html").is_none());
        assert!(OutputFormat::Json.is_machine());
    }

    #[test]
    fn the_envelope_adds_next_and_warnings() {
        #[derive(Serialize, JsonSchema)]
        struct R {
            n_count: u64,
        }
        impl CommandOutput for R {
            fn text(&self) -> String {
                String::new()
            }
            fn next(&self) -> Vec<NextStep> {
                vec![NextStep::new("show it", "oprobe show last")]
            }
            fn warnings(&self) -> Vec<String> {
                vec!["own".into()]
            }
        }
        let v = envelope_value(&R { n_count: 3 }, vec!["own".into(), "warned".into()]);
        assert_eq!(v["n_count"], 3);
        assert_eq!(v["next"][0]["cli"], "oprobe show last");
        assert_eq!(v["warnings"], serde_json::json!(["own", "warned"]));
        let t = envelope_value(&TextOutput { text: "x".into() }, vec![]);
        assert_eq!(t["next"], serde_json::json!([]));
        let o = Output::silent();
        o.warn("careful");
        assert_eq!(o.warnings(), vec!["careful".to_owned()]);
    }

    #[test]
    fn broken_pipes_are_recognized() {
        let e: anyhow::Error = std::io::Error::from(std::io::ErrorKind::BrokenPipe).into();
        assert!(is_broken_pipe(&e.context("writing the result")));
        let e: anyhow::Error = std::io::Error::from(std::io::ErrorKind::NotFound).into();
        assert!(!is_broken_pipe(&e));
    }
}
