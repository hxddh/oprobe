//! The toolbar's icons in each platform's own set (design v5 §9, U11): SF Symbols on macOS,
//! Segoe Fluent Icons on Windows, Adwaita symbolic names on Linux; a Unicode glyph in a
//! terminal and wherever the native set cannot be drawn.

/// A toolbar icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Run,
    Stop,
    Rerun,
    Compare,
    Health,
    Tune,
    Probe,
    Export,
    Commands,
    Sidebar,
    Inspector,
}

/// An icon's names: (SF Symbol, Segoe Fluent code point, Adwaita name, fallback glyph).
pub fn names(i: Icon) -> (&'static str, char, &'static str, &'static str) {
    match i {
        Icon::Run => (
            "play.fill",
            '\u{E768}',
            "media-playback-start-symbolic",
            "▶",
        ),
        Icon::Stop => ("stop.fill", '\u{E71A}', "media-playback-stop-symbolic", "■"),
        Icon::Rerun => ("arrow.clockwise", '\u{E72C}', "view-refresh-symbolic", "↻"),
        Icon::Compare => (
            "arrow.left.arrow.right",
            '\u{E8AB}',
            "object-flip-horizontal-symbolic",
            "⇄",
        ),
        Icon::Health => (
            "heart.text.square",
            '\u{EB51}',
            "emblem-favorite-symbolic",
            "♥",
        ),
        Icon::Tune => (
            "slider.horizontal.3",
            '\u{E713}',
            "emblem-system-symbolic",
            "⚙",
        ),
        Icon::Probe => (
            "waveform.path.ecg",
            '\u{E9D9}',
            "utilities-system-monitor-symbolic",
            "⌁",
        ),
        Icon::Export => (
            "square.and.arrow.up",
            '\u{E74E}',
            "document-save-symbolic",
            "⇪",
        ),
        Icon::Commands => ("command", '\u{E756}', "view-list-symbolic", "≡"),
        Icon::Sidebar => ("sidebar.left", '\u{E90C}', "sidebar-show-symbolic", "◧"),
        Icon::Inspector => (
            "sidebar.right",
            '\u{E90D}',
            "sidebar-show-right-symbolic",
            "◨",
        ),
    }
}

/// The platform's icon font, when its icons are drawn as text: Segoe Fluent Icons on Windows
/// (Windows 11; Segoe MDL2 Assets carries the same code points on Windows 10).
pub const WINDOWS_ICON_FONT: &str = "Segoe Fluent Icons, Segoe MDL2 Assets";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_a_name_in_every_set() {
        for i in [
            Icon::Run,
            Icon::Stop,
            Icon::Rerun,
            Icon::Compare,
            Icon::Health,
            Icon::Tune,
            Icon::Probe,
            Icon::Export,
            Icon::Commands,
            Icon::Sidebar,
            Icon::Inspector,
        ] {
            let (sf, seg, adw, glyph) = names(i);
            assert!(!sf.is_empty() && adw.ends_with("-symbolic") && !glyph.is_empty());
            assert!(
                ('\u{E000}'..='\u{F8FF}').contains(&seg),
                "{i:?}: private use"
            );
        }
    }
}
