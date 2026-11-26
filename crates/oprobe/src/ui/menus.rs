//! The app's menus (design app-native): the application menu (the system's menu bar on
//! macOS; on Windows and Linux its shortcuts work under the hidden title bar, and a flat
//! primary menu in the toolbar offers the app's commands), and the context menu of the runs in
//! the sidebar. Shortcuts use `mod` (⌘ on macOS, Ctrl elsewhere) and work whether a menu is
//! open or not; those a text field also uses (Delete, ⌘⌫) leave the key to the field. Every
//! item, the platform's role items included, is labelled in the app's language. While a dialog
//! or an alert is open, the app's own actions wait.

use nsh_core::desktop::{MenuRole, WindowMode};
use nsh_core::element::desktop::{MenuItem, popup_menu, popup_menu_at, set_application_menu};
use nsh_core::element::os;
use nsh_core::geometry::{Point, Points};
use nsh_core::keymap::When;
use nsh_core::platform::Os;
use nsh_core::prelude::*;

use crate::ui::i18n::{tr, trf};
use crate::ui::store::{Dialog, Page, Store};

/// Whether the app runs on macOS (its menu bar is the system's; the others show the menus in
/// the toolbar).
pub fn mac() -> bool {
    matches!(os(), Os::MacOs)
}

/// Whether the app runs on Windows (its words: Exit, Settings).
pub fn windows() -> bool {
    matches!(os(), Os::Windows)
}

/// Settings is a window of its own (macOS), not a dialog.
pub fn settings_window() -> bool {
    mac() && !crate::ui::kit::in_terminal()
}

/// Show or hide the sidebar: ⌃⌘S on macOS (as Finder and Xcode), Ctrl+Shift+S elsewhere.
/// `MenuItem::shortcut` checks a keystroke as off macOS (`mod` is Ctrl there), so the macOS
/// one names Command itself: `ctrl+mod` would be Ctrl twice and panic.
const SIDEBAR_MAC: &str = "ctrl+cmd+s";
const SIDEBAR: &str = "mod+shift+s";
/// Delete the selected runs: ⌘⌫ on macOS (Finder), Delete elsewhere (Explorer, Files).
const DELETE_MAC: &str = "mod+backspace";
const DELETE: &str = "delete";
/// Full screen off macOS (macOS's is the system's role item, ⌃⌘F).
const FULL_SCREEN: &str = "f11";
/// Quit on Linux (GNOME's Ctrl+Q; Windows quits with Alt+F4, the system's).
const QUIT: &str = "mod+q";

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
    "mod+1",
    "mod+2",
    SIDEBAR_MAC,
    SIDEBAR,
    DELETE_MAC,
    DELETE,
    FULL_SCREEN,
    QUIT,
];

/// The documentation of the installed version (its release tag, not `main`).
pub fn docs() -> String {
    format!(
        "https://github.com/hxddh/oprobe/tree/v{}/docs",
        env!("CARGO_PKG_VERSION")
    )
}

/// The project's address.
pub const HOME: &str = "https://github.com/hxddh/oprobe";

/// Help ▸ Documentation, and Help ▸ Glossary (the terms a profile prints).
fn help_items() -> [MenuItem; 2] {
    [
        MenuItem::new(tr("menu.docs")).on_select(|| {
            let _ = open_url(docs());
        }),
        MenuItem::new(tr("menu.glossary")).on_select(|| {
            let _ = open_url(crate::util::glossary_url());
        }),
    ]
}

/// A role item of the platform, labelled in the app's language.
fn role(r: MenuRole, key: &'static str) -> MenuItem {
    MenuItem::role(r).label(tr(key))
}

/// "Show in Finder" (macOS), "Show in Explorer" (Windows), "Show in Files" (elsewhere).
fn reveal_key() -> &'static str {
    if mac() {
        "menu.reveal_mac"
    } else if windows() {
        "menu.reveal_windows"
    } else {
        "menu.reveal"
    }
}

/// Settings is "Preferences" on GNOME, "Settings" on macOS and Windows.
fn settings_key() -> &'static str {
    if mac() || windows() {
        "menu.settings"
    } else {
        "menu.preferences"
    }
}

fn settings_item(s: Store, modal: bool) -> MenuItem {
    MenuItem::new(tr(settings_key()))
        .shortcut("mod+,")
        .disabled(modal)
        .on_select(move || s.open_settings())
}

