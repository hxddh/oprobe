//! Requests: every request of the run in a virtualized table (100k+ rows: the table holds a
//! page of formatted rows around the visible ones), filter box, sorting, a context menu.

use nsh_base::{
    DataTableClasses, DataTableColumn, DataTableContextMenu, DataTablePress, DataTableRange,
    DataTableRows, DataTableSelection, DataTableSort, DataTableSortDirection, data_table, theme,
};
use nsh_core::element::desktop::{MenuItem, popup_menu};
use nsh_core::prelude::*;

use crate::ui::kit::{Kind, btn, empty, field, small};
use crate::ui::model::requests::{Col, Filter, to_csv};
use crate::ui::model::{Tab, fmt};
use crate::ui::store::{Inspect, Store, store};

/// Rows the table holds at once (formatted): what is in view and a reach around it.
pub const PAGE: u32 = 300;

pub fn columns() -> Vec<DataTableColumn> {
    Col::ALL
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut col = DataTableColumn::default();
            col.key = c.key().into();
            col.title = Some(c.title().into());
            col.width = Some(px(c.width()));
            col.sortable = true;
            if c.numeric() {
                col.align = nsh_base::DataTableAlign::End;
            }
            if i == 0 {
                col.fixed = true.into();
            }
            col
        })
        .collect()
}

fn page_rows(s: Store, start: u32) -> DataTableRows {
    s.current.with(|c| {
        let Some(v) = c else {
            return DataTableRows::new(0);
        };
        s.req_rows.with(|rows| {
            let idx = &rows.0;
            let end = (start as usize + PAGE as usize).min(idx.len());
            let start = (start as usize).min(end);
            let slice = &idx[start..end];
            let mut out = DataTableRows::new(slice.len());
            for col in Col::ALL {
                out = out.texts(
                    col.key(),
                    slice
                        .iter()
                        .map(|i| col.cell(&v.run.reqs[*i as usize]))
                        .collect::<Vec<_>>(),
                );
            }
            out
        })
    })
}

fn copy_row_json(s: Store, i: u32) {
    let json = s.current.with_untracked(|c| {
        c.as_ref()
            .and_then(|c| c.run.reqs.get(i as usize))
            .and_then(|r| serde_json::to_string_pretty(r).ok())
    });
    if let Some(j) = json {
        set_clipboard(j);
        s.toast("Copied the request as JSON", crate::ui::kit::Tone::Neutral);
    }
}

fn add_filter(s: Store, token: String) {
    s.req_query.update(|q| {
        if !q.split_whitespace().any(|t| t == token) {
            if !q.trim().is_empty() {
                q.push(' ');
            }
            q.push_str(&token);
        }
    });
}

/// Show the request's operation in the timeline.
pub fn reveal_in_timeline(s: Store, i: u32) {
    let hit = s.current.with_untracked(|c| {
        let c = c.as_ref()?;
        let r = c.run.reqs.get(i as usize)?;
        let op = c.op_index.get(&r.op_id).copied();
        let t = r.t_us as f64 / 1e6;
        let span = (r.total_us as f64 / 1e6 * 6.0).max(0.05);
        Some((op, (t - span, t + span)))
    });
    if let Some((op, win)) = hit {
        batch(|| {
            s.tl_window.set(Some(win));
            if let Some(op) = op {
                s.inspect.set(Inspect::Op(op));
            }
            s.set_tab(Tab::Timeline);
        });
    }
}

fn context_menu(s: Store, row: u32) {
    let Some(i) = s
        .req_rows
        .with_untracked(|r| r.0.get(row as usize).copied())
    else {
        return;
    };
    let Some((api, status, remote, conn)) = s.current.with_untracked(|c| {
        c.as_ref()
            .and_then(|c| c.run.reqs.get(i as usize))
            .map(|r| {
                (
                    r.api.to_string(),
                    r.status,
                    r.remote.map(|a| a.ip().to_string()),
                    r.conn_id,
                )
            })
    }) else {
        return;
    };
    s.inspect.set(Inspect::Request(i));
    let mut items = vec![
        MenuItem::new("Copy Row as JSON").on_select(move || copy_row_json(s, i)),
        MenuItem::separator(),
        MenuItem::new(format!("Filter: API {api}")).on_select({
            let api = api.clone();
            move || add_filter(s, format!("api:{api}"))
        }),
        MenuItem::new(format!("Filter: Status {status}"))
            .on_select(move || add_filter(s, format!("status:{status}"))),
        MenuItem::new(format!("Filter: Connection #{conn}"))
            .on_select(move || add_filter(s, format!("conn:{conn}"))),
    ];
    if let Some(ip) = remote {
        items.push(
            MenuItem::new(format!("Filter: Remote {ip}"))
                .on_select(move || add_filter(s, format!("remote:{ip}"))),
        );
    }
    items.push(MenuItem::separator());
    items.push(MenuItem::new("Reveal in Timeline").on_select(move || reveal_in_timeline(s, i)));
    let _ = popup_menu(items);
}

/// Save the filtered rows as CSV.
pub fn export_csv(s: Store) {
    let data = s.current.with_untracked(|c| {
        c.as_ref().map(|c| {
            let rows = s.req_rows.with_untracked(|r| r.0.clone());
            (to_csv(&c.run.reqs, &rows), c.id().to_owned())
        })
    });
    if let Some((csv, id)) = data {
        crate::ui::files::save(s, format!("oprobe-{id}-requests.csv"), csv.into_bytes());
    }
}

