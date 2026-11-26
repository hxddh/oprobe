//! Output plumbing for every command: `-o/--output text|json`.
//!
//! A command returns a typed result implementing [`CommandOutput`] (`Serialize + JsonSchema`
//! plus a text renderer) and hands it to [`Output::finish`]:
//! - `text`: the renderer's output on stdout;
//! - `json`: exactly one JSON document on stdout.
//!
//! stdout carries data only; logs and progress go to stderr. Errors are printed by
//! [`Output::error`]: JSON on stdout in json mode, `error:` + `hint:` on stderr otherwise.
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
            eprintln!("{msg}");
        }
        self.bus.log(LogLevel::Info, msg);
    }

    /// A warning: stderr (even with `-q`) and a `log` event.
    pub fn warn(&self, msg: impl Into<String>) {
        let msg = crate::util::strip_url_credentials(&msg.into());
        if let Ok(mut w) = self.warned.lock() {
            w.push(crate::report::style::strip_ansi(&msg));
        }
        eprintln!("{} {msg}", yellow("warning:"));
        self.bus.log(LogLevel::Warn, msg);
    }

    /// The warnings given so far.
    pub fn warnings(&self) -> Vec<String> {
        self.warned.lock().map(|w| w.clone()).unwrap_or_default()
    }

    /// Print a command's result and return its exit code.
    pub fn finish<T: CommandOutput>(&self, v: &T) -> anyhow::Result<u8> {
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
                eprintln!("{} {}", red(&bold("error:")), e.message);
                if let Some(h) = &e.hint {
                    eprintln!("{} {h}", yellow(&bold("hint:")));
                }
                eprintln!("{}", dim(&format!("({})", e.code)));
            }
        }
        e.exit_code()
    }
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