fn about_item(s: Store, modal: bool) -> MenuItem {
    MenuItem::new(tr("menu.about"))
        .disabled(modal)
        .on_select(move || s.dialog.set(Some(Dialog::About)))
}

/// Quit off macOS: "Quit" with Ctrl+Q on Linux, "Exit" on Windows. It asks first while a
/// job runs.
fn quit_item(s: Store) -> MenuItem {
    if windows() {
        MenuItem::new(tr("menu.exit")).on_select(move || s.request_quit())
    } else {
        MenuItem::new(tr("menu.quit_plain"))
            .shortcut(QUIT)
            .on_select(move || s.request_quit())
    }
}

/// Enter or leave full screen (off macOS).
fn toggle_full_screen(s: Store) {
    let Some(w) = s.window.get_untracked() else {
        return;
    };
    let full = untracked(|| w.state()).is_some_and(|st| st.mode == WindowMode::FullScreen);
    w.set_mode(if full {
        WindowMode::Normal
    } else {
        WindowMode::FullScreen
    });
}

/// How many runs are selected, as far as a menu shows it: Copy Run ID and Delete need one,
/// Run Again and Prove exactly one, Compare two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selected {
    None,
    One,
    Several,
}

/// Everything the menu bar shows of the app's state: its language, what is enabled and
/// checked, the sidebar item's label. The bar is rebuilt when one of these changes and at no
/// other time ([`install`]); what an item acts on (which runs are selected) is read when the
/// item is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shown {
    lang: crate::ui::i18n::Lang,
    modal: bool,
    running: bool,
    proving: bool,
    selected: Selected,
    page: Page,
    sidebar: bool,
}

impl Shown {
    fn of(s: Store) -> Self {
        Self {
            lang: s.lang.get(),
            modal: s.modal(),
            running: s.running(),
            proving: s.proving(),
            selected: s.selection.with(|v| match v.len() {
                0 => Selected::None,
                1 => Selected::One,
                _ => Selected::Several,
            }),
            page: s.page.get(),
            sidebar: s.sidebar.get(),
        }
    }
}