pub fn requests() -> impl IntoElement {
    let s = store();
    let start = signal(0u32);
    let table = component_ref::<nsh_base::DataTable>();
    // A new filter or sort starts at the top.
    let first = stored(true);
    effect(move || {
        let _ = s.req_rows.get();
        if first.replace(false) {
            return;
        }
        untracked(|| {
            start.set(0);
            s.req_focus.set(None);
        });
    });
    // The focused row (a press, the arrows, j/k in a terminal): inspect it, keep it in view.
    effect(move || {
        let Some(row) = s.req_focus.get() else { return };
        untracked(|| {
            if let Some(i) = s.req_rows.with(|r| r.0.get(row as usize).copied()) {
                s.inspect.set(Inspect::Request(i));
            }
            if let Some(api) = table.api::<nsh_base::DataTableApi>() {
                api.scroll_to_row(row, Some(false));
            }
        });
    });
    let total = move || {
        s.current
            .with(|c| c.as_ref().map(|c| c.run.reqs.len()).unwrap_or(0))
    };
    let shown = move || s.req_rows.with(|r| r.0.len());
    let invalid = move || {
        let f = Filter::parse(&s.req_query.get());
        (!f.invalid.is_empty()).then(|| format!("not understood: {}", f.invalid.join(" ")))
    };
    let sort = move || {
        s.req_sort.get().map(|(c, desc)| {
            let mut so = DataTableSort::default();
            so.key = c.key().into();
            so.direction = if desc {
                DataTableSortDirection::Descending
            } else {
                DataTableSortDirection::Ascending
            };
            so
        })
    };
    view()
        .flex_1()
        .min_w(px(0.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .child(
                    field(
                        "Filter requests",
                        "api:GetObject status:5xx >100ms remote:10.0. conn:3 error retry",
                        s.req_query,
                    )
                    .flex_1()
                    .min_w(px(120.0))
                    .element_ref(&s.filter_ref),
                )
                .child(
                    small(move || {
                        invalid().unwrap_or_else(|| {
                            format!("{} of {}", fmt::compact(shown() as f64), fmt::compact(total() as f64))
                        })
                    })
                    .tabular_nums()
                    .number_of_lines(1),
                )
                .child(
                    small(move || {
                        s.req_sort
                            .get()
                            .map(|(c, desc)| format!("sorted by {} {}", c.title(), if desc { "↓" } else { "↑" }))
                            .unwrap_or_default()
                    })
                    .number_of_lines(1),
                )
                .child(
                    view()
                        .tooltip(crate::ui::kit::tip("What is different about the slowest requests"))
                        .child(btn("Slowest 1 %", || Kind::Plain, move || s.explain_slowest())),
                )
                .child(btn("Export CSV", || Kind::Plain, move || export_csv(s))),
        )
        .child(dynamic(move || {
            let has = s.current.with(|c| c.as_ref().is_some_and(|c| !c.run.reqs.is_empty()));
            untracked(|| {
                if !has {
                    return empty(
                        "No request rows",
                        "This run kept no per-request trace (the tuner's runs keep only aggregates).",
                    )
                    .into_element();
                }
                data_table()
                    .columns(columns())
                    .data(move || page_rows(s, start.get()))
                    .data_start(start)
                    .row_count(move || Some(shown() as u32))
                    .sort(sort)
                    .row_height(px(22.0))
                    .header_height(px(24.0))
                    .font_size(px(12.0))
                    .stripe(true)
                    .column_selectable(false)
                    .selection(move || {
                        s.req_focus.get().map(|row| {
                            let mut sel = DataTableSelection::default();
                            sel.row = Some(row);
                            sel
                        })
                    })
                    .accessibility_label("Requests")
                    .test_id("requests-table")
                    .classes({
                        let mut c = DataTableClasses::default();
                        c.header = Some("bg-secondary border-b border-border".into());
                        c.header_cell = Some("px-2".into());
                        c.cell = Some("px-2".into());
                        c.row_odd = Some("bg-muted".into());
                        c.row_selected = Some("bg-accent".into());
                        c.row_hover = Some("bg-secondary".into());
                        c.fixed_edge = Some("bg-border".into());
                        c.resize_handle = Some("bg-border".into());
                        c
                    })
                    .component_ref(&table)
                    .on_visible_rows_change(move |r: &DataTableRange| {
                        let cur = start.get_untracked();
                        if (r.start < cur.saturating_add(20) && cur > 0) || r.end + 20 > cur + PAGE {
                            start.set(r.start.saturating_sub(PAGE / 3));
                        }
                    })
                    .on_row_press(move |p: &DataTablePress| s.req_focus.set(Some(p.row)))
                    .on_selection_change(move |sel: &Option<DataTableSelection>| {
                        if let Some(row) = sel.as_ref().and_then(|s| s.row) {
                            s.req_focus.set(Some(row));
                        }
                    })
                    // A title's press sorts by it: ascending, descending, then off.
                    .on_column_press(move |p: &nsh_base::DataTableColumnPress| {
                        let Some(col) = Col::ALL.get(p.column as usize).copied() else { return };
                        s.req_sort.update(|so| {
                            *so = match *so {
                                Some((c, false)) if c == col => Some((col, true)),
                                Some((c, true)) if c == col => None,
                                _ => Some((col, false)),
                            }
                        });
                    })
                    .on_sort_change(move |sort: &Option<DataTableSort>| {
                        s.req_sort.set(sort.as_ref().and_then(|so| {
                            Col::from_key(&so.key)
                                .map(|c| (c, so.direction == DataTableSortDirection::Descending))
                        }));
                    })
                    .on_context_menu(move |m: &DataTableContextMenu| {
                        if let Some(row) = m.row {
                            context_menu(s, row)
                        }
                    })
                    .flex_1()
                    .border_t_1()
                    .border_color(move || theme().border)
                    .into_element()
            })
        }))
}
