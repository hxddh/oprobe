//! Documentation generated from the code stays in sync with it.
//!
//! `docs/cli.md` is the CLI reference generated from the clap definitions and `llms-full.txt`
//! the docs in one file. Refresh both with `OPROBE_BLESS=1 cargo test --test docs` after
//! changing a command, a flag or a page.

use std::path::Path;

#[test]
fn cli_reference_matches_the_code() {
    let want = oprobe::cli::reference::markdown();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/cli.md");
    if std::env::var_os("OPROBE_BLESS").is_some() {
        std::fs::write(&path, &want).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        have == want,
        "docs/cli.md is out of date with the command definitions; regenerate it with \
         `OPROBE_BLESS=1 cargo test --test docs` (or `oprobe man --markdown > docs/cli.md`)"
    );
}

#[test]
fn cli_reference_documents_every_command() {
    let md = oprobe::cli::reference::markdown();
    for c in [
        "run",
        "demo",
        "runs show",
        "target add",
        "self-update",
        "config edit",
    ] {
        assert!(md.contains(&format!("## `oprobe {c}`")), "{c}");
    }
    assert!(md.contains("--detail"));
}

/// Pages concatenated into `llms-full.txt` (llmstxt.org), in reading order.
const LLMS_FULL: &[&str] = &[
    "README.md",
    "docs/getting-started.md",
    "docs/concepts.md",
    "docs/reading-the-report.md",
    "docs/metrics.md",
    "docs/workloads.md",
    "docs/ci.md",
    "docs/agents.md",
    "docs/tuning.md",
    "docs/cost.md",
    "docs/install.md",
    "docs/troubleshooting.md",
    "docs/privacy.md",
    "docs/cli.md",
];

fn llms_full() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = String::from(
        "# oprobe — full documentation\n\n> Every page of the oprobe documentation in one file \
         (generated from README.md and docs/; see llms.txt for the index).\n",
    );
    for p in LLMS_FULL {
        // A page another stream adds later is simply absent until it lands.
        let Ok(text) = std::fs::read_to_string(root.join(p)) else {
            continue;
        };
        out.push_str(&format!("\n\n<!-- source: {p} -->\n\n"));
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

#[test]
fn llms_full_matches_the_docs() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("llms-full.txt");
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

/// Files other parts of the project add (links to them may land before the file does).
const LINKED_ELSEWHERE: &[&str] = &["docs/agents.md", "agents.md", "../AGENTS.md", "AGENTS.md"];

/// Relative links between the docs point at files that exist.
#[test]
fn relative_doc_links_resolve() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
