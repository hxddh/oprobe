//! One vocabulary and one colour scale for every judgement the App shows (design v5 §9: U2,
//! U3, U9). Run verdicts, finding severities, net verdicts, gate checks and verify outcomes all
//! map to a [`Level`] here, and the views colour by level only — so the Overview, the Findings
//! tab, Compare, Verify and the Health screen can never contradict each other, and the words
//! match the CLI's catalog.
//!
//! The judgements themselves come from the core: [`crate::model::Verdict`] (the run),
//! [`crate::analysis::diagnosis::Severity`] (findings), [`NetAssessment`] (verify and compare,
//! computed by the analysis), [`crate::analysis::compare::MetricStat`] (significance).

use crate::analysis::compare::Comparison;
use crate::analysis::diagnosis::Severity;
use crate::analysis::verdict::{MetricMove, NetAssessment, NetVerdict};
use crate::analysis::verification::VerifyOutcome;
use crate::model::{Report, VerdictLevel};
use crate::report::diff::{Check, GateVerdict, Status};
use crate::ui::model::i18n::tr;

/// How a judgement is coloured. `Inconclusive` has its own (hatched / dimmed) style: it is never
/// shown as "same" or as a pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Neutral,
    Ok,
    Info,
    Inconclusive,
    Warn,
    Crit,
}

/// A run's verdict: its word and level.
pub fn run_level(v: VerdictLevel) -> (&'static str, Level) {
    match v {
        VerdictLevel::Ok => (tr("level.healthy"), Level::Ok),
        VerdictLevel::Review => (tr("level.review"), Level::Info),
        // "attention" and "warning" were two words for one thing (U9).
        VerdictLevel::Attention => (tr("level.warning"), Level::Warn),
        VerdictLevel::Critical => (tr("level.critical"), Level::Crit),
    }
}

/// A finding's severity: its word and level (the same scale as [`run_level`]).
pub fn severity(s: Severity) -> (&'static str, Level) {
    match s {
        Severity::Info => (tr("severity.info"), Level::Info),
        Severity::Warning => (tr("severity.warning"), Level::Warn),
        Severity::Critical => (tr("severity.critical"), Level::Crit),
    }
}

/// A net verdict: its word and level.
pub fn net(v: NetVerdict) -> (&'static str, Level) {
    match v {
        NetVerdict::Improved => (tr("net.improved"), Level::Ok),
        NetVerdict::Regressed => (tr("net.regressed"), Level::Crit),
        NetVerdict::Traded => (tr("net.traded"), Level::Warn),
        NetVerdict::NoChange => (tr("net.no_change"), Level::Neutral),
        NetVerdict::Inconclusive => (tr("net.inconclusive"), Level::Inconclusive),
    }
}

/// A verify outcome (did the predicted metric move as the finding said): word and level.
pub fn verify_outcome(o: VerifyOutcome) -> (&'static str, Level) {
    match o {
        VerifyOutcome::Confirmed => (tr("verify.confirmed"), Level::Ok),
        VerifyOutcome::Refuted => (tr("verify.refuted"), Level::Crit),
        VerifyOutcome::Inconclusive => (tr("verify.inconclusive"), Level::Inconclusive),
    }
}

/// What a gate check says, shown: never "same" for a change the gate could not judge (U3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Improved,
    Regressed,
    /// Within the limits, but the change is too large to call "no change" and not shown to
    /// be significant (one run per side, or an interval that includes zero).
    Inconclusive,
    /// Within the limits and small.
    Within,
    Skipped,
}

impl CheckState {
    pub fn word(self) -> &'static str {
        match self {
            CheckState::Improved => tr("check.improved"),
            CheckState::Regressed => tr("check.regressed"),
            CheckState::Inconclusive => tr("check.inconclusive"),
            CheckState::Within => tr("check.within"),
            CheckState::Skipped => tr("check.skipped"),
        }
    }

    pub fn level(self) -> Level {
        match self {
            CheckState::Improved => Level::Ok,
            CheckState::Regressed => Level::Crit,
            CheckState::Inconclusive => Level::Inconclusive,
            CheckState::Within | CheckState::Skipped => Level::Neutral,
        }
    }
}

/// A relative change this large is never "within limits" without significance behind it.
pub const INCONCLUSIVE_RATIO: f64 = 0.10;

/// The state of gate check `c`. `significant` is the comparison's significance of the same
/// metric when it has one (`MetricStat::significant`).
pub fn check_state(c: &Check, significant: Option<bool>) -> CheckState {
    match c.verdict {
        Status::Fail => CheckState::Regressed,
        Status::Improved => CheckState::Improved,
        Status::Skipped => CheckState::Skipped,
        Status::Pass => {
            let big = c.delta_ratio.is_none_or(|d| {
                let half_limit = c.limit_ratio.map(|l| l * 0.5).unwrap_or(INCONCLUSIVE_RATIO);
                d.abs() >= half_limit.min(INCONCLUSIVE_RATIO).max(0.01)
            }) && c.baseline != c.candidate;
            // A big change the comparison's own statistics call noise is within the limits.
            if big && significant != Some(false) {
                CheckState::Inconclusive
            } else {
                CheckState::Within
            }
        }
    }
}

