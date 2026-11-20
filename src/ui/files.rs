//! Saving and opening files: the system dialogs on desktops; in a terminal (or where no dialog
//! is available) the working directory (Documents or home when that is `/`), under a name that
//! never replaces an existing file.

use std::path::PathBuf;

use nsh_core::file_dialog::{OpenDialog, SaveDialog};
use nsh_core::prelude::*;

use crate::ui::kit::Tone;
use crate::ui::store::Store;

/// Ask where to save `bytes` (suggesting `name`), write it, and say so.
pub fn save(store: Store, name: String, bytes: Vec<u8>) {
    spawn(async move {
        let chosen = if crate::ui::kit::in_terminal() {
            Err("terminal".to_owned())
        } else {
            pick_save_path(SaveDialog::new().file_name(name.clone())).await
        };
        let path: PathBuf = match chosen {
            // The system dialog already asked before replacing a file.
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(_) => unused_path(&fallback_dir().join(&name)),
        };
        let p2 = path.clone();
        let res = blocking(move || std::fs::write(&p2, bytes)).await;
        match res {
            Ok(Ok(())) => {
                store.toast(format!("Saved {}", path.display()), Tone::Ok);
                if !crate::ui::kit::in_terminal() {
                    let _ = reveal_path(path);
                }
            }
            Ok(Err(e)) => store.toast(
                format!("could not save {}: {e}", path.display()),
                Tone::Crit,
            ),
            Err(e) => store.toast(format!("could not save: {e}"), Tone::Crit),
        }
    });
}

/// Ask for `.oprobe` run files and open them.
pub fn open(store: Store) {
    spawn(async move {
        if crate::ui::kit::in_terminal() {
            store.toast(
                "Open a run file with `oprobe top FILE.oprobe`, or drop it on the window",
                Tone::Info,
            );
            return;
        }
        match pick_paths(OpenDialog::files()).await {
            Ok(Some(paths)) => {
                for p in paths {
                    store.open_path(p);
                }
            }
            Ok(None) => {}
            Err(e) => store.toast(format!("cannot show the open dialog: {e}"), Tone::Crit),
        }
    });
}

/// Where files go without a save dialog: the working directory in a terminal (where the user
/// started oprobe), else (or when that is `/`, as under Finder) Documents, else home.
fn fallback_dir() -> PathBuf {
    let cwd = std::env::current_dir().ok();
    if crate::ui::kit::in_terminal()
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

/// `path`, or `name-1.ext`, `name-2.ext`, … when it already exists.
pub(crate) fn unused_path(path: &std::path::Path) -> PathBuf {
    if !path.exists() {
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
        .find(|p| !p.exists())
        .expect("an unused name")
}

/// Export the shown run: its file (`.oprobe`), the report as JSON or Markdown.
pub fn export_run(store: Store, format: &'static str) {
    let data = store.current.with_untracked(|c| {
        c.as_ref().map(|c| {
            let id = c.id().to_owned();
            let bytes = match format {
                "json" => serde_json::to_vec_pretty(&c.run.report).unwrap_or_default(),
                "md" => crate::report::markdown::render(&c.run.report).into_bytes(),
                _ => crate::store::runs::encode_run(&c.run).unwrap_or_default(),
            };
            (id, bytes)
        })
    });
    match data {
        Some((id, bytes)) => save(store, format!("oprobe-{id}.{format}"), bytes),
        None => store.toast("Nothing to export: open a run first", Tone::Warn),
    }
}
