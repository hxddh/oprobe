//! The frame around the four views: the bar with the views, the running job and the language;
//! a notice line; keys `1`–`4` switch views (outside text fields).

use nsh_base::theme;
use nsh_core::prelude::*;

use crate::ui::fmt;
use crate::ui::i18n::{Lang, tr, trf};
use crate::ui::kit::{self, Kind, Tone, btn, ring, segmented, small, strong};
use crate::ui::store::{Page, Store, store};

pub fn shell() -> impl IntoElement {
    let s = store();
    install_keys(s);
    view()
        .flex_1()
        .min_h(px(0.0))
        .child(header(s))
        .child(notice(s))
        .child(dynamic(move || {
            let page = s.page.get();
            untracked(|| match page {
                Page::Runs => super::runs::runs().into_element(),
                Page::Profile => super::profile::profile().into_element(),
                Page::Operator => super::operator::operator().into_element(),
                Page::Compare => super::compare::compare().into_element(),
            })
        }))
}

fn install_keys(s: Store) {
    let opts = KeyOptions {
        when: nsh_core::keymap::When::NotEditing,
        ..Default::default()
    };
    for (k, p) in ["1", "2", "3", "4"].into_iter().zip(Page::ALL) {
        on_key_with(k, opts, move |_| s.page.set(p));
    }
}

fn nav(s: Store, p: Page) -> impl IntoElement {
    let on = move || s.page.get() == p;
    nsh_base::button()
        .selected(on)
        .on_press(move || s.page.set(p))
        .accessibility_label(tr(p.key()))
        .test_id(p.key())
        .px(px(10.0))
        .h(px(26.0))
        .justify_center()
        .rounded(px(6.0))
        .bg(move || {
            if on() {
                theme().accent
            } else {
                transparent_black().into()
            }
        })
        .hover(|st| st.bg(move || theme().secondary))
        .focus_visible(ring)
        .child(
            text(tr(p.key()))
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM)
                .number_of_lines(1),
        )
}

fn header(s: Store) -> impl IntoElement {
    let term = kit::in_terminal();
    view()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_2()
        .px_3()
        .py(px(if term { 0.0 } else { 8.0 }))
        .border_b_1()
        .border_color(move || theme().border)
        .bg(move || theme().color("toolbar").unwrap_or(theme().background))
        .child(strong("oprobe").pr_2())
        .children(Page::ALL.map(|p| nav(s, p)))
        .child(view().flex_1().min_w(px(8.0)))
        .child(dynamic(move || {
            let st = s.run_state.get().filter(|st| st.outcome.is_none());
            untracked(|| {
                st.map(|st| {
                    let what = if st.cleaning {
                        tr("phase.cleanup").to_owned()
                    } else {
                        format!(
                            "{} {}/{}",
                            tr("app.running"),
                            fmt::clock_s(st.elapsed_s),
                            fmt::clock_s(st.total_s)
                        )
                    };
                    view()
                        .flex_row()
                        .gap_1()
                        .items_center()
                        .child(text("●").text_size(px(11.0)).text_color(kit::token("ok")))
                        .child(small(what).tabular_nums().number_of_lines(1))
                })
            })
        }))
        .child(segmented(
            vec![
                (None, SharedString::from(tr("app.lang_auto"))),
                (Some(Lang::En), SharedString::from("English")),
                (Some(Lang::Zh), SharedString::from("中文")),
            ],
            move || s.lang_choice.get(),
            move |l| s.set_lang(l),
        ))
}

fn notice(s: Store) -> impl IntoElement {
    dynamic(move || {
        let n = s.notice.get();
        untracked(|| {
            n.map(|(msg, tone)| {
                view()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(move || theme().border)
                    .child(
                        text(msg)
                            .text_size(px(12.0))
                            .flex_1()
                            .min_w(px(0.0))
                            .selectable(true)
                            .text_color(move || match tone {
                                Tone::Neutral => theme().foreground,
                                t => t.color(),
                            }),
                    )
                    .child(btn(
                        tr("app.dismiss"),
                        || Kind::Ghost,
                        move || s.notice.set(None),
                    ))
            })
        })
    })
}

/// The frame of a view's content: scrolling and padded.
pub fn page(body: View) -> impl IntoElement {
    scroll().flex_1().min_h(px(0.0)).child(
        body.p(px(if kit::in_terminal() { 8.0 } else { 16.0 }))
            .gap_3()
            .min_w(px(0.0)),
    )
}

/// `{}` filled in a translated string.
pub fn fill(key: &'static str, args: &[&dyn std::fmt::Display]) -> String {
    trf(key, args)
}
