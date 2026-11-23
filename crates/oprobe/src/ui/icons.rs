//! The app's icons: a few of Lucide's (https://lucide.dev, ISC licence, © Lucide
//! Contributors), drawn as SVG in the text colour (`currentColor`) at 16 pt. In a terminal an
//! icon is a glyph.

use nsh_core::element::ImageSrc;
use nsh_core::prelude::*;

use crate::ui::kit::in_terminal;

/// An icon of the set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Sidebar,
    Plus,
    Play,
    Stop,
    Flask,
    More,
    Menu,
}

impl Icon {
    /// The shapes inside a 24 × 24 view box.
    fn shapes(self) -> &'static str {
        match self {
            Icon::Sidebar => {
                r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M9 3v18"/>"#
            }
            Icon::Plus => r#"<path d="M5 12h14"/><path d="M12 5v14"/>"#,
            Icon::Play => r#"<polygon points="6 3 20 12 6 21 6 3" fill="currentColor"/>"#,
            Icon::Stop => {
                r#"<rect width="14" height="14" x="5" y="5" rx="2" fill="currentColor"/>"#
            }
            Icon::Flask => {
                r#"<path d="M10 2v7.527a2 2 0 0 1-.211.896L4.72 20.55a1 1 0 0 0 .9 1.45h12.76a1 1 0 0 0 .9-1.45l-5.069-10.127A2 2 0 0 1 14 9.527V2"/><path d="M8.5 2h7"/><path d="M7 16h10"/>"#
            }
            Icon::More => {
                r#"<circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/><circle cx="5" cy="12" r="1"/>"#
            }
            Icon::Menu => r#"<path d="M4 6h16"/><path d="M4 12h16"/><path d="M4 18h16"/>"#,
        }
    }

    /// The glyph that stands for it on a terminal grid.
    fn glyph(self) -> &'static str {
        match self {
            Icon::Sidebar => "▥",
            Icon::Plus => "+",
            Icon::Play => "▶",
            Icon::Stop => "■",
            Icon::Flask => "⚗",
            Icon::More => "…",
            Icon::Menu => "☰",
        }
    }

    fn markup(self) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{}</svg>"#,
            self.shapes()
        )
    }
}

/// `icon` at `size` points, in the colour of `color` (the text colour around it if `None`).
pub fn icon(icon: Icon, size: f32, color: Option<fn() -> AdaptiveColor>) -> AnyElement {
    if in_terminal() {
        let t = text(icon.glyph()).text_size(px(size));
        return match color {
            Some(c) => t.text_color(c).into_element(),
            None => t.into_element(),
        };
    }
    let s = svg(ImageSrc::svg_markup(&icon.markup()))
        .w(px(size))
        .h(px(size))
        .flex_shrink_0();
    match color {
        Some(c) => s.text_color(c).into_element(),
        None => s.into_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_one_svg_in_the_text_colour() {
        for i in [
            Icon::Sidebar,
            Icon::Plus,
            Icon::Play,
            Icon::Stop,
            Icon::Flask,
            Icon::More,
            Icon::Menu,
        ] {
            let m = i.markup();
            assert!(m.starts_with("<svg") && m.ends_with("</svg>"), "{i:?}");
            assert!(m.contains("currentColor"));
            assert!(!i.glyph().is_empty());
        }
    }
}
