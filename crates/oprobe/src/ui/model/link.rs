//! Saved views (design v5 §9): the tab, the Requests filter and sort, the timeline's zoom, the
//! brush and the selected operation encode into an `oprobe://run/ID?view=…` link — copied from
//! the app, written into exports, opened again with `oprobe gui 'oprobe://run/…'` or a click.
//!
//! The `view` parameter is itself a small query string (`tab=requests&q=status:5xx&op=42`), so
//! the link stays one parameter that tools can carry around untouched.

use crate::ui::model::Tab;

/// What a view link restores.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewState {
    pub run_id: String,
    pub tab: Option<Tab>,
    /// The Requests filter.
    pub query: String,
    /// Column key and descending.
    pub sort: Option<(String, bool)>,
    /// The timeline's window, seconds.
    pub window_s: Option<(f64, f64)>,
    /// A heatmap brush: from, to (s) and the latency floor (ms).
    pub brush: Option<(f64, f64, f64)>,
    /// The operation whose waterfall is open.
    pub op_id: Option<u64>,
}

const SCHEME: &str = "oprobe";

fn tab_key(t: Tab) -> &'static str {
    match t {
        Tab::Overview => "overview",
        Tab::Requests => "requests",
        Tab::Timeline => "timeline",
        Tab::Connections => "connections",
        Tab::Findings => "findings",
        Tab::Cost => "cost",
    }
}

fn tab_of(s: &str) -> Option<Tab> {
    Tab::ALL.into_iter().find(|t| tab_key(*t) == s)
}

fn num(v: f64) -> String {
    // Short and exact enough for a window in seconds or a latency in ms.
    let s = format!("{v:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

impl ViewState {
    /// The link: `oprobe://run/ID?view=tab%3Drequests%26q%3D…`.
    pub fn to_link(&self) -> String {
        let mut inner = url::form_urlencoded::Serializer::new(String::new());
        if let Some(t) = self.tab {
            inner.append_pair("tab", tab_key(t));
        }
        if !self.query.trim().is_empty() {
            inner.append_pair("q", self.query.trim());
        }
        if let Some((col, desc)) = &self.sort {
            inner.append_pair(
                "sort",
                &format!("{col}:{}", if *desc { "desc" } else { "asc" }),
            );
        }
        if let Some((a, b)) = self.window_s {
            inner.append_pair("win", &format!("{},{}", num(a), num(b)));
        }
        if let Some((a, b, ms)) = self.brush {
            inner.append_pair("brush", &format!("{},{},{}", num(a), num(b), num(ms)));
        }
        if let Some(op) = self.op_id {
            inner.append_pair("op", &op.to_string());
        }
        let view = inner.finish();
        let mut out = url::form_urlencoded::Serializer::new(String::new());
        let id: String =
            url::form_urlencoded::byte_serialize(self.run_id.as_bytes()).collect::<String>();
        if view.is_empty() {
            return format!("{SCHEME}://run/{id}");
        }
        out.append_pair("view", &view);
        format!("{SCHEME}://run/{id}?{}", out.finish())
    }

    /// Read a link back; `None` when it is not an `oprobe://run/…` link. Unknown or malformed
    /// parts are ignored (a link from a newer version still opens the run).
    pub fn parse(link: &str) -> Option<ViewState> {
        let u = url::Url::parse(link.trim()).ok()?;
        if u.scheme() != SCHEME || u.host_str() != Some("run") {
            return None;
        }
        let id = u.path().trim_start_matches('/');
        let id = url::form_urlencoded::parse(format!("x={id}").as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        if id.is_empty() {
            return None;
        }
        let mut v = ViewState {
            run_id: id,
            ..Default::default()
        };
        let Some(view) = u
            .query_pairs()
            .find(|(k, _)| k == "view")
            .map(|(_, v)| v.into_owned())
        else {
            return Some(v);
        };
        let floats = |s: &str| -> Vec<f64> {
            s.split(',')
                .filter_map(|x| x.trim().parse::<f64>().ok())
                .filter(|x| x.is_finite())
                .collect()
        };
        for (k, val) in url::form_urlencoded::parse(view.as_bytes()) {
            match k.as_ref() {
                "tab" => v.tab = tab_of(&val),
                "q" => v.query = val.into_owned(),
                "sort" => {
                    if let Some((c, d)) = val.split_once(':') {
                        v.sort = Some((c.to_owned(), d == "desc"));
                    }
                }
                "win" => {
                    if let [a, b] = floats(&val)[..]
                        && a < b
                    {
                        v.window_s = Some((a, b));
                    }
                }
                "brush" => {
                    if let [a, b, ms] = floats(&val)[..]
                        && a < b
                    {
                        v.brush = Some((a, b, ms));
                    }
                }
                "op" => v.op_id = val.parse().ok(),
                _ => {}
            }
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_round_trips_through_its_link() {
        let v = ViewState {
            run_id: "20260101-120000-ab12".into(),
            tab: Some(Tab::Requests),
            query: "status:5xx api:GetObject".into(),
            sort: Some(("total".into(), true)),
            window_s: Some((1.25, 3.5)),
            brush: Some((1.0, 2.0, 50.0)),
            op_id: Some(42),
        };
        let link = v.to_link();
        assert!(
            link.starts_with("oprobe://run/20260101-120000-ab12?view="),
            "{link}"
        );
        assert!(!link.contains(' '), "{link}");
        assert_eq!(ViewState::parse(&link), Some(v));
    }

    #[test]
    fn a_bare_or_odd_link_still_opens_the_run() {
        let v = ViewState::parse("oprobe://run/abc").unwrap();
        assert_eq!(v.run_id, "abc");
        assert_eq!(v.tab, None);
        let v =
            ViewState::parse("oprobe://run/abc?view=tab%3Dnope%26win%3D5%2C1%26zz%3D1").unwrap();
        assert_eq!((v.tab, v.window_s), (None, None));
        assert!(ViewState::parse("https://run/abc").is_none());
        assert!(ViewState::parse("oprobe://run/").is_none());
        assert_eq!(
            ViewState {
                run_id: "x".into(),
                ..Default::default()
            }
            .to_link(),
            "oprobe://run/x"
        );
    }
}