/// The comparison's significance for check `c` (by metric name), when it has one.
pub fn significance_of(cmp: &Comparison, c: &Check) -> Option<bool> {
    let metric = match c.id.as_str() {
        "throughput" => "ops_per_s",
        id if id.starts_with("latency_p99") => "p99_ms",
        id if id.starts_with("latency_p50") => "p50_ms",
        _ => return None,
    };
    cmp.stat(metric).map(|s| s.significant)
}

/// The net assessment of a comparison: the analysis's own (`Comparison.net`), else one read off
/// the gate — a regression with an improvement is a trade, never a plain "confirmed" (U2). The
/// Verify banner and Compare both show this one value.
pub fn net_of(cmp: &Comparison) -> NetAssessment {
    if let Some(n) = &cmp.net {
        return n.clone();
    }
    net_of_gate(&cmp.gate, |c| significance_of(cmp, c))
}

/// [`net_of`] from the gate checks alone.
pub fn net_of_gate(
    gate: &crate::report::diff::DiffReport,
    significant: impl Fn(&Check) -> Option<bool>,
) -> NetAssessment {
    let mv = |c: &Check| MetricMove {
        metric: c.id.clone(),
        delta_ratio: c.delta_ratio.unwrap_or(0.0),
    };
    if gate.verdict == GateVerdict::NotComparable {
        return NetAssessment {
            verdict: NetVerdict::Inconclusive,
            better: Vec::new(),
            worse: Vec::new(),
        };
    }
    let mut better = Vec::new();
    let mut worse = Vec::new();
    let mut unsure = false;
    for c in &gate.checks {
        match check_state(c, significant(c)) {
            CheckState::Improved => better.push(mv(c)),
            CheckState::Regressed => worse.push(mv(c)),
            CheckState::Inconclusive => unsure = true,
            _ => {}
        }
    }
    let verdict = match (better.is_empty(), worse.is_empty()) {
        (false, false) => NetVerdict::Traded,
        (true, false) => NetVerdict::Regressed,
        (false, true) => NetVerdict::Improved,
        (true, true) if unsure => NetVerdict::Inconclusive,
        (true, true) => NetVerdict::NoChange,
    };
    NetAssessment {
        verdict,
        better,
        worse,
    }
}

/// A net assessment in one human sentence: `traded: Throttled requests −95 %, Throughput −21 %`
/// (metric names, not machine ids — U12).
pub fn net_sentence(n: &NetAssessment) -> String {
    let (word, _) = net(n.verdict);
    let moves: Vec<String> = n
        .better
        .iter()
        .chain(&n.worse)
        .map(|m| format!("{} {:+.1} %", metric_name(&m.metric), m.delta_ratio * 100.0))
        .collect();
    if moves.is_empty() {
        word.to_owned()
    } else {
        format!("{word}: {}", moves.join(", "))
    }
}

/// A metric or gate check id in words (`latency_p99.read.range` → `p99 latency (read.range)`).
pub fn metric_name(id: &str) -> String {
    let (base, verb) = match id.split_once('.') {
        Some((b, v)) if b.starts_with("latency_") || b == "present" => (b, Some(v)),
        _ => (id, None),
    };
    let key: Option<&'static str> = match base {
        "ops_per_s" | "throughput" => Some("metric.ops_per_s"),
        "mib_per_s" | "throughput_bytes" => Some("metric.mib_per_s"),
        "p50_ms" | "latency_p50" => Some("metric.p50_ms"),
        "p99_ms" | "latency_p99" => Some("metric.p99_ms"),
        "p999_ms" => Some("metric.p999_ms"),
        "error_ratio" | "error_rate" => Some("metric.error_ratio"),
        "throttled_request_ratio" | "throttle_ratio" => Some("metric.throttled_request_ratio"),
        "requests_per_op" => Some("metric.requests_per_op"),
        "read_amplification" => Some("metric.read_amplification"),
        "cost_per_million_ops" => Some("metric.cost_per_million_ops"),
        _ => None,
    };
    let name = key
        .map(|k| tr(k).to_owned())
        .unwrap_or_else(|| base.replace('_', " "));
    match verb {
        Some(v) => format!("{name} ({v})"),
        None => name,
    }
}

