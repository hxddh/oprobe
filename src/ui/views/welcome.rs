//! First run (no runs in the store): three ways in — the demo, your own bucket (with the
//! credentials this machine already has), or a run file.

use nsh_base::theme;
use nsh_core::prelude::*;

use crate::ui::kit::{Kind, body, btn, caption, panel, row_button, small, strong, title};
use crate::ui::model::cloud::detect;
use crate::ui::store::{Overlay, store};

fn path_card(heading: &'static str, detail: &'static str) -> View {
    panel()
        .flex_1()
        .min_w(px(200.0))
        .p_4()
        .gap_2()
        .child(strong(heading))
        .child(small(detail))
}

pub fn welcome() -> impl IntoElement {
    let s = store();
    let found = detect();
    scroll().flex_1().child(
        view()
            .p_6()
            .gap_5()
            .max_w(px(860.0))
            .child(
                view()
                    .gap_1()
                    .child(title("See every request your storage client makes"))
                    .child(body("oprobe runs a realistic workload through Apache OpenDAL and traces each request — queue, DNS, TCP, TLS, time to first byte, transfer — then tells you where the time and the money go, and what to change.").text_color(move || theme().muted_foreground)),
            )
            .child(
                view()
                    .flex_row()
                    .flex_wrap()
                    .gap_3()
                    .child(
                        path_card("Try the demo", "A local lab server with throttling and slow stragglers. No account, no credentials, about ten seconds.")
                            .child(view().flex_1())
                            .child(btn("Try the Demo", || Kind::Primary, move || s.demo()).self_start()),
                    )
                    .child(
                        path_card("Connect my bucket", "S3, GCS, Azure Blob and every OpenDAL service. oprobe writes only under its own run prefix and removes it afterwards.")
                            .child(view().flex_1())
                            .child(btn("Connect…", || Kind::Plain, move || s.open(Overlay::NewRun)).self_start()),
                    )
                    .child(
                        path_card("Open a run file", "A .oprobe file from `oprobe run` on a server or in CI, or one a colleague sent. You can also drop it on this window.")
                            .child(view().flex_1())
                            .child(btn("Open…", || Kind::Plain, move || crate::ui::files::open(s)).self_start()),
                    ),
            )
            .child((!found.is_empty()).then(|| {
                view()
                    .gap_1()
                    .child(caption("CREDENTIALS FOUND ON THIS MACHINE"))
                    .children(found.into_iter().map(move |d| {
                        let label = d.label.clone();
                        let scheme = d.scheme;
                        let region = d.region.clone();
                        row_button(label.clone(), || false, move || {
                            batch(|| {
                                s.form.target.set(scheme.to_owned());
                                if let Some(r) = &region {
                                    s.form.region.set(r.clone());
                                }
                                s.form.advanced.set(region.is_some());
                            });
                            s.open(Overlay::NewRun);
                        })
                        .flex_row()
                        .gap_2()
                        .child(body(label))
                        .child(small(match (&d.region, &d.credentials) {
                            (Some(r), _) => format!("{} · {r}", d.scheme),
                            _ => d.scheme.to_owned(),
                        }))
                    }))
            }))
            .child(small("Shortcuts: ⌘N new run · ⌘O open · ⌘K commands (Ctrl on Windows and Linux).")),
    )
}
