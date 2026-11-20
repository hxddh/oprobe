//! The app shell: a sidebar of pages and the selected page.

use nsh_base::{button, theme};
use nsh_core::prelude::*;

use crate::ui::pages::{probe::probe_page, tune::tune_page, workbench::workbench};
use crate::ui::store::{Page, store};
use crate::ui::widgets::{muted, stage_color};

fn nav_item(page: Page, label: &'static str, hint: &'static str) -> impl IntoElement {
    let s = store();
    let selected = move || s.page.get() == page;
    button()
        .selected(selected)
        .on_press(move || s.page.set(page))
        .accessibility_label(label)
        .px_3()
        .py_2()
        .gap_0p5()
        .rounded(move || px(theme().radius.0))
        .bg(move || {
            if selected() {
                theme().sidebar_accent
            } else {
                theme().sidebar
            }
        })
        .hover(|st| st.opacity(0.85))
        .focus_visible(|st| st.outline(px(2.0)).outline_color(move || theme().ring))
        .child(text(label).text_sm().font_weight(FontWeight::MEDIUM))
        .child(muted(hint))
}

fn sidebar() -> impl IntoElement {
    let s = store();
    view()
        .w(px(210.0))
        .flex_shrink_0()
        .p_3()
        .gap_1()
        .bg(move || theme().sidebar)
        .border_r_1()
        .border_color(move || theme().sidebar_border)
        .child(
            view()
                .px_3()
                .py_3()
                .gap_0p5()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            view()
                                .w(px(10.0))
                                .h(px(10.0))
                                .rounded_full()
                                .bg(stage_color("server")),
                        )
                        .child(text("oprobe").text_lg().font_weight(FontWeight::BOLD)),
                )
                .child(muted(concat!(
                    "storage microscope · v",
                    env!("CARGO_PKG_VERSION")
                ))),
        )
        .child(nav_item(Page::Workbench, "Workbench", "run & diagnose"))
        .child(nav_item(Page::Tune, "Tune", "find the best settings"))
        .child(nav_item(Page::Probe, "Probe", "handshakes & TTFB"))
        .child(view().flex_1())
        .child(muted(move || format!("● {}", s.status.get().label())).px_3())
        .child(dynamic(|| {
            if nsh_base::terminal::is_terminal() {
                muted("Ctrl+C quit · Tab moves").px_3().into_element()
            } else {
                view().into_element()
            }
        }))
}

pub fn shell() -> impl IntoElement {
    let s = store();
    view()
        .flex_1()
        .flex_row()
        .child(sidebar())
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .child(dynamic(move || match s.page.get() {
                    Page::Workbench => untracked(|| workbench().into_element()),
                    Page::Tune => untracked(|| tune_page().into_element()),
                    Page::Probe => untracked(|| probe_page().into_element()),
                })),
        )
}