/// The menus of the menu bar for what `shown` says of the app's state.
fn application(s: Store, shown: Shown) -> Vec<MenuItem> {
    let Shown {
        modal,
        running,
        proving,
        selected,
        page,
        sidebar,
        ..
    } = shown;
    // Run Again and Prove act on the run that is shown: while it still loads they do
    // nothing (`Store::run_again`, `Store::start_prove`).
    let one = selected == Selected::One;
    let mut menus = Vec::new();
    if mac() {
        menus.push(MenuItem::submenu(
            "oprobe",
            [
                role(MenuRole::About, "menu.about"),
                MenuItem::separator(),
                settings_item(s, modal),
                MenuItem::separator(),
                role(MenuRole::Hide, "menu.hide"),
                role(MenuRole::HideOthers, "menu.hide_others"),
                role(MenuRole::ShowAll, "menu.show_all"),
                MenuItem::separator(),
                role(MenuRole::Quit, "menu.quit"),
            ],
        ));
    }
    let mut file = vec![
        MenuItem::new(tr("menu.new_run"))
            .shortcut("mod+n")
            .disabled(modal)
            .on_select(move || s.dialog.set(Some(Dialog::NewRun))),
        MenuItem::new(tr("menu.open_file"))
            .shortcut("mod+o")
            .disabled(modal)
            .on_select(move || s.pick_run_file()),
        MenuItem::separator(),
        role(MenuRole::Close, "menu.close"),
    ];
    if !mac() {
        file.push(MenuItem::separator());
        file.push(settings_item(s, modal));
        file.push(MenuItem::separator());
        file.push(quit_item(s));
    }
    menus.push(MenuItem::submenu(tr("menu.file"), file));
    menus.push(MenuItem::submenu(
        tr("menu.edit"),
        [
            role(MenuRole::Undo, "menu.undo"),
            role(MenuRole::Redo, "menu.redo"),
            MenuItem::separator(),
            role(MenuRole::Cut, "menu.cut"),
            role(MenuRole::Copy, "menu.copy"),
            role(MenuRole::Paste, "menu.paste"),
            role(MenuRole::SelectAll, "menu.select_all"),
            MenuItem::separator(),
            MenuItem::new(tr("menu.copy_id"))
                .shortcut("mod+shift+c")
                .shortcut_when(When::NotEditing)
                .disabled(modal || selected == Selected::None)
                .on_select(move || s.copy(s.selection.get_untracked().join(" "))),
        ],
    ));
    menus.push(MenuItem::submenu(
        tr("menu.run"),
        [
            MenuItem::new(tr("menu.run_again"))
                .shortcut("mod+r")
                .disabled(modal || running || !one)
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
                .disabled(modal || proving || !one)
                .on_select(move || {
                    s.page.set(Page::Operator);
                    s.start_prove();
                }),
            MenuItem::new(tr("menu.compare"))
                .disabled(modal || selected != Selected::Several)
                .on_select(move || s.show_selection(s.selection.get_untracked())),
            MenuItem::separator(),
            MenuItem::new(tr("menu.delete"))
                .shortcut(if mac() { DELETE_MAC } else { DELETE })
                .shortcut_when(When::NotEditing)
                .disabled(modal || selected == Selected::None)
                .on_select(move || s.ask_delete(s.selection.get_untracked())),
        ],
    ));
    let mut view = vec![
        MenuItem::new(tr("nav.profile"))
            .shortcut("mod+1")
            .checked(page == Page::Profile)
            .disabled(modal)
            .on_select(move || s.page.set(Page::Profile)),
        MenuItem::new(tr("nav.operator"))
            .shortcut("mod+2")
            .checked(page == Page::Operator)
            .disabled(modal)
            .on_select(move || s.page.set(Page::Operator)),
        MenuItem::separator(),
        MenuItem::new(if sidebar {
            tr("menu.hide_sidebar")
        } else {
            tr("menu.show_sidebar")
        })
        .shortcut(if mac() { SIDEBAR_MAC } else { SIDEBAR })
        .disabled(modal)
        .on_select(move || s.sidebar.update(|v| *v = !*v)),
        MenuItem::separator(),
    ];
    if mac() {
        view.push(role(MenuRole::ToggleFullScreen, "menu.full_screen"));
    } else {
        view.push(
            MenuItem::new(tr("menu.full_screen"))
                .shortcut(FULL_SCREEN)
                .on_select(move || toggle_full_screen(s)),
        );
    }
    menus.push(MenuItem::submenu(tr("menu.view"), view));
    if mac() {
        menus.push(MenuItem::submenu(
            tr("menu.window"),
            [
                role(MenuRole::Minimize, "menu.minimize"),
                role(MenuRole::Zoom, "menu.zoom"),
            ],
        ));
    }
    let mut help: Vec<MenuItem> = help_items().into();
    if !mac() {
        help.push(MenuItem::separator());
        help.push(about_item(s, modal));
    }
    menus.push(MenuItem::submenu(tr("menu.help"), help));
    menus
}

/// Keep the menu bar in step with what it shows ([`Shown`]), and rebuild it for nothing
/// else. nsh gives the items of a rebuilt bar new ids, so a shortcut pressed while the bar is
/// rebuilt names an item that is gone and does nothing: 9.2 rebuilt it whenever the selection
/// changed (every arrow key in the sidebar, every run that landed or finished loading), and
/// its tests waited for the bar to settle. It is still rebuilt when the language changes, a
/// dialog or an alert opens or closes, a run or a prove starts or ends, the selection goes
/// between none, one and several runs, the page changes (its check mark) or the sidebar
/// shows or hides (its item's label): each changes an item.
pub fn install(s: Store) {
    let shown = computed(move || Shown::of(s));
    effect(move || {
        let shown = shown.get();
        set_application_menu(untracked(|| application(s, shown)));
    });
}

