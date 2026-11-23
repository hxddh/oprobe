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
  - the sidebar's width is fixed (240 pt), and ⌃⌘S / Ctrl+Shift+S or the toolbar button hides it.
- **Selection**:
  - a click selects one run and shows it;
  - ⌘-click / Ctrl-click adds or removes a run, and ⇧-click extends the selection;
  - two or more selected runs show Compare. There is no separate Compare page and no "pick" buttons.
- **Detail**: Profile or Operator for one run, chosen by a segmented control in the toolbar (⌘1, ⌘2).
- **Toolbar** (in the title bar: the system's title bar is hidden and its controls stay where the
  platform puts them):
  - leading: the sidebar toggle, then the title (the run id) and subtitle (operator · preset · window);
  - trailing: the Profile/Operator control, **New Run** (＋), and **Run** ▶ / **Stop** ■ while a
    job runs (with its progress);
  - the whole bar is a drag region: a double press zooms or maximizes as the platform does.

## Menus and keys

The shortcuts use the platform's modifier (`mod`: ⌘ on macOS, Ctrl elsewhere).

| Menu | Items |
|---|---|
| oprobe (macOS) | About, Settings… (mod+,), Hide, Hide Others, Show All, Quit |
| File | New Run… (mod+N), Open Run File… (mod+O), Close Window (mod+W) |
| Edit | the platform's editing roles; Copy Run ID (mod+shift+C) |
| Run | Run Again (mod+R), Stop (mod+.), Prove… (mod+shift+P), Compare Selected, Delete Run… (mod+⌫) |
| View | Profile (mod+1), Operator (mod+2), Show/Hide Sidebar, Enter Full Screen |
| Window | Minimize, Zoom (the platform's roles) |
| Help | oprobe Documentation |

- **macOS**: the system's menu bar.
- **Windows and Linux**: the window draws no menu bar under a hidden title bar. The same menus are
  in a **primary menu** button at the trailing end of the toolbar (⋯ on Windows, ☰ on GNOME). Every
  shortcut works whether or not a menu is open.
- **Context menus** (secondary click on a run row) offer: Open, Compare With Selected, Prove…,
  Copy Run ID, Copy Command (`oprobe show ID`), Delete…. The selected rows act together.
- **Delete** asks first (a dialog naming the runs; Delete is the destructive button, and Cancel is
  the default).

## Windows and dialogs

- **New Run** is a dialog: the operator (with the lab scenarios as quick picks), the preset, and the
  duration and concurrency (advanced, collapsed). Return starts the run and Escape cancels.
- **Settings** (mod+,) is a dialog: the language (System, English, 简体中文) and where runs are
  stored. Nothing about settings stays in the main window.
- **Open Run File…** uses the system's file dialog (`.oprobe`, `.json` traces). Dropping such a file
  on the window opens it.
- A running job shows its progress on the Dock icon or taskbar button.

## Look

- **Type**: the system font. Body 13 pt, secondary 11 pt, section headings 13 pt semibold in
  sentence case (no ALL CAPS, no letter-spacing tricks), and the run title 15 pt semibold. Numbers
  are tabular.
- **Spacing**: a 4 pt grid. Content insets are 20 pt (desktop) and 1 cell (terminal). Sections are
  separated by space and a hairline, not by boxes; at most one level of grouping (a grouped inset
  surface for forms).
- **Color**: one accent (the theme's primary), and semantic colors only for meaning (ok, warning,
  critical, inconclusive). Verdicts are a small filled dot plus a word, not an outlined pill.
- **Controls**: push buttons have the platform's height (22/24 pt on macOS, 32 pt on Windows,
  34 pt on GNOME, approximated by one density per OS). A default button is filled with the accent;
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
