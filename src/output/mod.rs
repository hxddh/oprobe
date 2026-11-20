//! Output plumbing for every command (design §9): `-o/--output text|json|ndjson|md|html`.
//!
//! A command returns a typed result implementing [`CommandOutput`] (`Serialize + JsonSchema`
//! plus text/Markdown renderers) and hands it to [`Output::finish`]:
//! - `text` / `md`: the renderer's output on stdout;
//! - `json`: exactly one JSON document on stdout;
//! - `ndjson`: the event stream (`oprobe.events/1`) on stdout, ending with a `result` event.
//!
//! stdout carries data only; logs and progress go to stderr (or become `log` events in
//! ndjson mode). Errors are printed by [`Output::error`]: JSON on stdout in json/ndjson modes,
//! `error:` + `hint:` on stderr otherwise.

use std::io::Write;

use schemars::JsonSchema;
use serde::Serialize;

use crate::errors::{ErrorBody, ErrorCode, OprobeError};
use crate::events::{Event, EventBus, EventKind, LogLevel, NdjsonWriter};
use crate::report::style::{bold, dim, red, yellow};

/// `-o/--output`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Ndjson,
    Md,
    Html,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            "ndjson" => Some(Self::Ndjson),
            "md" | "markdown" => Some(Self::Md),
            "html" => Some(Self::Html),
            _ => None,
        }
    }

    /// Machine formats print JSON (also for errors).
    pub fn is_machine(self) -> bool {
        matches!(self, Self::Json | Self::Ndjson)
    }
}

/// The typed result of a command.
pub trait CommandOutput: Serialize + JsonSchema {
    /// Human text (may contain ANSI colors when enabled).
    fn text(&self) -> String;

    /// Markdown; defaults to the text in a code block.
    fn markdown(&self) -> String {
        format!("```text\n{}```\n", self.text())
    }

    /// HTML (`-o html`); `None` until the HTML renderer exists for this result.
    fn html(&self) -> Option<String> {
        None
    }

    /// Exit code of the command for this result (0 unless a gate failed).
    fn exit_code(&self) -> u8 {
        0
    }

    /// The final `result` event of an ndjson stream.
    fn result_event(&self) -> EventKind {
        EventKind::Result {
            run_id: None,
            path: None,
            report: None,
            data: serde_json::to_value(self).ok(),
        }
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
}

impl Output {
    /// Set up output for `format`. In ndjson mode the event stream starts on stdout now
    /// (with its `version` line).
    pub fn new(format: OutputFormat, quiet: bool, non_interactive: bool) -> Self {
        let bus = EventBus::new();
        if format != OutputFormat::Text {
            // Machine and document formats never carry ANSI colors.
            crate::report::style::set(false);
        }
        if format == OutputFormat::Ndjson {
            NdjsonWriter::new(std::io::stdout()).attach(&bus);
            bus.emit_event(&Event::version());
        }
        Self {
            format,
            quiet,
            non_interactive,
            bus,
        }
    }

    /// An output that writes nothing to stdout (tests, in-process use).
    pub fn silent() -> Self {
        Self {
            format: OutputFormat::Json,
            quiet: true,
            non_interactive: true,
            bus: EventBus::new(),
        }
    }

    /// The event bus of this command (ndjson output is subscribed when `-o ndjson`).
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// A progress/log line: stderr in text/json/md, a `log` event in ndjson; nothing with `-q`.
    pub fn info(&self, msg: impl Into<String>) {
        let msg = msg.into();
        match self.format {
            OutputFormat::Ndjson => self.bus.log(LogLevel::Info, msg),
            _ if self.quiet => {}
            _ => eprintln!("{msg}"),
        }
    }

    /// A warning: stderr (even with `-q`) or a `log` event.
    pub fn warn(&self, msg: impl Into<String>) {
        let msg = msg.into();
        match self.format {
            OutputFormat::Ndjson => self.bus.log(LogLevel::Warn, msg),
            _ => eprintln!("{} {msg}", yellow("warning:")),
        }
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
            OutputFormat::Md => {
                let t = v.markdown();
                out.write_all(t.as_bytes())?;
                if !t.ends_with('\n') {
                    out.write_all(b"\n")?;
                }
            }
            OutputFormat::Json => {
                serde_json::to_writer_pretty(&mut out, v)?;
                out.write_all(b"\n")?;
            }
            OutputFormat::Ndjson => {
                drop(out);
                self.bus.emit(v.result_event());
            }
            OutputFormat::Html => match v.html() {
                Some(h) => out.write_all(h.as_bytes())?,
                None => {
                    return Err(OprobeError::new(
                        ErrorCode::UsageInvalidArgument,
                        "this command has no HTML output",
                    )
                    .with_hint("use -o text, -o md, -o json or -o ndjson")
                    .into());
                }
            },
        }
        let _ = std::io::stdout().flush();
        Ok(v.exit_code())
    }

    /// Print a classified error (JSON on stdout in json/ndjson modes) and return its exit code.
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
            OutputFormat::Ndjson => self.bus.emit(EventKind::Error { error: e.clone() }),
            _ => {
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

    #[test]
    fn formats_parse() {
        for (s, f) in [
            ("text", OutputFormat::Text),
            ("JSON", OutputFormat::Json),
            ("ndjson", OutputFormat::Ndjson),
            ("markdown", OutputFormat::Md),
            ("html", OutputFormat::Html),
        ] {
            assert_eq!(OutputFormat::parse(s), Some(f));
        }
        assert!(OutputFormat::parse("xml").is_none());
        assert!(OutputFormat::Ndjson.is_machine());
    }

    #[test]
    fn broken_pipes_are_recognized() {
        let e: anyhow::Error = std::io::Error::from(std::io::ErrorKind::BrokenPipe).into();
        assert!(is_broken_pipe(&e.context("writing the result")));
        let e: anyhow::Error = std::io::Error::from(std::io::ErrorKind::NotFound).into();
        assert!(!is_broken_pipe(&e));
    }

    #[test]
    fn html_without_a_renderer_is_a_usage_error() {
        let o = Output {
            format: OutputFormat::Html,
            quiet: true,
            non_interactive: true,
            bus: EventBus::new(),
        };
        let e = o.finish(&TextOutput { text: "x".into() }).unwrap_err();
        let body = crate::errors::classify(&e, "s3");
        assert_eq!(body.code, ErrorCode::UsageInvalidArgument);
        assert_eq!(body.exit_code(), 2);
        assert!(body.hint.unwrap().contains("-o md"));
    }
}
