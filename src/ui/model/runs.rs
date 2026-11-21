//! The run list: what a row says (partial badge, "stopped during warmup"), search, the
//! truthful `RUNS (n)` count with "show more", and which run a compare pairs with by default
//! (the same target identity and workload hash, design v4 §2 §11).

use crate::store::RunSummary;

/// How many runs the sidebar lists before "Show more".
pub const PAGE: usize = 50;

/// The partial badge of a run: `partial · budget`, `partial · interrupted`. A budget stop is
/// a partial run, not a failure.
pub fn partial_badge(r: &RunSummary) -> Option<String> {
    match (r.partial.as_deref(), r.interrupted) {
        (Some(why), _) => Some(format!("partial · {why}")),
        (None, true) => Some("partial · interrupted".into()),
        (None, false) => None,
    }
}

/// The run ended before its measurement window started (stopped during warmup or while the
/// dataset was being prepared): it has no throughput to show.
pub fn stopped_in_warmup(r: &RunSummary) -> bool {
    (r.interrupted || r.partial.is_some()) && (r.window_s < 0.05 || r.ops_per_s <= 0.0)
}

/// The numbers of a run row: `12 400 ops/s · p99 31 ms`, or `stopped during warmup`.
pub fn row_numbers(r: &RunSummary) -> String {
    if stopped_in_warmup(r) {
        return "stopped during warmup".into();
    }
    format!(
        "{} ops/s · p99 {}",
        super::fmt::compact(r.ops_per_s),
        super::fmt::dur_ms(r.p99_ms)
    )
}

/// Whether `r` matches the sidebar search: every word appears in its id, label, target,
/// headline or partial reason (case-insensitive).
pub fn matches(r: &RunSummary, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let hay = format!(
        "{} {} {} {} {} {}",
        r.id,
        r.label.as_deref().unwrap_or(""),
        r.target_name.as_deref().unwrap_or(""),
        r.target_display,
        r.headline,
        partial_badge(r).unwrap_or_default()
    )
    .to_lowercase();
    q.split_whitespace().all(|w| hay.contains(w))
}

/// What the sidebar lists: the runs matching `query`, at most `limit` of them, and how many
/// matched in all (the header shows `RUNS (shown of matched)` when they differ).
pub fn visible(runs: &[RunSummary], query: &str, limit: usize) -> (Vec<RunSummary>, usize) {
    let all: Vec<&RunSummary> = runs.iter().filter(|r| matches(r, query)).collect();
    let total = all.len();
    (all.into_iter().take(limit).cloned().collect(), total)
}

/// The sidebar header: `RUNS (12)`, or `RUNS (50 of 180)` while some are not listed.
pub fn header(shown: usize, matched: usize, all: usize) -> String {
    if shown == all {
        format!("RUNS ({all})")
    } else {
        format!("RUNS ({shown} of {matched})")
    }
}

/// Two runs measured the same thing when they share a (known) target identity and workload.
pub fn same_workload(a: &RunSummary, b: &RunSummary) -> bool {
    !a.target_id.is_empty() && a.target_id == b.target_id && a.workload_hash == b.workload_hash
}

/// The default compare of run `current` (newest-first `runs`): the newest earlier run of the
/// same target and workload as A, `current` as B. `None` when there is no such run.
pub fn default_pair(runs: &[RunSummary], current: &str) -> Option<(String, String)> {
    let pos = runs.iter().position(|r| r.id == current)?;
    let cur = &runs[pos];
    runs[pos + 1..]
        .iter()
        .find(|r| same_workload(r, cur))
        .map(|prev| (prev.id.clone(), cur.id.clone()))
}

/// The runs a compare selector offers for a side whose other side is `other`: the runs of
/// the same target and workload first, then the rest (newest first within each).
pub fn compare_choices(runs: &[RunSummary], other: Option<&str>) -> Vec<RunSummary> {
    let Some(o) = other.and_then(|id| runs.iter().find(|r| r.id == id)) else {
        return runs.to_vec();
    };
    let (mut same, rest): (Vec<RunSummary>, Vec<RunSummary>) =
        runs.iter().cloned().partition(|r| same_workload(r, o));
    same.extend(rest);
    same
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn run(id: &str, target: &str, workload: &str) -> RunSummary {
        RunSummary {
            id: id.into(),
            target_display: target.into(),
            target_id: target.into(),
            workload_hash: workload.into(),
            window_s: 10.0,
            ops_per_s: 100.0,
            p99_ms: 12.0,
            ..Default::default()
        }
    }

    #[test]
    fn partial_runs_have_a_badge_and_warmup_stops_show_no_rate() {
        let mut r = run("20261121-100000-aaaa", "s3://b", "w1");
        assert_eq!(partial_badge(&r), None);
        assert!(row_numbers(&r).contains("ops/s"));
        r.partial = Some("budget".into());
        r.interrupted = true;
        assert_eq!(partial_badge(&r).as_deref(), Some("partial · budget"));
        assert!(!stopped_in_warmup(&r));
        r.window_s = 0.0;
        r.ops_per_s = 0.0;
        assert!(stopped_in_warmup(&r));
        assert_eq!(row_numbers(&r), "stopped during warmup");
        let mut i = run("20261121-100000-bbbb", "s3://b", "w1");
        i.interrupted = true;
        assert_eq!(partial_badge(&i).as_deref(), Some("partial · interrupted"));
    }

    #[test]
    fn search_limit_and_a_truthful_count() {
        let runs: Vec<RunSummary> = (0..120)
            .map(|i| {
                let mut r = run(&format!("20261121-10{i:04}-aaaa"), "s3://b", "w");
                r.label = Some(if i % 2 == 0 { "nightly" } else { "adhoc" }.into());
                r
            })
            .collect();
        let (shown, matched) = visible(&runs, "", PAGE);
        assert_eq!((shown.len(), matched), (50, 120));
        assert_eq!(header(shown.len(), matched, runs.len()), "RUNS (50 of 120)");
        let (shown, matched) = visible(&runs, "NIGHTLY", 200);
        assert_eq!((shown.len(), matched), (60, 60));
        assert_eq!(header(60, 60, 120), "RUNS (60 of 60)");
        assert_eq!(header(120, 120, 120), "RUNS (120)");
    }

    #[test]
    fn compare_pairs_the_same_target_and_workload() {
        // Newest first: c (t1/w1), x (t2/w1), y (t1/w2), a (t1/w1).
        let runs = vec![
            run("20261121-100400-cccc", "t1", "w1"),
            run("20261121-100300-xxxx", "t2", "w1"),
            run("20261121-100200-yyyy", "t1", "w2"),
            run("20261121-100100-aaaa", "t1", "w1"),
        ];
        assert_eq!(
            default_pair(&runs, "20261121-100400-cccc"),
            Some(("20261121-100100-aaaa".into(), "20261121-100400-cccc".into()))
        );
        // Nothing earlier measured the same thing: no default pair.
        assert_eq!(default_pair(&runs, "20261121-100300-xxxx"), None);
        assert_eq!(default_pair(&runs, "20261121-100100-aaaa"), None);
        // An unknown target identity never pairs.
        let mut u = runs.clone();
        u[0].target_id.clear();
        u[3].target_id.clear();
        assert_eq!(default_pair(&u, "20261121-100400-cccc"), None);
        let ch = compare_choices(&runs, Some("20261121-100400-cccc"));
        assert_eq!(ch[0].id, "20261121-100400-cccc");
        assert_eq!(ch[1].id, "20261121-100100-aaaa");
    }
}
