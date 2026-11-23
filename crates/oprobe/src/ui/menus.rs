//! The app's menus (design app-native): the application menu (the system's menu bar on
//! macOS; on Windows and Linux the same menus in the toolbar's primary menu, since a hidden
//! title bar shows no menu bar), and the context menu of the runs in the sidebar. Shortcuts use
//! `mod` (⌘ on macOS, Ctrl elsewhere) and work whether a menu is open or not.

use nsh_core::desktop::MenuRole;
use nsh_core::element::desktop::{MenuItem, popup_menu, set_application_menu};
use nsh_core::element::os;
use nsh_core::platform::Os;
use nsh_core::prelude::*;

use crate::ui::i18n::{tr, trf};
use crate::ui::store::{Dialog, Page, Store};

/// Whether the app runs on macOS (its menu bar is the system's; the others show the menus in
/// the toolbar).
pub fn mac() -> bool {
    matches!(os(), Os::MacOs)
}

/// Show or hide the sidebar: ⌃⌘S on macOS (as Finder and Xcode), Ctrl+Shift+S elsewhere.
/// `MenuItem::shortcut` checks a keystroke as off macOS (`mod` is Ctrl there), so the macOS
/// one names Command itself: `ctrl+mod` would be Ctrl twice and panic.
const SIDEBAR_MAC: &str = "ctrl+cmd+s";
const SIDEBAR: &str = "mod+shift+s";

/// Every shortcut of the menus, checked in tests as `MenuItem::shortcut` checks them.
#[cfg(test)]
const ALL: &[&str] = &[
    "mod+,",
    "mod+n",
    "mod+o",
    "mod+shift+c",
    "mod+r",
    "mod+.",
    "mod+shift+p",
    "mod+backspace",
    "mod+1",
    "mod+2",
    SIDEBAR_MAC,
    SIDEBAR,
];

/// The documentation's address.
const DOCS: &str = "https://github.com/hxddh/oprobe/tree/main/docs";

/// The menus of the menu bar, in the app's language and state.
pub fn application(s: Store) -> Vec<MenuItem> {
    let running = s.running();
    let proving = s.proving();
    let one = s.selection.with(|v| v.len() == 1) && s.current.with(|c| c.is_some());
    let sel = s.selection.get();
    let page = s.page.get();
    let sidebar = s.sidebar.get();
    let mut menus = Vec::new();
    if mac() {
        menus.push(MenuItem::submenu(
            "oprobe",
            [
                MenuItem::role(MenuRole::About),
                MenuItem::separator(),
                MenuItem::new(tr("menu.settings"))
                    .shortcut("mod+,")
                    .on_select(move || s.dialog.set(Some(Dialog::Settings))),
                MenuItem::separator(),
                MenuItem::role(MenuRole::Hide),
                MenuItem::role(MenuRole::HideOthers),
                MenuItem::role(MenuRole::ShowAll),
                MenuItem::separator(),
                MenuItem::role(MenuRole::Quit),
            ],
        ));
    }
    let mut file = vec![
        MenuItem::new(tr("menu.new_run"))
            .shortcut("mod+n")
            .on_select(move || s.dialog.set(Some(Dialog::NewRun))),
        MenuItem::new(tr("menu.open_file"))
            .shortcut("mod+o")
            .on_select(move || s.pick_run_file()),
        MenuItem::separator(),
        MenuItem::role(MenuRole::Close),
    ];
    if !mac() {
        file.push(MenuItem::separator());
        file.push(
            MenuItem::new(tr("menu.settings"))
                .shortcut("mod+,")
                .on_select(move || s.dialog.set(Some(Dialog::Settings))),
        );
        file.push(MenuItem::role(MenuRole::Quit));
    }
    menus.push(MenuItem::submenu(tr("menu.file"), file));
    let copy_ids = sel.join(" ");
    menus.push(MenuItem::submenu(
        tr("menu.edit"),
        [
            MenuItem::role(MenuRole::Undo),
            MenuItem::role(MenuRole::Redo),
            MenuItem::separator(),
            MenuItem::role(MenuRole::Cut),
            MenuItem::role(MenuRole::Copy),
            MenuItem::role(MenuRole::Paste),
            MenuItem::role(MenuRole::SelectAll),
            MenuItem::separator(),
            MenuItem::new(tr("menu.copy_id"))
                .shortcut("mod+shift+c")
                .disabled(sel.is_empty())
                .on_select(move || s.copy(copy_ids.clone())),
        ],
    ));
    let del = sel.clone();
    let cmp = sel.clone();
    menus.push(MenuItem::submenu(
        tr("menu.run"),
        [
            MenuItem::new(tr("menu.run_again"))
                .shortcut("mod+r")
                .disabled(running || !one)
                .on_select(move || s.run_again()),
            MenuItem::new(tr("menu.stop"))
                .shortcut("mod+.")
                .disabled(!running && !proving)
                .on_select(move || {
                    s.stop_run();
                    s.stop_prove();
                }),
            MenuItem::separator(),
            MenuItem::new(tr("menu.prove"))
                .shortcut("mod+shift+p")
                .disabled(proving || !one)
                .on_select(move || {
                    s.page.set(Page::Operator);
                    s.start_prove();
                }),
            MenuItem::new(tr("menu.compare"))
                .disabled(cmp.len() < 2)
                .on_select(move || s.show_selection(cmp.clone())),
            MenuItem::separator(),
            MenuItem::new(tr("menu.delete"))
                .shortcut("mod+backspace")
                .disabled(del.is_empty())
                .on_select(move || s.ask_delete(del.clone())),
        ],
    ));
    let mut view = vec![
        MenuItem::new(tr("nav.profile"))
            .shortcut("mod+1")
            .checked(page == Page::Profile)
            .on_select(move || s.page.set(Page::Profile)),
        MenuItem::new(tr("nav.operator"))
            .shortcut("mod+2")
            .checked(page == Page::Operator)
            .on_select(move || s.page.set(Page::Operator)),
        MenuItem::separator(),
        MenuItem::new(if sidebar {
            tr("menu.hide_sidebar")
        } else {
            tr("menu.show_sidebar")
        })
        .shortcut(if mac() { SIDEBAR_MAC } else { SIDEBAR })
        .on_select(move || s.sidebar.update(|v| *v = !*v)),
    ];
    if mac() {
        view.push(MenuItem::separator());
        view.push(MenuItem::role(MenuRole::ToggleFullScreen));
    }
    menus.push(MenuItem::submenu(tr("menu.view"), view));
    if mac() {
        menus.push(MenuItem::submenu(
            tr("menu.window"),
            [
                MenuItem::role(MenuRole::Minimize),
                MenuItem::role(MenuRole::Zoom),
            ],
        ));
    }
    menus.push(MenuItem::submenu(
        tr("menu.help"),
        [MenuItem::new(tr("menu.docs")).on_select(|| {
            let _ = open_url(DOCS);
        })],
    ));
    menus
}

