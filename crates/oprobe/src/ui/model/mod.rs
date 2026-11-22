//! What the interactive surfaces show, in plain Rust (no UI types): formatting, scales,
//! series, the Requests table's filter and sort, connection tables, comparisons, preferences.
//! Unit-tested here; the views only read it.

pub mod cloud;
pub mod commands;
pub mod compare;
pub mod fmt;
pub mod health;
pub mod hints;
pub mod i18n;
pub mod icons;
pub mod link;
pub mod live;
pub mod metrics;
pub mod requests;
pub mod run;
pub mod runs;
pub mod scale;
pub mod series;
pub mod target;
pub mod verdicts;
pub mod verify;
pub mod waterfall;

use serde::{Deserialize, Serialize};

/// The inspector's tabs, in order (⌘1–⌘6, `1`–`6` in the terminal).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tab {
    #[default]
    Overview,
    Requests,
    Timeline,
    Connections,
    Findings,
    Cost,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Overview,
        Tab::Requests,
        Tab::Timeline,
        Tab::Connections,
        Tab::Findings,
        Tab::Cost,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Requests => "Requests",
            Tab::Timeline => "Timeline",
            Tab::Connections => "Connections",
            Tab::Findings => "Findings",
            Tab::Cost => "Cost",
        }
    }

    /// The tab's title in the App's language.
    pub fn title(self) -> &'static str {
        use crate::ui::model::i18n::tr;
        match self {
            Tab::Overview => tr("tab.overview"),
            Tab::Requests => tr("tab.requests"),
            Tab::Timeline => tr("tab.timeline"),
            Tab::Connections => tr("tab.connections"),
            Tab::Findings => tr("tab.findings"),
            Tab::Cost => tr("tab.cost"),
        }
    }

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Settings the app keeps between launches (NSH `Storage`, key [`Prefs::KEY`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub sidebar_width: f32,
    pub inspector_width: f32,
    pub sidebar_visible: bool,
    pub inspector_visible: bool,
    pub tab: Tab,
    /// Most recent first.
    pub recent_targets: Vec<String>,
    pub preset: String,
    pub duration: String,
    pub concurrency: String,
    /// Latency axes start logarithmic.
    pub log_latency: bool,
    /// Notify when a run finishes while the window is in the background.
    pub notify: bool,
    /// `system`, `light` or `dark`.
    pub appearance: String,
    /// The app's text size relative to the system's (Settings → Text Size; U15).
    pub text_scale: f32,
    /// The app was last left on a run (it opens there), not on a target's health.
    pub start_on_run: bool,
}

impl Default for Prefs {
    /// New runs start from the CLI's built-in defaults ([`crate::config::defaults`]).
    fn default() -> Self {
        let d = crate::config::defaults();
        Self {
            sidebar_width: 240.0,
            inspector_width: 300.0,
            sidebar_visible: true,
            inspector_visible: true,
            tab: Tab::Overview,
            recent_targets: Vec::new(),
            preset: d.workload.preset.slug().into(),
            duration: d.workload.duration.to_string(),
            concurrency: d.knobs.concurrency.to_string(),
            log_latency: true,
            notify: true,
            appearance: "system".into(),
            text_scale: 1.0,
            start_on_run: false,
        }
    }
}

impl Prefs {
    pub const KEY: &'static str = "oprobe.prefs.v3";
    const MAX_RECENT: usize = 8;

    pub fn from_json(s: Option<&str>) -> Self {
        s.and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    /// The text scales Settings offers.
    pub const TEXT_SCALES: [(f32, &'static str); 4] = [
        (0.9, "Small"),
        (1.0, "Default"),
        (1.15, "Large"),
        (1.3, "Larger"),
    ];

    /// [`Prefs::text_scale`], kept within what the layout supports.
    pub fn text_scale(&self) -> f32 {
        if self.text_scale.is_finite() {
            self.text_scale.clamp(0.8, 1.5)
        } else {
            1.0
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Remember `target` as the most recent.
    pub fn push_recent(&mut self, target: &str) {
        let t = target.trim();
        if t.is_empty() {
            return;
        }
        self.recent_targets.retain(|r| r != t);
        self.recent_targets.insert(0, t.to_owned());
        self.recent_targets.truncate(Self::MAX_RECENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_round_trip_and_tolerate_old_or_broken_json() {
        let mut p = Prefs::default();
        p.push_recent("s3://a");
        p.push_recent("s3://b");
        p.push_recent("s3://a");
        assert_eq!(p.recent_targets, vec!["s3://a", "s3://b"]);
        p.tab = Tab::Timeline;
        assert_eq!(Prefs::from_json(Some(&p.to_json())), p);
        assert_eq!(Prefs::from_json(Some("{\"tab\":\"cost\"}")).tab, Tab::Cost);
        assert_eq!(Prefs::from_json(Some("nonsense")), Prefs::default());
    }
}