/// Which finding code speaks for a headline metric (so a KPI is coloured by what the analysis
/// concluded about it, not by a fixed rule — U9).
fn finding_for(metric: &str) -> &'static [&'static str] {
    match metric {
        "error_ratio" => &["errors", "integrity"],
        "throttled_request_ratio" => &["throttling", "retries"],
        "p99_ms" => &["tail_latency", "tail_stragglers", "server_latency"],
        _ => &[],
    }
}

/// The level of a headline metric: the severity of the finding about it, else neutral. Errors
/// of 0.27 % under a "critical throttling" verdict are no longer redder than the throttling.
pub fn metric_level(r: &Report, metric: &str) -> Level {
    let codes = finding_for(metric);
    r.findings
        .iter()
        .filter(|f| codes.contains(&f.code.as_str()))
        .map(|f| severity(f.severity).1)
        .max()
        .unwrap_or(Level::Neutral)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::diff::DiffReport;

    fn check(id: &str, verdict: Status, base: f64, cand: f64, limit: Option<f64>) -> Check {
        Check {
            id: id.into(),
            unit: "ms".into(),
            baseline: base,
            candidate: cand,
            delta_ratio: Some((cand - base) / base),
            limit_ratio: limit,
            abs_limit: None,
            verdict,
            note: String::new(),
        }
    }

    fn gate(verdict: GateVerdict, checks: Vec<Check>) -> DiffReport {
        DiffReport {
            verdict,
            passed: verdict == GateVerdict::Passed,
            comparable: verdict != GateVerdict::NotComparable,
            baseline: "a".into(),
            candidate: "b".into(),
            thresholds: Default::default(),
            identity: Default::default(),
            config_changes: Vec::new(),
            checks,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_large_unsignificant_change_is_inconclusive_never_same() {
        // U3: p99 +87.7 % that the gate passed (too few tail samples).
        let c = check("latency_p99.read", Status::Pass, 10.0, 18.77, Some(1.0));
        assert_eq!(check_state(&c, None), CheckState::Inconclusive);
        assert_eq!(check_state(&c, Some(true)), CheckState::Inconclusive);
        // The comparison's statistics call it noise.
        assert_eq!(check_state(&c, Some(false)), CheckState::Within);
        let small = check("latency_p99.read", Status::Pass, 10.0, 10.2, Some(0.2));
        assert_eq!(check_state(&small, None), CheckState::Within);
        let fail = check("throughput", Status::Fail, 1480.0, 915.0, Some(0.1));
        assert_eq!(check_state(&fail, None), CheckState::Regressed);
        assert_eq!(CheckState::Inconclusive.level(), Level::Inconclusive);
    }

    #[test]
    fn a_regression_with_an_improvement_is_a_trade() {
        // U2: throttling 4.58 → 0 %, ops −20.6 %: "traded", not "confirmed" next to "regression".
        let g = gate(
            GateVerdict::Regression,
            vec![
                check(
                    "throttled_request_ratio",
                    Status::Improved,
                    0.0458,
                    0.0001,
                    None,
                ),
                check("throughput", Status::Fail, 1480.0, 1175.0, Some(0.1)),
            ],
        );
        let n = net_of_gate(&g, |_| None);
        assert_eq!(n.verdict, NetVerdict::Traded);
        let s = net_sentence(&n);
        assert!(s.starts_with("traded: Throttled requests"), "{s}");
        assert!(s.contains("Throughput -20.6 %"), "{s}");
        let none = gate(GateVerdict::NotComparable, Vec::new());
        assert_eq!(
            net_of_gate(&none, |_| None).verdict,
            NetVerdict::Inconclusive
        );
    }

    #[test]
    fn one_scale_for_every_judgement() {
        assert_eq!(
            run_level(VerdictLevel::Attention).1,
            severity(Severity::Warning).1
        );
        assert_eq!(run_level(VerdictLevel::Attention).0, "Warning");
        assert_eq!(run_level(VerdictLevel::Critical).1, Level::Crit);
        assert_eq!(net(NetVerdict::Inconclusive).1, Level::Inconclusive);
        assert_eq!(
            metric_name("latency_p99.read.range"),
            "p99 latency (read.range)"
        );
        assert_eq!(metric_name("ops_per_s"), "Throughput");
        assert_eq!(metric_name("summary_weird_thing"), "summary weird thing");
    }

    #[test]
    fn kpis_are_coloured_by_the_finding_about_them() {
        let mut r = crate::report::fixture::report();
        r.findings.retain(|f| f.code == "throttling");
        for f in &mut r.findings {
            f.severity = Severity::Critical;
        }
        assert_eq!(metric_level(&r, "throttled_request_ratio"), Level::Crit);
        // No finding about errors: neutral, however non-zero the ratio is.
        assert_eq!(metric_level(&r, "error_ratio"), Level::Neutral);
    }
}
