//! Saving reports, profiles and code: the system save dialog on desktops, the working
//! directory in a terminal (or where no dialog is available).

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
        let base: PathBuf = match chosen {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(_) => std::env::current_dir().unwrap_or_default().join(&suggested),
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
