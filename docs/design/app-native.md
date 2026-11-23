# The app, native (design v6 §4, amended)

The 6.0 app drew everything itself: a row of tabs across the top, the language in the window, bordered
cards, ALL-CAPS headings, buttons of its own. It worked, but it looked like a web page in a window.
This document is the bar the app is held to: it follows the platform's conventions wherever nsh gives
us the native part, and keeps a quiet, consistent look of its own where it does not.

## Structure: a sidebar of runs and a detail

oprobe is a document-style professional tool (Instruments, Xcode, Console). The runs are the documents.

```
┌──────────────┬──────────────────────────────────────────────────────────┐
│ ● ● ●  ▥     │  20261122-235341-e93f          [Profile|Operator]  ＋ ▶ │  ← one bar: title bar + toolbar
│              │  lab://throttle · small-files · 8 s                       │
│ RUNNING      ├──────────────────────────────────────────────────────────┤
│  ◐ lab://…   │                                                            │
│ LAB://THROT… │   detail of the selection                                  │
│  ● e93f  …   │   (one run: Profile or Operator; two or more: Compare)     │
│  ● c272  …   │                                                            │
│ LAB://DEMO   │                                                            │
│  ● 91aa  …   │                                                            │
└──────────────┴──────────────────────────────────────────────────────────┘
```

