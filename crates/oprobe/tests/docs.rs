//! The documentation stays in sync with the code.
//!
//! - `llms-full.txt` is the docs in one file. Refresh it with `OPROBE_BLESS=1 cargo test --test
//!   docs` after changing a page.
//! - `docs/cli.md` names every command and every flag that `--help` shows.
//! - Relative links between the docs resolve.

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../..")).to_path_buf()
}

/// Pages concatenated into `llms-full.txt` (llmstxt.org), in reading order.
const LLMS_FULL: &[&str] = &[
    "README.md",
    "docs/getting-started.md",
    "docs/concepts.md",
    "docs/operator.md",
    "docs/cli.md",
    "docs/reading-the-profile.md",
    "docs/embedding.md",
    "docs/app.md",
    "docs/install.md",
    "docs/troubleshooting.md",
    "docs/privacy.md",
];

fn llms_full() -> String {
    let mut out = String::from(
        "# oprobe — full documentation\n\n> Every page of the oprobe documentation in one file \
         (generated from README.md and docs/; see llms.txt for the index).\n",
    );
    for p in LLMS_FULL {
        let text = std::fs::read_to_string(root().join(p))
            .unwrap_or_else(|e| panic!("{p} is listed in LLMS_FULL but cannot be read: {e}"));
        out.push_str(&format!("\n\n<!-- source: {p} -->\n\n"));
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

#[test]
fn llms_full_matches_the_docs() {
    let path = root().join("llms-full.txt");
    let want = llms_full();
    if std::env::var_os("OPROBE_BLESS").is_some() {
        std::fs::write(&path, &want).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        have == want,
        "llms-full.txt is out of date with the docs; regenerate it with \
         `OPROBE_BLESS=1 cargo test --test docs`"
    );
}

/// Files other parts of the project own (links to them may land before the file does).
const LINKED_ELSEWHERE: &[&str] = &["../AGENTS.md", "AGENTS.md"];

/// Relative links between the docs point at files that exist.
#[test]
fn relative_doc_links_resolve() {
    let root = root();
    let mut files = vec![root.join("README.md"), root.join("README.zh-CN.md")];
    for e in std::fs::read_dir(root.join("docs")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "md") {
            files.push(p);
        }
    }
    let mut broken = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        let dir = f.parent().unwrap();
        for (i, _) in text.match_indices("](") {
            let rest = &text[i + 2..];
            let Some(end) = rest.find(')') else { continue };
            let link = rest[..end].split('#').next().unwrap_or("").trim();
            if link.is_empty()
                || link.contains("://")
                || link.starts_with("mailto:")
                || link.contains(' ')
            {
                continue;
            }
            if !dir.join(link).exists() && !LINKED_ELSEWHERE.contains(&link) {
                broken.push(format!("{}: {link}", f.display()));
            }
        }
    }
    assert!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));
}

/// `oprobe [COMMAND] --help`, as the user sees it.
fn help(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .args(args)
        .arg("--help")
        .env("NO_COLOR", "1")
        .env("COLUMNS", "200")
        .output()
        .unwrap();
    assert!(out.status.success(), "oprobe {args:?} --help failed");
    String::from_utf8(out.stdout).unwrap()
}

/// The long flags of a help text (`--max-requests`), from its option lines.
fn long_flags(help: &str) -> Vec<String> {
    let mut flags = Vec::new();
    for line in help.lines() {
        let l = line.trim_start();
        if !l.starts_with('-') {
            continue;
        }
        for word in l.split([' ', ',', '=', '<', '[']) {
            if let Some(name) = word.strip_prefix("--")
                && !name.is_empty()
                && name.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                && !flags.contains(&format!("--{name}"))
            {
                flags.push(format!("--{name}"));
            }
        }
    }
    flags
}

