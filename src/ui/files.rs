//! Saving reports, profiles and code: the system save dialog on desktops; in a terminal (or
//! where no dialog is available) the working directory (Documents or home when that is `/`),
//! under a name that does not replace an existing file.

use std::path::PathBuf;

use nsh_core::file_dialog::SaveDialog;
use nsh_core::prelude::*;

use crate::ui::store::Store;

fn save(store: Store, suggested: String, contents: Vec<(String, String)>) {
    spawn(async move {
        let chosen = if nsh_base::terminal::is_terminal() {
            Err("terminal".to_owned())
        } else {
            pick_save_path(SaveDialog::new().file_name(suggested.clone())).await
        };
        let exts: Vec<String> = contents.iter().map(|(e, _)| e.clone()).collect();
        let base: PathBuf = match chosen {
            // The system dialog already asked before replacing a file.
            Ok(Some(p)) => p,
            Ok(None) => return,
            // No dialog: never overwrite silently.
            Err(_) => unused_path(&fallback_dir().join(&suggested), &exts),
        };
        let mut written = Vec::new();
        for (ext, body) in contents {
            let path = if ext.is_empty() {
                base.clone()
            } else {
                base.with_extension(ext)
            };
            let p2 = path.clone();
            let res = blocking(move || std::fs::write(&p2, body)).await;
            match res {
                Ok(Ok(())) => written.push(path.display().to_string()),
                Ok(Err(e)) => {
                    store.notice.set(Some(
                        format!("could not save {}: {e}", path.display()).into(),
                    ));
                    return;
                }
                Err(e) => {
                    store
                        .notice
                        .set(Some(format!("could not save: {e}").into()));
                    return;
                }
            }
        }
        store
            .notice
            .set(Some(format!("saved {}", written.join(", ")).into()));
    });
}

/// Where files go without a save dialog: the working directory in a terminal (where the user
/// started oprobe), else (or when that is `/`, as under Finder) Documents, else home.
fn fallback_dir() -> PathBuf {
    let cwd = std::env::current_dir().ok();
    if nsh_base::terminal::is_terminal()
        && let Some(d) = cwd.as_ref().filter(|d| d.parent().is_some())
    {
        return d.clone();
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    if let Some(h) = home {
        let docs = h.join("Documents");
        return if docs.is_dir() { docs } else { h };
    }
    cwd.unwrap_or_default()
}

/// `path`, or `name-1`, `name-2`, … when it (or a sibling with one of `exts`) already exists.
pub(crate) fn unused_path(path: &std::path::Path, exts: &[String]) -> PathBuf {
    let with = |p: &std::path::Path, e: &str| {
        if e.is_empty() {
            p.to_path_buf()
        } else {
            p.with_extension(e)
        }
    };
    let taken = |p: &std::path::Path| exts.iter().any(|e| with(p, e).exists());
    if !taken(path) {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|i| path.with_file_name(format!("{stem}-{i}{ext}")))
        .find(|p| !taken(p))
        .expect("an unused name")
}

pub fn save_report(store: Store) {
    let Some(r) = store.report.get_untracked() else {
        return;
    };
    let json = serde_json(&r);
    let md = crate::report::markdown::render(&r);
    let name = format!("oprobe-{}.json", crate::util::compact_utc_now());
    save(store, name, vec![("json".into(), json), ("md".into(), md)]);
}

pub fn save_tuned_profile(store: Store) {
    let Some(t) = store.tuned.get_untracked() else {
        return;
    };
    save(
        store,
        "tuned.toml".into(),
        vec![(String::new(), t.profile_toml)],
    );
}

pub fn save_tuned_rust(store: Store) {
    let Some(t) = store.tuned.get_untracked() else {
        return;
    };
    save(store, "storage.rs".into(), vec![(String::new(), t.rust)]);
}

fn serde_json(r: &crate::report::Report) -> String {
    crate::report::to_json(r)
}