- **Sidebar** (a source list on the window's sidebar material: vibrancy on macOS, Mica on Windows 11):
  - the runs, newest first, grouped under their operator;
  - each row has a verdict dot, the short id, the preset and the time, and its goodput and p99 at
    the trailing edge;
  - a running job is a row at the top with its progress;
  - it is one listbox for the keyboard and screen readers (one Tab stop; rows are options with a
    selected state): ↑/↓ move, Shift+↑/↓ extend, Home/End, type-select (short id, operator,
    preset), Return opens, Delete (⌘⌫ on macOS) asks to delete;
  - the selection follows the platform: macOS fills it with the accent while the list has the
    focus in the key window and with grey otherwise; Windows and GNOME use a subtle fill with an
    accent pill at the leading edge; a secondary click outlines the row the menu acts on;
  - its width follows a drag of its trailing edge (200–420 pt, 248 by default) and is
    remembered, and ⌃⌘S / Ctrl+Shift+S or the toolbar button hides it;
  - only the detail column paints the window's background: the sidebar is transparent over the
    material (macOS `Backdrop::Sidebar`, Windows 11 `Backdrop::Window`, Mica) and paints the
    theme's `sidebar` colour where there is none (Windows 10, Linux, a terminal).
- **Selection**:
  - a click selects one run and shows it;
  - ⌘-click / Ctrl-click adds or removes a run, and ⇧-click extends the selection;
  - two or more selected runs show Compare. There is no separate Compare page and no "pick" buttons.
- **Detail**: Profile or Operator for one run, chosen by a segmented control in the toolbar (⌘1, ⌘2).
- **Toolbar** (in the title bar: the system's title bar is hidden and its controls stay where the
  platform puts them; 52 pt on macOS, 48 px on Windows, 46 px on GNOME, or the hidden title
  bar's own height):
  - leading: the sidebar toggle, then the title (the run id) and subtitle (operator · preset · window);
  - trailing: the Profile/Operator control, **New Run** (＋), and **Run** ▶ / **Stop** ■ while a
    job runs (with its progress);
  - the whole bar is a drag region: a double press zooms or maximizes as the platform does (the
    title is not selectable, so it does not take the press).
- **State**: the window's placement, the sidebar's visibility and width, and the page are
  restored at the next start.

## Menus and keys

The shortcuts use the platform's modifier (`mod`: ⌘ on macOS, Ctrl elsewhere).

| Menu | Items |
|---|---|
| oprobe (macOS) | About oprobe, Settings… (mod+,), Hide, Hide Others, Show All, Quit oprobe |
| File | New Run… (mod+N), Open Run File… (mod+O), Close Window (mod+W); off macOS: Settings / Preferences (mod+,), Quit (Ctrl+Q, Linux) or Exit (Windows) |
| Edit | the platform's editing roles; Copy Run ID (mod+shift+C) |
| Run | Run Again (mod+R), Stop (mod+.), Prove… (mod+shift+P), Compare Selected, Delete Run… (⌘⌫ on macOS, Delete elsewhere) |
| View | Profile (mod+1), Operator (mod+2), Show/Hide Sidebar, Full Screen (the system's on macOS, F11 elsewhere) |
| Window | Minimize, Zoom (the platform's roles, macOS) |
| Help | oprobe Documentation; About oprobe off macOS |

- **macOS**: the system's menu bar. Role items (About, Hide, Quit, Undo, …) carry the app's
  labels (`MenuItem::role(..).label(tr(..))`), so a Chinese app on an English Mac has Chinese
  menus.
- **Windows and Linux**: the window draws no menu bar under a hidden title bar; the menus'
  shortcuts work all the same. The toolbar's **primary menu** (⋯ on Windows, ☰ on GNOME) is
  flat, in sections, and opens under its button (`popup_menu_at` with the button's frame): New
  Run…, Open Run File… | Show/Hide Sidebar, Full Screen | Preferences (Settings on Windows),
  Documentation, About oprobe | Quit (Exit on Windows).
- Shortcuts a text field also uses (Delete, ⌘⌫, mod+shift+C) are `When::NotEditing`, and the
  app's menu actions are disabled while a dialog or an alert is open.
- **Context menus** (secondary click on a run row) offer: Open, Compare With Selected, Prove…,
  Copy Run ID, Copy Command (`oprobe show ID`), Delete…. The selected rows act together.
- **Delete** asks with the system's alert (`nsh_base::alert_dialog`: NSAlert, TaskDialog,
  AdwAlertDialog): Delete is destructive and Cancel the default. The runs move to oprobe's trash
  (`App::trash`, `<data>/trash/`); the notice bar offers **Undo** (`App::restore`). When the
  notice goes, the folder is handed to the system's trash (`move_to_trash`) where there is one;
  oprobe empties its own after a week. A terminal asks with a dialog.
- **Quit** (⌘Q, Ctrl+Q, Exit) and the main window's close button ask first while a run or a
  prove goes on ("Stop and Quit" / Cancel): the app listens to quit requests, and the main
  window does not close on its own while a job runs.

## Windows and dialogs

- **New Run** is a dialog: the operator field (focused when it opens) with a **Choose** pop-up
  (the lab's scenarios, the config file's `[operators.NAME]`, recent `op.toml` files, Choose
  op.toml…), the workload as a pop-up (`nsh_base::select`), and the duration and concurrency.
  Return anywhere presses the default button and Escape cancels. A real target plans first
  (`App::plan`): the dialog shows what the run writes, reads and deletes, under which prefix, its
  ETA and its budgets, and only then offers Run.
- **Settings** (mod+,): the language (System, English, 简体中文), where runs are stored and the
  config file. On macOS it is a window of its own (`window().id("settings")`, not modal, no
  Done: every change applies at once); on Windows and GNOME a preferences dialog with a close
  button; on a terminal a dialog.
- **About oprobe**: the version and links (the system's About panel on macOS).
- **Open Run File…** uses the system's open dialog (`pick_paths`, filtered to `.oprobe` and
  `.json`) and opens the file where it is. Files opened from Finder or Explorer
  (`next_open_files`) and files dropped on the window open too; the window is outlined while a
  file is dragged over it.
- A running job shows its progress on the Dock icon or taskbar button; when it ends with the
  window in the background, a notification says so and the Dock or taskbar asks for attention.
- Dialogs fade in unless reduced motion is on.

## Look

- **Type**: the system font. Body 13 pt, secondary 11 pt, section headings 13 pt semibold in
  sentence case (no ALL CAPS, no letter-spacing tricks), and the run title 15 pt semibold. Numbers
  are tabular.
- **Spacing**: a 4 pt grid. Content insets are 20 pt (desktop) and 1 cell (terminal). Sections are
  separated by space and a hairline, not by boxes; at most one level of grouping (a grouped inset
  surface for forms).
- **Color**: one accent (the theme's primary), and semantic colors only for meaning (ok, warning,
  critical, inconclusive). Text on the accent is at least 4.5:1 in both modes, field and
  switch borders at least 3:1 against the background, and the semantic colors read as text at
  4.5:1. Verdicts are a small filled dot plus a word, not an outlined pill.
- **Controls**: push buttons have the platform's height (22/24 pt on macOS, 32 pt on Windows,
  34 pt on GNOME, approximated by one density per OS) as a minimum that grows with the user's
  text size (`font_scale()`), so text never clips. A default button is filled with the accent;
  toolbar buttons are borderless icons with tooltips. Switches only for on/off settings; for a layer
  in the stack, a checkbox-style switch is fine.
- **Icons**: one small set (Lucide, ISC), 16 pt, tinted with the text color.
- **Motion**: none beyond the platform's own (sheets, popovers); respect reduced motion.

## Terminal

The same structure runs on the terminal grid: the sidebar is the left column, the toolbar is the
first row, menus are the primary menu (F10), dialogs are `Modal`s, and there is no material. The
keys are the same, with Ctrl.

## Not in scope

Multiple windows per run, tabs, custom themes, and a toolbar the user can customize.