/// The primary menu of Windows and Linux (and the terminal's F10): flat, in sections, as
/// GNOME's and Windows' primary menus are. The other commands are in the menus their
/// shortcuts belong to, and in the toolbar.
pub fn primary(s: Store) -> Vec<MenuItem> {
    let modal = s.modal();
    let term = crate::ui::kit::in_terminal();
    let mut items = vec![
        MenuItem::new(tr("menu.new_run"))
            .disabled(modal)
            .on_select(move || s.dialog.set(Some(Dialog::NewRun))),
    ];
    if !term {
        items.push(
            MenuItem::new(tr("menu.open_file"))
                .disabled(modal)
                .on_select(move || s.pick_run_file()),
        );
    } else {
        let running = s.running();
        let proving = s.proving();
        let sel = s.selection.get_untracked();
        let one = sel.len() == 1 && s.current.with_untracked(|c| c.is_some());
        let page = s.page.get_untracked();
        items.push(
            MenuItem::new(tr("menu.run_again"))
                .disabled(modal || running || !one)
                .on_select(move || s.run_again()),
        );
        items.push(
            MenuItem::new(tr("menu.stop"))
                .disabled(!running && !proving)
                .on_select(move || {
                    s.stop_run();
                    s.stop_prove();
                }),
        );
        items.push(
            MenuItem::new(tr("menu.prove"))
                .disabled(modal || proving || !one)
                .on_select(move || {
                    s.page.set(Page::Operator);
                    s.start_prove();
                }),
        );
        items.push(
            MenuItem::new(tr("menu.delete"))
                .disabled(modal || sel.is_empty())
                .on_select(move || s.ask_delete(sel.clone())),
        );
        items.push(MenuItem::separator());
        items.push(
            MenuItem::new(tr("nav.profile"))
                .checked(page == Page::Profile)
                .disabled(modal)
                .on_select(move || s.page.set(Page::Profile)),
        );
        items.push(
            MenuItem::new(tr("nav.operator"))
                .checked(page == Page::Operator)
                .disabled(modal)
                .on_select(move || s.page.set(Page::Operator)),
        );
    }
    items.push(MenuItem::separator());
    items.push(
        MenuItem::new(if s.sidebar.get() {
            tr("menu.hide_sidebar")
        } else {
            tr("menu.show_sidebar")
        })
        .disabled(modal)
        .on_select(move || s.sidebar.update(|v| *v = !*v)),
    );
    if !term {
        items.push(MenuItem::new(tr("menu.full_screen")).on_select(move || toggle_full_screen(s)));
    }
    items.push(MenuItem::separator());
    items.push(
        MenuItem::new(tr(settings_key()))
            .disabled(modal)
            .on_select(move || s.open_settings()),
    );
    items.extend(help_items());
    items.push(about_item(s, modal));
    // A terminal quits with Ctrl+C.
    if !term {
        items.push(MenuItem::separator());
        items.push(if windows() {
            MenuItem::new(tr("menu.exit")).on_select(move || s.request_quit())
        } else {
            MenuItem::new(tr("menu.quit_plain")).on_select(move || s.request_quit())
        });
    }
    items
}

/// Show the primary menu: under its button (`at`, its bottom-left corner in the window), else
/// at the pointer.
pub fn show_primary(s: Store, at: Option<Point<Points>>) {
    let items = untracked(|| primary(s));
    let _ = match at {
        Some(p) => popup_menu_at(items, p),
        None => popup_menu(items),
    };
}

/// The context menu of run `id` in the sidebar: it acts on the selection when `id` is in it,
/// else on `id` alone. The row it acts on is highlighted while the menu is open.
pub fn run_context(s: Store, id: &str) {
    let sel = s.selection.get_untracked();
    let ids = if sel.iter().any(|x| x == id) {
        sel
    } else {
        vec![id.to_owned()]
    };
    let one = ids.len() == 1;
    let reveal = ids[0].clone();
    let (open, cmp, prove, copy, cmd, del) = (
        ids.clone(),
        ids.clone(),
        ids[0].clone(),
        ids.join(" "),
        format!("oprobe show {}", ids.join(" ")),
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
        // The run's file, to hand a teammate (a run id means nothing to them).
        MenuItem::new(tr(reveal_key()))
            .disabled(!one || crate::ui::kit::in_terminal())
            .on_select(move || {
                let file = s.run_file(&reveal);
                s.task(async move {
                    let _ = reveal_path(file).await;
                });
            }),
        MenuItem::separator(),
        MenuItem::new(tr("menu.delete")).on_select(move || s.ask_delete(del.clone())),
    ];
    s.context_row.set(Some(id.to_owned()));
    let closed = popup_menu(items);
    s.task(async move {
        closed.await;
        s.context_row.set(None);
    });
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