/// Keep the menu bar in step with the language and the app's state.
pub fn install(s: Store) {
    effect(move || {
        let _ = s.lang.get();
        set_application_menu(application(s));
    });
}

/// The menus as one popup (the toolbar's primary menu on Windows and Linux).
pub fn show_primary(s: Store) {
    let items = untracked(|| application(s));
    let _ = popup_menu(items);
}

/// The context menu of run `id` in the sidebar: it acts on the selection when `id` is in it,
/// else on `id` alone.
pub fn run_context(s: Store, id: &str) {
    let sel = s.selection.get_untracked();
    let ids = if sel.iter().any(|x| x == id) {
        sel
    } else {
        vec![id.to_owned()]
    };
    let one = ids.len() == 1;
    let (open, cmp, prove, copy, cmd, del) = (
        ids.clone(),
        ids.clone(),
        ids[0].clone(),
        ids.join(" "),
        ids.iter()
            .map(|i| format!("oprobe show {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
        ids.clone(),
    );
    let items = vec![
        MenuItem::new(if one {
            tr("menu.open").to_owned()
        } else {
            trf("menu.compare_n", &[&ids.len()])
        })
        .on_select(move || s.show_selection(open.clone())),
        MenuItem::new(tr("menu.compare_with"))
            .disabled(!one || s.selection.with_untracked(|v| v.is_empty() || v == &cmp))
            .on_select({
                let id = cmp[0].clone();
                move || s.select(&id, crate::ui::store::Select::Toggle)
            }),
        MenuItem::new(tr("menu.prove"))
            .disabled(!one || s.proving())
            .on_select(move || {
                s.open_run(&prove);
                s.page.set(Page::Operator);
            }),
        MenuItem::separator(),
        MenuItem::new(tr("menu.copy_id")).on_select(move || s.copy(copy.clone())),
        MenuItem::new(tr("menu.copy_command")).on_select(move || s.copy(cmd.clone())),
        MenuItem::separator(),
        MenuItem::new(tr("menu.delete")).on_select(move || s.ask_delete(del.clone())),
    ];
    let _ = popup_menu(items);
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_shortcut_is_one_on_every_platform() {
        for k in super::ALL {
            for mac in [false, true] {
                assert!(
                    nsh_core::desktop::Shortcut::parse(k, mac).is_ok(),
                    "{k} (mac: {mac})"
                );
            }
            // `MenuItem::shortcut` panics on a keystroke that is not one.
            let _ = nsh_core::element::desktop::MenuItem::new("x").shortcut(k);
        }
    }
}