/// `doc` names `flag` as a whole word (`--max` is not `--max-bytes`).
fn mentions(doc: &str, flag: &str) -> bool {
    doc.match_indices(flag).any(|(i, _)| {
        doc[i + flag.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-'))
    })
}

/// The subcommands `oprobe --help` lists.
fn commands(help: &str) -> Vec<String> {
    let Some((_, rest)) = help.split_once("Commands:") else {
        return Vec::new();
    };
    rest.lines()
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| *c != "help")
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_cli_reference_names_every_command_and_flag() {
    let doc = std::fs::read_to_string(root().join("docs/cli.md")).unwrap();
    let top = help(&[]);
    let cmds = commands(&top);
    assert!(
        cmds.iter().any(|c| c == "run") && cmds.iter().any(|c| c == "prove"),
        "unexpected `oprobe --help`: {top}"
    );
    let mut missing = Vec::new();
    for c in &cmds {
        if !doc.contains(&format!("## `oprobe {c}")) {
            missing.push(format!("a `## `oprobe {c}` section"));
        }
    }
    for args in std::iter::once(Vec::new()).chain(cmds.iter().map(|c| vec![c.as_str()])) {
        for f in long_flags(&help(&args)) {
            if !mentions(&doc, &f) {
                missing.push(format!("{f} (oprobe {})", args.join(" ")));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "docs/cli.md does not mention:\n{}",
        missing.join("\n")
    );
}

/// The README promises only the checks the installers always run (design v6 §8, U7): the
/// minisign signature is checked only when a release embeds a key, so the README does not
/// mention it (docs/install.md explains when it applies).
#[test]
fn the_readme_does_not_promise_a_signature_check() {
    for f in ["README.md", "README.zh-CN.md"] {
        let text = std::fs::read_to_string(root().join(f)).unwrap();
        assert!(
            !text.to_lowercase().contains("minisign"),
            "{f} mentions minisign; see docs/install.md#verifying-a-download"
        );
    }
}

/// Double-clicking a `.oprobe` file opens it (A6): the Linux `.desktop` entry passes the files
/// and claims the MIME type its shared-mime-info file defines, and the MSI registers the
/// extension with the binary.
#[test]
fn the_packages_register_run_files() {
    let read = |f: &str| std::fs::read_to_string(root().join(f)).unwrap();
    let desktop = read("packaging/desktop/io.github.hxddh.oprobe.desktop");
    let lines: Vec<&str> = desktop.lines().collect();
    assert!(lines.contains(&"Exec=oprobe %F"), "{desktop}");
    assert!(
        lines.contains(&"MimeType=application/x-oprobe-run;"),
        "{desktop}"
    );
    let mime = read("packaging/desktop/io.github.hxddh.oprobe.xml");
    assert!(
        mime.contains("<mime-type type=\"application/x-oprobe-run\">")
            && mime.contains("<glob pattern=\"*.oprobe\"/>"),
        "{mime}"
    );
    let wxs = read("packaging/windows/oprobe.wxs");
    assert!(
        wxs.contains("<Extension Id=\"oprobe\" ContentType=\"application/x-oprobe-run\">")
            && wxs.contains(
                "<Verb Id=\"open\" TargetFile=\"oprobe.exe\" Argument=\"&quot;%1&quot;\" />"
            ),
        "{wxs}"
    );
}

/// The release notes advertise a package channel only when the release publishes to it: its
/// secret is set (P1).
#[test]
fn release_notes_name_only_published_channels() {
    let yml = std::fs::read_to_string(root().join(".github/workflows/release.yml")).unwrap();
    let notes = yml
        .split("- name: Release notes from CHANGELOG")
        .nth(1)
        .and_then(|s| s.split("- uses:").next())
        .unwrap();
    for (secret, flag, channel) in [
        ("TAP_TOKEN", "HAS_TAP_TOKEN", "brew install"),
        ("SCOOP_TOKEN", "HAS_SCOOP_TOKEN", "scoop install"),
        ("CARGO_REGISTRY_TOKEN", "HAS_CARGO_TOKEN", "cargo install"),
    ] {
        assert!(
            notes.contains(&format!("{flag}: ${{{{ secrets.{secret} != '' }}}}")),
            "{secret}"
        );
        let before = notes.split(channel).next().unwrap();
        let guard = before.rsplit("if [").next().unwrap();
        assert!(
            guard.starts_with(&format!(" \"${flag}\" = true ]")),
            "`{channel}` is printed without a check of {secret}: {guard}"
        );
    }
}
