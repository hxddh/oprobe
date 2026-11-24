//! The app's words in English and 简体中文 (design v6 D2): the one message catalog of oprobe.
//! The app follows the system's language; a setting in the app overrides it
//! ([`crate::ui::store::Store::set_lang`]). Numbers, units, codes, operator paths and the
//! text the core writes (a finding's cause and evidence) stay as they are; finding titles are
//! translated here by code.

use std::cell::Cell;
use std::fmt::Display;

/// A language of the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh-CN",
        }
    }

    /// A locale tag or language name: `zh`, `zh-CN`, `zh_CN.UTF-8`, `zh-Hans` → 中文; `en…` →
    /// English; anything else is not one of the app's languages.
    pub fn parse(tag: &str) -> Option<Lang> {
        let t = tag.trim().to_ascii_lowercase();
        if t.starts_with("zh") {
            Some(Lang::Zh)
        } else if t.starts_with("en") {
            Some(Lang::En)
        } else {
            None
        }
    }
}

thread_local! {
    static CURRENT: Cell<Lang> = const { Cell::new(Lang::En) };
}

/// Make `l` the language [`tr`] answers in (the root re-draws the app after it).
pub fn set(l: Lang) {
    CURRENT.with(|c| c.set(l));
}

pub fn current() -> Lang {
    CURRENT.with(|c| c.get())
}

/// The system's language: the platform's preferred languages and locale, else the locale
/// variables (a terminal), else English.
pub fn system() -> Lang {
    let platform = nsh_core::prelude::locale_settings()
        .languages
        .iter()
        .find_map(|l| Lang::parse(l.as_str()))
        .or_else(|| Lang::parse(nsh_core::prelude::locale().as_str()))
        .filter(|l| *l != Lang::En);
    platform
        .or_else(|| {
            ["LC_ALL", "LC_MESSAGES", "LANG"]
                .iter()
                .filter_map(|k| std::env::var(k).ok())
                .find(|v| !v.is_empty())
                .and_then(|v| Lang::parse(&v))
        })
        .unwrap_or(Lang::En)
}

/// `key` in the current language (English, then the key itself, when missing).
pub fn tr(key: &'static str) -> &'static str {
    tr_dyn(key).unwrap_or(key)
}

/// A key made at run time (`verdict.ok`, `finding.throttling`): `None` when the catalog has
/// no entry in either language.
pub fn tr_dyn(key: &str) -> Option<&'static str> {
    find(current(), key).or_else(|| find(Lang::En, key))
}

/// `key` with each `{}` filled by the next of `args`.
pub fn trf(key: &'static str, args: &[&dyn Display]) -> String {
    let mut out = String::new();
    let mut rest = tr(key);
    for a in args {
        match rest.split_once("{}") {
            Some((head, tail)) => {
                out.push_str(head);
                out.push_str(&a.to_string());
                rest = tail;
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// The title of a finding by its code (the code in words when the catalog has none).
pub fn finding_title(code: &str) -> String {
    tr_dyn(&format!("finding.{code}"))
        .map(str::to_owned)
        .unwrap_or_else(|| code.replace('_', " "))
}

fn find(l: Lang, key: &str) -> Option<&'static str> {
    let table = match l {
        Lang::En => EN,
        Lang::Zh => ZH,
    };
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

static EN: &[(&str, &str)] = &[
    ("plot.read_written", "read + written"),
    ("timeline.throughput", "Throughput"),
    ("timeline.problems", "Problems per second"),
    ("conn.dialed_of", "{} during warm-up · {} in the window"),
    ("conn.dialed_window", "{} in the window"),
    ("conn.min_rtt", "Round trip, minimum (kernel)"),
    (
        "conn.rtt_inflated",
        "The smoothed round trip includes the server's delay and delayed acknowledgements; the minimum is the network's.",
    ),
    ("slice.cold", "first read"),
    ("slice.warm", "re-read"),
    ("compare.p99", "p99 over time"),
    ("compare.requests", "Requests per second"),
    ("compare.apis", "Requests by API"),
    ("compare.ttfb_p99", "TTFB p99"),
    ("compare.requests_of", "{} requests"),
    ("compare.requests_per_s", "Requests/s"),
    ("compare.failed", "Failed ops"),
    ("compare.retries", "Retries"),
    ("compare.throttled_count", "Throttled responses"),
    ("ev.affected_apis_count", "APIs affected"),
    ("ev.p50_ms", "Operation p50"),
    ("ev.p99_ms", "Operation p99"),
    ("ev.replayed_p99_ms", "p99 with the remedy (replayed)"),
    ("ev.typical_ttfb_ms", "Typical time to first byte"),
    ("ev.median_request_ms", "Median request"),
    ("ev.slowest_request_ms", "Slowest request"),
    ("ev.straggler_pct", "Stragglers"),
    ("ev.straggler_ttfb_ms", "Straggler wait (p50)"),
    ("ev.time_cost_ratio", "Share of the time it costs"),
    ("ev.throttled_request_ratio", "Throttled requests"),
    ("ev.throttled_op_ratio", "Throttled operations"),
    ("ev.error_ratio", "Failed operations"),
    ("ev.p99_to_p50_ratio", "p99 / p50"),
    ("ev.overread_ratio", "Over-read"),
    ("ev.ttfb_pct", "Time waiting for the first byte"),
    ("ev.retry_pct", "Time in retry backoff"),
    ("ev.queue_pct", "Time queued"),
    ("ev.connect_pct", "Time connecting"),
    ("ev.body_pct", "Time receiving the body"),
    ("ev.upload_pct", "Time uploading"),
    ("ev.reuse_pct", "Connection reuse"),
    ("ev.dns_p50_ms", "DNS p50"),
    ("ev.tcp_p50_ms", "TCP connect p50"),
    ("ev.tls_p50_ms", "TLS handshake p50"),
    ("ev.cold_requests_count", "Requests on new connections"),
    ("ev.client_cpu_ratio", "Client CPU"),
    ("ev.sched_lag_p99_ms", "Scheduler lag p99"),
    ("ev.hot_key_share_ratio", "Hottest key's share"),
    ("ev.explains_ratio", "Share it explains"),
    ("ev.stream_bytes_per_s", "Per-stream throughput"),
    ("ev.fitted_bytes_per_s", "Fitted bandwidth"),
    ("ev.fitted_requests_per_s", "Fitted request rate"),
    ("ev.fitted_inflight_count", "Fitted limit (in flight)"),
    ("ev.ceiling_lo_inflight_count", "Clean up to (in flight)"),
    ("ev.ceiling_hi_inflight_count", "Throttled from (in flight)"),
    (
        "ev.peak_inflight_per_op_count",
        "In flight per operation (peak)",
    ),
    ("ev.requests_per_op_mean", "Requests per operation"),
    ("ev.parts_per_op_mean", "Parts per operation"),
    ("ev.inflight_per_op_mean", "In flight per operation"),
    (
        "ev.extra_requests_per_1k_ops_mean",
        "Extra requests per 1 000 ops",
    ),
    (
        "ev.saved_requests_per_1k_ops_mean",
        "Requests saved per 1 000 reads",
    ),
    ("ev.mean_request_bytes", "Mean request size"),
    ("ev.tail_cut_deadline_ms", "Tail-cut deadline"),
    ("ev.tail_cut_blind_s", "Tail-cut blind for"),
    ("ev.accepted_requests_per_s", "Accepted while throttled"),
    ("ev.monthly_usd", "Projected monthly bill"),
    ("ev.server_errors_count", "Server errors"),
    ("ev.failed_ops_count", "Failed operations"),
    ("ev.hung_requests_count", "Hung requests"),
    ("ev.timeouts_count", "Timeouts"),
    ("ev.throttled_responses_count", "Throttled responses"),
    (
        "ev.rate_limited_failures_count",
        "Failures from the rate limit",
    ),
    ("ev.missed_arrivals_count", "Missed arrivals"),
    ("ev.window_s", "Window"),
    ("app.dismiss", "Dismiss"),
    ("metric.p999", "p99.9"),
    ("metric.errors_of_ops", "Failed ops"),
    ("metric.throttled_of_requests", "Throttled reqs"),
    (
        "metric.counts",
        "{} operations · {} requests · {} measured · failed per operation, throttled per request",
    ),
    ("trust.title", "Read these numbers with care"),
    (
        "trust.saturated",
        "The client was saturated (CPU {} · scheduling lag p99 {}): server time may include the client's own delay.",
    ),
    ("trust.partial", "The run is partial: {}."),
    ("trust.missed", "{} of the open-loop arrivals started late."),
    (
        "trust.few",
        "Only {} operations were measured: the percentiles are rough.",
    ),
    ("profile.requests", "Requests by API"),
    ("profile.retries", "Retries and failures"),
    ("profile.details", "Details"),
    ("profile.setup", "Run setup"),
    ("col.api", "API"),
    ("col.requests", "Requests"),
    ("col.status", "Status"),
    ("col.ttfb", "TTFB p50 / p99"),
    ("col.total_p50_p99", "Total p50 / p99"),
    ("col.bytes", "Down / up"),
    ("col.reused", "Reused"),
    ("col.key", "Key"),
    ("col.share", "Share"),
    ("col.throttled", "Throttled"),
    ("col.p99", "p99"),
    ("retry.retries", "Retries"),
    ("retry.backoff", "Time waiting between attempts"),
    ("retry.throttled", "Throttling responses"),
    ("retry.after", "Longest Retry-After asked"),
    ("retry.errors", "Other errors"),
    ("retry.errors_split", "server {} · client {} · network {}"),
    ("retry.failed", "Failed operations"),
    ("retry.classes", "Failure classes"),
    ("details.bytes", "Bytes asked for → moved"),
    ("bytes.read", "Read"),
    ("bytes.write", "Written"),
    ("bytes.wire", "On the wire (in / out)"),
    ("bytes.cache", "Served by the cache"),
    ("bytes.amplification", "Amplification"),
    (
        "bytes.amplification_of",
        "read ×{} · over-read ×{} · write ×{} · {} requests per operation",
    ),
    ("details.hot_keys", "Hottest keys"),
    ("details.sizes", "Object sizes"),
    ("details.prefixes", "Prefixes"),
    ("details.remotes", "Server addresses"),
    ("setup.target", "Target"),
    ("setup.endpoint", "Endpoint"),
    ("setup.workload", "Workload"),
    ("setup.load", "Load"),
    ("setup.open_loop", "open loop, {} operations/s"),
    ("setup.closed_loop", "closed loop, {} workers"),
    ("setup.duration", "Duration"),
    ("setup.duration_of", "{} after {} warm-up"),
    ("setup.dataset", "Dataset"),
    ("setup.mix", "Mix"),
    ("setup.seed", "Seed"),
    ("setup.started", "Started"),
    ("setup.client", "Client"),
    ("setup.kept", "Trace rows kept"),
    ("setup.copy", "Copy command"),
    ("plot.inflight", "Requests in flight"),
    ("plot.peak", "peak per second"),
    (
        "compare.note",
        "Each column is one run: their differences include run-to-run noise. Prove a change to measure it.",
    ),
    ("menu.file", "File"),
    ("menu.edit", "Edit"),
    ("menu.run", "Run"),
    ("menu.view", "View"),
    ("menu.window", "Window"),
    ("menu.help", "Help"),
    ("menu.settings", "Settings…"),
    ("menu.new_run", "New Run…"),
    ("menu.open_file", "Open Run File…"),
    ("menu.copy_id", "Copy Run ID"),
    ("menu.copy_command", "Copy Command"),
    ("menu.reveal_mac", "Show in Finder"),
    ("menu.reveal_windows", "Show in Explorer"),
    ("menu.reveal", "Show in Files"),
    ("menu.run_again", "Run Again"),
    ("menu.stop", "Stop"),
    ("menu.prove", "Prove…"),
    ("menu.compare", "Compare Selected"),
    ("menu.compare_n", "Compare {} Runs"),
    ("menu.compare_with", "Compare With Selected"),
    ("menu.delete", "Delete…"),
    ("menu.open", "Open"),
    ("menu.hide_sidebar", "Hide Sidebar"),
    ("menu.show_sidebar", "Show Sidebar"),
    ("menu.docs", "oprobe Documentation"),
    ("menu.glossary", "Glossary of Terms"),
    ("menu.primary", "Main Menu"),
    ("side.running", "Running"),
    ("runs.blank", " "),
    ("settings.title", "Settings"),
    ("settings.language", "Language"),
    ("settings.data", "Runs are stored in"),
    ("dialog.cancel", "Cancel"),
    ("dialog.new_run", "New run"),
    ("dialog.done", "Done"),
    ("delete.title", "Delete runs"),
    ("delete.one", "Delete run {}?"),
    ("delete.many", "Delete {} runs?"),
    (
        "delete.detail",
        "They move to the trash with their trial runs; Undo brings them back.",
    ),
    ("app.failed", "That did not work; see the log"),
    ("app.lang_auto", "System"),
    ("app.loading", "Loading…"),
    ("app.running", "running"),
    ("nav.runs", "Runs"),
    ("nav.profile", "Profile"),
    ("nav.operator", "Operator"),
    ("nav.compare", "Compare"),
    // Runs
    ("runs.new", "New run"),
    ("runs.operator", "Operator"),
    (
        "runs.operator_hint",
        "s3://bucket/prefix, op.toml, a configured name, or lab://demo",
    ),
    ("runs.start", "Run"),
    ("runs.stop", "Stop"),
    ("runs.lab", "Lab"),
    ("runs.preset", "Workload"),
    ("runs.duration", "Duration"),
    ("runs.concurrency", "Concurrency"),
    (
        "runs.several",
        "Several operators separated by spaces run one after another on the same workload and open side by side in Compare.",
    ),
    ("runs.live", "Live"),
    ("runs.history", "History · {} runs"),
    ("runs.empty", "No runs yet"),
    ("runs.profile_lab", "Profile the lab…"),
    (
        "runs.empty_hint",
        "Profile the local lab: an S3 with faults, no credentials, in seconds.",
    ),
    ("runs.open", "Open run {}"),
    ("runs.pick", "Compare {}"),
    ("runs.delete", "Delete {}"),
    ("runs.compare_picked", "Compare {} picked"),
    ("runs.pick_hint", "+ picks a run for Compare"),
    (
        "runs.need_operator",
        "Enter an operator first: a URL, an op.toml file or lab://demo",
    ),
    (
        "runs.bad_concurrency",
        "Concurrency is a whole number of at least 1",
    ),
    (
        "lab.demo",
        "2.5 % slow stragglers; throttling above 40 requests in flight.",
    ),
    ("lab.throttle", "503 SlowDown above 8 requests in flight."),
    (
        "lab.stragglers",
        "3 % of requests wait 400 ms before answering.",
    ),
    ("lab.bandwidth", "All responses share 16 MiB/s."),
    ("lab.flaky", "4 % server errors and 2 % connection resets."),
    ("lab.ratelimit", "503 SlowDown above 300 requests/s."),
    ("phase.idle", "Idle"),
    ("phase.preparing dataset", "Preparing the dataset"),
    ("phase.warming up", "Warming up"),
    ("phase.measuring", "Measuring"),
    ("phase.verifying", "Verifying"),
    ("phase.cleaning up", "Cleaning up"),
    ("phase.cleanup", "Cleaning up test data"),
    ("phase.done", "Done"),
    // Shared vocabulary
    ("metric.goodput", "Goodput"),
    ("metric.bandwidth", "Bandwidth"),
    ("metric.p50", "p50"),
    ("metric.p99", "p99"),
    ("metric.errors", "Errors"),
    ("metric.throttled", "Throttled"),
    ("verdict.ok", "OK"),
    ("verdict.review", "Review"),
    ("verdict.attention", "Attention"),
    ("verdict.critical", "Critical"),
    ("severity.info", "Info"),
    ("severity.warning", "Warning"),
    ("severity.critical", "Critical"),
    ("stage.queue", "Queue"),
    ("stage.cache", "Cache"),
    ("stage.connect", "Connect"),
    ("stage.upload", "Upload"),
    ("stage.server", "Server (TTFB)"),
    ("stage.download", "Download"),
    ("stage.retry", "Retry backoff"),
    ("stage.client", "Client"),
    ("req.queue", "queue"),
    ("req.connect", "connect"),
    ("req.send", "send"),
    ("req.ttfb", "TTFB"),
    ("req.body", "body"),
    ("req.stall", "stall"),
    ("req.retry", "retry backoff"),
    ("req.client", "client"),
    // Profile
    ("profile.none", "No run open"),
    ("profile.none_hint", "Open a run from Runs, or start one."),
    ("profile.to_operator", "Open in Operator"),
    ("profile.pick", "Compare…"),
    ("profile.time", "Where the time went"),
    ("profile.findings", "Findings · {}"),
    (
        "profile.no_findings",
        "Nothing to fix: no layer change is suggested for this run.",
    ),
    ("profile.remedy", "Remedy"),
    ("profile.advice", "Advice"),
    ("profile.try_remedy", "Prove this remedy"),
    ("profile.try_remedy_of", "Prove the remedy of {}"),
    ("profile.slowest", "Slowest operations · request waterfall"),
    ("profile.hover", "Point at a request to see its stages"),
    ("profile.timeline", "Timeline"),
    ("profile.latency", "Latency"),
    ("profile.heatmap", "Request latency over time"),
    ("profile.connections", "Connections"),
    ("profile.warnings", "Warnings"),
    ("conn.dialed", "Connections dialed"),
    ("conn.reuse", "Requests on reused connections"),
    ("conn.protocols", "Protocols"),
    ("conn.peak", "Peak requests in flight"),
    ("conn.dial_failures", "Failed dials"),
    ("conn.proxy", "Proxy"),
    ("conn.proxy_connect", "Proxy CONNECT"),
    ("conn.rtt", "Round trip, smoothed (kernel)"),
    ("conn.retransmitting", "Retransmitting connections"),
    ("conn.retransmitting_of", "{} of {} · {} segments"),
    ("conn.limited", "Sending limited by"),
    (
        "conn.limited_of",
        "receive window {} · send buffer {} of the time",
    ),
    ("conn.delivery", "Delivery rate p50"),
    ("conn.tls_handshakes", "TLS handshakes"),
    (
        "conn.tls_handshakes_of",
        "{} · {} resumed · {} HelloRetryRequest",
    ),
    ("conn.cold_start", "Cold start"),
    (
        "conn.cold_start_of",
        "warm-up dials: connect {} · first operations {}",
    ),
    ("write.path", "Writes"),
    ("write.part_size", "part size {}"),
    ("write.asked", "asked {}"),
    ("write.service", "the service takes {}–{}"),
    ("write.parts", "{} parts of {} on average"),
    ("details.cold_warm", "First read / re-read of a key"),
    ("col.errors", "Errors"),
    ("verb.rates", "{}/s · {} MiB/s · {} req/op"),
    ("verb.throttled", "throttled {}"),
    (
        "plot.hot_keys_throttling",
        "Hottest keys · share of all throttling",
    ),
    (
        "plot.hot_keys_requests",
        "Hottest keys · share of all requests",
    ),
    ("plot.new_connections", "new connections"),
    ("plot.cpu_cores", "client CPU (cores)"),
    ("plot.failed_ops", "failed operations"),
    ("profile.await_tree", "Async call stack while in flight"),
    ("sig.stream_wait", "h2 stream wait"),
    ("sig.flow_stall", "h2 flow-control stall"),
    ("sig.streams", "h2 streams open"),
    ("sig.upload_acked", "upload acknowledged at"),
    ("sig.send_buffered", "sent to the kernel at"),
    ("sig.cache_status", "CDN cache"),
    ("sig.region", "region"),
    // Operator
    ("operator.none", "No operator yet"),
    (
        "operator.none_hint",
        "Open a run first: its operator is the one you change and prove.",
    ),
    ("operator.of_run", "The operator of run {}"),
    ("operator.remedies", "Remedies of the findings"),
    (
        "operator.no_remedies",
        "No finding of this run names a layer change.",
    ),
    ("operator.apply", "Prove"),
    ("operator.section.layers", "Layers"),
    ("operator.section.read", "Read"),
    ("operator.section.write", "Write"),
    ("operator.section.http", "HTTP client"),
    ("operator.default", "default"),
    ("operator.changed", "changed"),
    ("operator.remedy_of", "Remedy for {}"),
    ("operator.changes", "Changes"),
    (
        "operator.no_changes",
        "No changes yet: switch a layer or change a value above.",
    ),
    ("operator.diff", "The change as Rust and as op.toml"),
    ("operator.copy_rust", "Copy Rust"),
    ("operator.copy_toml", "Copy TOML"),
    ("operator.reset", "Reset"),
    ("operator.prove", "Prove"),
    (
        "operator.prove_hint",
        "Trials of this run, A and B interleaved, decide whether a change helps on your objective. A finding's remedy is sized from the run, as `oprobe prove RUN CODE` does; a value you change is proved as you set it.",
    ),
    ("operator.objective", "Objective"),
    ("operator.budget", "Budget"),
    ("operator.prove_run", "Prove"),
    ("operator.trials", "{} trials finished"),
    ("operator.pair", "Pair {}"),
    ("operator.pairs_count", "{} pairs of trials"),
    (
        "operator.secondary",
        "Secondary, not decisive (shares as percentage points, pp): {}",
    ),
    ("operator.proven_diff", "The proven diff"),
    ("operator.stopped", "Stopped."),
    ("prove.improved", "improved"),
    ("prove.worse", "worse"),
    ("prove.inconclusive", "inconclusive"),
    // Compare
    ("compare.pick", "Runs to compare"),
    ("compare.none", "Select two or more runs"),
    (
        "compare.none_hint",
        "Hold ⌘ or Ctrl and click runs in the sidebar, or run several operators at once.",
    ),
    ("compare.of", "{} runs compared"),
    ("compare.findings", "Findings"),
    ("compare.operator", "Operator vs first"),
    ("compare.baseline", "(the first)"),
    ("compare.same", "same operator"),
    ("profile.distribution", "Latency distribution"),
    ("profile.verbs", "By verb"),
    ("plot.tail", "Share of operations slower than a latency"),
    (
        "plot.tail_buckets",
        "Share of operations slower than a latency (from the latency buckets)",
    ),
    ("plot.ops", "operations"),
    ("plot.throttled_secs", "throttled"),
    ("plot.error_secs", "errors"),
    ("plot.throttled_rate", "Throttled responses per second"),
    ("plot.errors_rate", "Failed operations per second"),
    ("plot.per_s", "per second"),
    ("plot.by_inflight", "Share throttled by requests in flight"),
    ("plot.throttled_share", "throttled"),
    ("plot.limit_interval", "limit interval"),
    ("plot.limit", "limit ≈ {}"),
    ("plot.convergence", "Change of the objective, pair by pair"),
    ("plot.pair_delta", "each pair"),
    ("plot.estimate", "estimate"),
    ("plot.interval", "95 % interval"),
    ("plot.noise", "noise floor (A/A)"),
    ("plot.evidence", "Evidence"),
    ("verb.ops", "{} ops"),
    ("compare.cdf", "Latency distribution"),
    ("compare.goodput", "Goodput over time"),
    // Findings by code
    ("finding.throttling", "Throttling"),
    ("finding.rate_limit", "Request rate limit"),
    ("finding.key_limit", "Per-key limit"),
    ("finding.bandwidth_cap", "Bandwidth cap"),
    ("finding.tail_stragglers", "Server stragglers"),
    ("finding.hung_requests", "Hung requests"),
    ("finding.small_ranges", "Small range reads"),
    ("finding.slow_multipart", "Slow multipart writes"),
    ("finding.repeated_reads", "Re-reading the same bytes"),
    ("finding.cold_connections", "Cold connections"),
    ("finding.client_bound", "Client-bound"),
    ("finding.errors", "Errors"),
    ("finding.invalid_run", "Invalid run"),
    ("open.filter", "oprobe runs and traces"),
    ("open.button", "Open"),
    ("runs.operator_file", "Operator files"),
    ("delete.confirm", "Delete"),
    ("delete.moved_one", "Moved run {} to the trash."),
    ("delete.moved_many", "Moved {} runs to the trash."),
    ("quit.title_run", "A run is in progress. Stop it and quit?"),
    (
        "quit.title_prove",
        "A prove is in progress. Stop it and quit?",
    ),
    (
        "quit.detail",
        "oprobe stops it and removes its test data before it quits.",
    ),
    ("quit.confirm", "Stop and Quit"),
    ("notify.run_done", "The run finished"),
    ("notify.run_failed", "The run failed"),
    ("notify.run_stopped", "The run stopped"),
    ("notify.prove_done", "The prove finished"),
    ("menu.preferences", "Preferences"),
    ("menu.about", "About oprobe"),
    ("menu.exit", "Exit"),
    ("menu.quit_plain", "Quit"),
    ("menu.quit", "Quit oprobe"),
    ("menu.hide", "Hide oprobe"),
    ("menu.hide_others", "Hide Others"),
    ("menu.show_all", "Show All"),
    ("menu.close", "Close Window"),
    ("menu.undo", "Undo"),
    ("menu.redo", "Redo"),
    ("menu.cut", "Cut"),
    ("menu.copy", "Copy"),
    ("menu.paste", "Paste"),
    ("menu.select_all", "Select All"),
    ("menu.full_screen", "Full Screen"),
    ("menu.minimize", "Minimize"),
    ("menu.zoom", "Zoom"),
    ("side.runs", "Runs"),
    ("side.resize", "Resize the sidebar"),
    ("app.undo", "Undo"),
    ("runs.review_plan", "Review Plan"),
    ("runs.configured", "Configured"),
    ("runs.recent", "Recent"),
    ("runs.files", "Files"),
    ("runs.pick_file", "Choose op.toml…"),
    ("runs.choose", "Choose"),
    (
        "plan.first",
        "A real target is planned first: what the run writes, reads and deletes, and the budgets that stop it.",
    ),
    ("plan.planning", "Planning…"),
    ("plan.title", "The plan"),
    (
        "plan.traffic",
        "About {} operations, {} requests, {} moved.",
    ),
    ("plan.read_only", "Read-only: nothing is written."),
    (
        "plan.writes",
        "Writes {} objects under {}, then deletes {}.",
    ),
    (
        "plan.eta",
        "Takes about {}, with the dataset and the cleanup.",
    ),
    ("plan.budgets", "Stops at {}."),
    (
        "plan.no_budgets",
        "No budget is set: the run stops at the end of its window.",
    ),
    ("plan.target", "{} · {} · {} workers · {} warm-up + {}"),
    ("plan.max_requests", "{} requests"),
    ("settings.config", "Config file"),
    (
        "settings.config_hint",
        "Configured operators ([operators.NAME]) and defaults are read from the config file.",
    ),
    ("settings.storage", "Your storage"),
    (
        "settings.storage_hint",
        "Describe your bucket in an op.toml file and enter its path in New Run; credentials come from the environment (AWS_*, GOOGLE_APPLICATION_CREDENTIALS, AZURE_STORAGE_*), or from ${VARIABLES} the file names.",
    ),
    ("settings.storage_docs", "How to describe it"),
    ("settings.preferences", "Preferences"),
    ("dialog.close", "Close"),
    ("about.version", "Version {}"),
    (
        "about.tagline",
        "A profiler for OpenDAL operators: profile, explain, prove.",
    ),
    ("about.website", "Website"),
    ("about.license", "Apache License 2.0"),
    (
        "operator.proves_edits",
        "Proves your changes, with the values you set.",
    ),
    (
        "operator.proves_finding",
        "Proves the remedy of {}, sized from this run.",
    ),
    (
        "operator.proves_top",
        "Proves the top finding's remedy, sized from this run.",
    ),
    // Brush, drill-down and honest charts (6.3 B5–B7, A4)
    ("plot.attempted", "attempted"),
    ("plot.throttled_responses", "throttled responses"),
    ("plot.too_few", "n={}, too few for {}"),
    ("plot.requests", "requests"),
    ("plot.retries", "retries"),
    ("chart.a11y_time", "{} over {}, {}–{}"),
    ("chart.a11y_empty", "{} over {}, no data"),
    ("chart.a11y_range", "{}, {} to {}"),
    ("heat.ops_per_cell", "{} ops per cell"),
    ("heat.a11y", "{} over {}, up to {} operations per cell"),
    ("timeline.requests", "Requests per second"),
    ("timeline.all_verbs", "{} · all verbs"),
    ("timeline.every_verb", "All verbs"),
    ("timeline.verb_filter", "Show the timeline of one verb"),
    ("range.title", "Selected {}–{}"),
    ("range.clear", "Clear"),
    (
        "range.hint",
        "Drag across a chart or the heatmap to select a time range; Escape or a double-click clears it.",
    ),
    (
        "range.counts",
        "{} operations · {} requests in the {} selected · failed per operation, throttled per request",
    ),
    (
        "range.sampled",
        "Rows were sampled ({} kept): these counts are estimates, each row weighted by the operations it stands for.",
    ),
    ("range.empty", "No operation started in this range."),
    ("profile.examples", "Show {} examples"),
    ("profile.hide_examples", "Hide examples"),
    ("drill.cell", "Slowest operations at {} · {}–{}"),
    ("drill.api", "Slowest {} requests"),
    ("drill.none", "The kept rows hold no operation here."),
    ("drill.close", "Close"),
    ("drill.pick", "Click an operation to see its requests."),
    (
        "drill.api_hint",
        "Click an API to list its slowest requests.",
    ),
    ("col.verb", "Verb"),
    ("col.latency", "Latency"),
    ("col.attempts", "Attempts"),
    ("col.at", "At"),
    ("col.total", "Total"),
    ("col.ttfb_only", "TTFB"),
    ("col.code", "Error code"),
    ("col.throttle_share", "Share of throttling"),
    ("sig.code", "error code"),
    ("sig.retry_after", "Retry-After"),
    ("sig.rtt", "rtt"),
    ("sig.min_rtt", "min rtt"),
    ("sig.retransmits", "retransmits"),
    ("sig.server_timing", "Server-Timing"),
];

static ZH: &[(&str, &str)] = &[
    ("plot.read_written", "读 + 写"),
    ("timeline.throughput", "吞吐"),
    ("timeline.problems", "每秒问题数"),
    ("conn.dialed_of", "预热期 {} · 测量窗口内 {}"),
    ("conn.dialed_window", "测量窗口内 {}"),
    ("conn.min_rtt", "最小往返时延（内核）"),
    (
        "conn.rtt_inflated",
        "平滑往返时延包含服务端处理时间和延迟确认；最小值才是网络本身的往返时延。",
    ),
    ("slice.cold", "首次读"),
    ("slice.warm", "重复读"),
    ("compare.p99", "p99 随时间"),
    ("compare.requests", "每秒请求"),
    ("compare.apis", "按 API 的请求"),
    ("compare.ttfb_p99", "首字节 p99"),
    ("compare.requests_of", "{} 个请求"),
    ("compare.requests_per_s", "请求/秒"),
    ("compare.failed", "失败操作"),
    ("compare.retries", "重试"),
    ("compare.throttled_count", "被限流的响应"),
    ("ev.affected_apis_count", "受影响的 API"),
    ("ev.p50_ms", "操作 p50"),
    ("ev.p99_ms", "操作 p99"),
    ("ev.replayed_p99_ms", "采用该修复后的 p99（回放）"),
    ("ev.typical_ttfb_ms", "典型首字节时间"),
    ("ev.median_request_ms", "请求中位数"),
    ("ev.slowest_request_ms", "最慢的请求"),
    ("ev.straggler_pct", "掉队请求"),
    ("ev.straggler_ttfb_ms", "掉队请求等待（p50）"),
    ("ev.time_cost_ratio", "占用的时间比例"),
    ("ev.throttled_request_ratio", "被限流的请求"),
    ("ev.throttled_op_ratio", "被限流的操作"),
    ("ev.error_ratio", "失败操作"),
    ("ev.p99_to_p50_ratio", "p99 / p50"),
    ("ev.overread_ratio", "多读"),
    ("ev.ttfb_pct", "等待首字节的时间"),
    ("ev.retry_pct", "重试退避的时间"),
    ("ev.queue_pct", "排队时间"),
    ("ev.connect_pct", "建连时间"),
    ("ev.body_pct", "接收响应体的时间"),
    ("ev.upload_pct", "上传时间"),
    ("ev.reuse_pct", "连接复用"),
    ("ev.dns_p50_ms", "DNS p50"),
    ("ev.tcp_p50_ms", "TCP 建连 p50"),
    ("ev.tls_p50_ms", "TLS 握手 p50"),
    ("ev.cold_requests_count", "新连接上的请求"),
    ("ev.client_cpu_ratio", "客户端 CPU"),
    ("ev.sched_lag_p99_ms", "调度延迟 p99"),
    ("ev.hot_key_share_ratio", "最热的键的占比"),
    ("ev.explains_ratio", "可解释的比例"),
    ("ev.stream_bytes_per_s", "单流吞吐"),
    ("ev.fitted_bytes_per_s", "拟合带宽"),
    ("ev.fitted_requests_per_s", "拟合请求速率"),
    ("ev.fitted_inflight_count", "拟合上限（在途）"),
    ("ev.ceiling_lo_inflight_count", "不被限流的在途数上限"),
    ("ev.ceiling_hi_inflight_count", "开始限流的在途数"),
    (
        "ev.peak_inflight_per_op_count",
        "每个操作的在途请求（峰值）",
    ),
    ("ev.requests_per_op_mean", "每个操作的请求数"),
    ("ev.parts_per_op_mean", "每个操作的分片数"),
    ("ev.inflight_per_op_mean", "每个操作的在途请求"),
    (
        "ev.extra_requests_per_1k_ops_mean",
        "每 1 000 个操作多出的请求",
    ),
    (
        "ev.saved_requests_per_1k_ops_mean",
        "每 1 000 次读省下的请求",
    ),
    ("ev.mean_request_bytes", "平均请求大小"),
    ("ev.tail_cut_deadline_ms", "Tail-cut 截止时间"),
    ("ev.tail_cut_blind_s", "Tail-cut 盲区"),
    ("ev.accepted_requests_per_s", "限流期间被接受的请求"),
    ("ev.monthly_usd", "预计月度账单"),
    ("ev.server_errors_count", "服务端错误"),
    ("ev.failed_ops_count", "失败操作"),
    ("ev.hung_requests_count", "挂起的请求"),
    ("ev.timeouts_count", "超时"),
    ("ev.throttled_responses_count", "被限流的响应"),
    ("ev.rate_limited_failures_count", "因速率限制失败"),
    ("ev.missed_arrivals_count", "错过的到达"),
    ("ev.window_s", "测量窗口"),
    ("app.dismiss", "关闭"),
    ("metric.p999", "p99.9"),
    ("metric.errors_of_ops", "失败操作"),
    ("metric.throttled_of_requests", "被限流请求"),
    (
        "metric.counts",
        "{} 次操作 · {} 个请求 · 测量 {} · 失败按操作计，限流按请求计",
    ),
    ("trust.title", "这些数字需谨慎解读"),
    (
        "trust.saturated",
        "客户端已饱和（CPU {} · 调度延迟 p99 {}）：服务端耗时里可能混入了客户端自身的延迟。",
    ),
    ("trust.partial", "这是一次不完整的运行：{}。"),
    ("trust.missed", "开环负载中有 {} 的请求未能按时发起。"),
    ("trust.few", "只测到 {} 次操作：分位数只是粗略值。"),
    ("profile.requests", "按 API 的请求"),
    ("profile.retries", "重试与失败"),
    ("profile.details", "详情"),
    ("profile.setup", "运行配置"),
    ("col.api", "API"),
    ("col.requests", "请求"),
    ("col.status", "状态码"),
    ("col.ttfb", "首字节 p50 / p99"),
    ("col.total_p50_p99", "总耗时 p50 / p99"),
    ("col.bytes", "下行 / 上行"),
    ("col.reused", "复用"),
    ("col.key", "键"),
    ("col.share", "占比"),
    ("col.throttled", "被限流"),
    ("col.p99", "p99"),
    ("retry.retries", "重试次数"),
    ("retry.backoff", "两次尝试之间的等待"),
    ("retry.throttled", "限流响应"),
    ("retry.after", "服务端要求的最长 Retry-After"),
    ("retry.errors", "其他错误"),
    ("retry.errors_split", "服务端 {} · 客户端 {} · 网络 {}"),
    ("retry.failed", "失败的操作"),
    ("retry.classes", "失败类别"),
    ("details.bytes", "请求的字节 → 实际传输"),
    ("bytes.read", "读取"),
    ("bytes.write", "写入"),
    ("bytes.wire", "线上（收 / 发）"),
    ("bytes.cache", "由缓存提供"),
    ("bytes.amplification", "放大"),
    (
        "bytes.amplification_of",
        "读 ×{} · 过读 ×{} · 写 ×{} · 每次操作 {} 个请求",
    ),
    ("details.hot_keys", "最热的键"),
    ("details.sizes", "对象大小"),
    ("details.prefixes", "前缀"),
    ("details.remotes", "服务端地址"),
    ("setup.target", "目标"),
    ("setup.endpoint", "Endpoint"),
    ("setup.workload", "负载"),
    ("setup.load", "压力"),
    ("setup.open_loop", "开环，每秒 {} 次操作"),
    ("setup.closed_loop", "闭环，{} 个 worker"),
    ("setup.duration", "时长"),
    ("setup.duration_of", "测量 {}（预热 {}）"),
    ("setup.dataset", "数据集"),
    ("setup.mix", "操作组成"),
    ("setup.seed", "随机种子"),
    ("setup.started", "开始时间"),
    ("setup.client", "客户端"),
    ("setup.kept", "保留的行数据"),
    ("setup.copy", "拷贝命令"),
    ("plot.inflight", "在途请求"),
    ("plot.peak", "每秒峰值"),
    (
        "compare.note",
        "每一列都只是一次运行，差异里包含运行之间的随机波动。要量化某个改动，请用“证明”。",
    ),
    ("menu.file", "文件"),
    ("menu.edit", "编辑"),
    ("menu.run", "运行"),
    ("menu.view", "显示"),
    ("menu.window", "窗口"),
    ("menu.help", "帮助"),
    ("menu.settings", "设置…"),
    ("menu.new_run", "新建运行…"),
    ("menu.open_file", "打开运行文件…"),
    ("menu.copy_id", "拷贝运行 ID"),
    ("menu.copy_command", "拷贝命令"),
    ("menu.reveal_mac", "在访达中显示"),
    ("menu.reveal_windows", "在资源管理器中显示"),
    ("menu.reveal", "在文件管理器中显示"),
    ("menu.run_again", "再次运行"),
    ("menu.stop", "停止"),
    ("menu.prove", "证明…"),
    ("menu.compare", "对比所选"),
    ("menu.compare_n", "对比 {} 次运行"),
    ("menu.compare_with", "与所选对比"),
    ("menu.delete", "删除…"),
    ("menu.open", "打开"),
    ("menu.hide_sidebar", "隐藏边栏"),
    ("menu.show_sidebar", "显示边栏"),
    ("menu.docs", "oprobe 文档"),
    ("menu.glossary", "术语表"),
    ("menu.primary", "主菜单"),
    ("side.running", "正在运行"),
    ("runs.blank", " "),
    ("settings.title", "设置"),
    ("settings.language", "语言"),
    ("settings.data", "运行记录存放在"),
    ("dialog.cancel", "取消"),
    ("dialog.new_run", "新建运行"),
    ("dialog.done", "完成"),
    ("delete.title", "删除运行"),
    ("delete.one", "删除运行 {}？"),
    ("delete.many", "删除 {} 次运行？"),
    ("delete.detail", "它们和试验运行会移到废纸篓，可以撤销。"),
    ("app.failed", "操作未完成，请查看日志"),
    ("app.lang_auto", "跟随系统"),
    ("app.loading", "加载中…"),
    ("app.running", "运行中"),
    ("nav.runs", "运行"),
    ("nav.profile", "剖析"),
    ("nav.operator", "Operator"),
    ("nav.compare", "对比"),
    ("runs.new", "新建运行"),
    ("runs.operator", "Operator"),
    (
        "runs.operator_hint",
        "s3://bucket/prefix、op.toml、配置中的名称，或 lab://demo",
    ),
    ("runs.start", "运行"),
    ("runs.stop", "停止"),
    ("runs.lab", "实验室"),
    ("runs.preset", "负载"),
    ("runs.duration", "时长"),
    ("runs.concurrency", "并发"),
    (
        "runs.several",
        "以空格分隔多个 operator，它们会在同一负载上依次运行，并在“对比”中并排打开。",
    ),
    ("runs.live", "实时"),
    ("runs.history", "历史 · {} 次运行"),
    ("runs.empty", "还没有运行"),
    ("runs.profile_lab", "剖析实验室…"),
    (
        "runs.empty_hint",
        "先剖析本地实验室：带故障的 S3，无需凭证，几秒出结果。",
    ),
    ("runs.open", "打开运行 {}"),
    ("runs.pick", "对比 {}"),
    ("runs.delete", "删除 {}"),
    ("runs.compare_picked", "对比已选的 {} 个"),
    ("runs.pick_hint", "点 + 把运行加入对比"),
    (
        "runs.need_operator",
        "请先输入 operator：URL、op.toml 文件或 lab://demo",
    ),
    ("runs.bad_concurrency", "并发须为不小于 1 的整数"),
    ("lab.demo", "2.5 % 的慢请求；在途请求超过 40 个时限流。"),
    ("lab.throttle", "在途请求超过 8 个时返回 503 SlowDown。"),
    ("lab.stragglers", "3 % 的请求要等 400 ms 才响应。"),
    ("lab.bandwidth", "所有响应共享 16 MiB/s。"),
    ("lab.flaky", "4 % 服务端错误，2 % 连接重置。"),
    ("lab.ratelimit", "超过 300 次请求/秒时返回 503 SlowDown。"),
    ("phase.idle", "空闲"),
    ("phase.preparing dataset", "准备数据集"),
    ("phase.warming up", "预热中"),
    ("phase.measuring", "测量中"),
    ("phase.verifying", "校验中"),
    ("phase.cleaning up", "清理中"),
    ("phase.cleanup", "正在清理测试数据"),
    ("phase.done", "完成"),
    ("metric.goodput", "有效吞吐"),
    ("metric.bandwidth", "带宽"),
    ("metric.p50", "p50"),
    ("metric.p99", "p99"),
    ("metric.errors", "错误"),
    ("metric.throttled", "被限流"),
    ("verdict.ok", "正常"),
    ("verdict.review", "值得一看"),
    ("verdict.attention", "需关注"),
    ("verdict.critical", "严重"),
    ("severity.info", "提示"),
    ("severity.warning", "警告"),
    ("severity.critical", "严重"),
    ("stage.queue", "排队"),
    ("stage.cache", "缓存"),
    ("stage.connect", "建连"),
    ("stage.upload", "上传"),
    ("stage.server", "服务端 (TTFB)"),
    ("stage.download", "下载"),
    ("stage.retry", "重试退避"),
    ("stage.client", "客户端"),
    ("req.queue", "排队"),
    ("req.connect", "建连"),
    ("req.send", "发送"),
    ("req.ttfb", "首字节"),
    ("req.body", "响应体"),
    ("req.stall", "停顿"),
    ("req.retry", "重试退避"),
    ("req.client", "客户端"),
    ("profile.none", "没有打开的运行"),
    ("profile.none_hint", "在“运行”中打开一次运行，或新建一次。"),
    ("profile.to_operator", "在 Operator 中打开"),
    ("profile.pick", "对比…"),
    ("profile.time", "时间花在哪里"),
    ("profile.findings", "发现 · {}"),
    (
        "profile.no_findings",
        "无需修复：这次运行没有建议的层改动。",
    ),
    ("profile.remedy", "改法"),
    ("profile.advice", "建议"),
    ("profile.try_remedy", "证明这个改法"),
    ("profile.try_remedy_of", "证明 {} 的改法"),
    ("profile.slowest", "最慢的操作 · 请求瀑布图"),
    ("profile.hover", "指向一个请求以查看各阶段"),
    ("profile.timeline", "时间线"),
    ("profile.latency", "延迟"),
    ("profile.heatmap", "请求延迟随时间分布"),
    ("profile.connections", "连接"),
    ("profile.warnings", "警告"),
    ("conn.dialed", "新建连接数"),
    ("conn.reuse", "复用连接的请求"),
    ("conn.protocols", "协议"),
    ("conn.peak", "在途请求峰值"),
    ("conn.dial_failures", "建连失败"),
    ("conn.proxy", "代理"),
    ("conn.proxy_connect", "代理 CONNECT"),
    ("conn.rtt", "平滑往返时延（内核）"),
    ("conn.retransmitting", "发生重传的连接"),
    ("conn.retransmitting_of", "{} / {} · {} 个分段"),
    ("conn.limited", "发送受限于"),
    ("conn.limited_of", "接收窗口 {} · 发送缓冲区 {} 的时间"),
    ("conn.delivery", "投递速率 p50"),
    ("conn.tls_handshakes", "TLS 握手"),
    (
        "conn.tls_handshakes_of",
        "{} 次 · {} 复用会话 · {} 次 HelloRetryRequest",
    ),
    ("conn.cold_start", "冷启动"),
    ("conn.cold_start_of", "预热期建连：建连 {} · 首批操作 {}"),
    ("write.path", "写入"),
    ("write.part_size", "分片大小 {}"),
    ("write.asked", "请求的是 {}"),
    ("write.service", "服务支持 {}–{}"),
    ("write.parts", "{} 个分片，平均 {}"),
    ("details.cold_warm", "键的首次读 / 重复读"),
    ("col.errors", "错误"),
    ("verb.rates", "{}/s · {} MiB/s · {} 请求/操作"),
    ("verb.throttled", "被限流 {}"),
    ("plot.hot_keys_throttling", "最热的键 · 占全部限流"),
    ("plot.hot_keys_requests", "最热的键 · 占全部请求"),
    ("plot.new_connections", "新建连接"),
    ("plot.cpu_cores", "客户端 CPU（核）"),
    ("plot.failed_ops", "失败的操作"),
    ("profile.await_tree", "在途时的异步调用栈"),
    ("sig.stream_wait", "h2 等待流"),
    ("sig.flow_stall", "h2 流控停顿"),
    ("sig.streams", "h2 打开的流"),
    ("sig.upload_acked", "上传被确认于"),
    ("sig.send_buffered", "交给内核于"),
    ("sig.cache_status", "CDN 缓存"),
    ("sig.region", "区域"),
    ("operator.none", "还没有 operator"),
    (
        "operator.none_hint",
        "请先打开一次运行：要修改和验证的就是它的 operator。",
    ),
    ("operator.of_run", "运行 {} 的 operator"),
    ("operator.remedies", "各项发现的改法"),
    ("operator.no_remedies", "这次运行的发现都不涉及层的改动。"),
    ("operator.apply", "证明"),
    ("operator.section.layers", "层"),
    ("operator.section.read", "读取"),
    ("operator.section.write", "写入"),
    ("operator.section.http", "HTTP 客户端"),
    ("operator.default", "默认"),
    ("operator.changed", "已修改"),
    ("operator.remedy_of", "改法：{}"),
    ("operator.changes", "改动"),
    (
        "operator.no_changes",
        "还没有改动：在上方切换一个层或修改一个值。",
    ),
    ("operator.diff", "改动的 Rust 与 op.toml 形式"),
    ("operator.copy_rust", "复制 Rust"),
    ("operator.copy_toml", "复制 TOML"),
    ("operator.reset", "重置"),
    ("operator.prove", "证明"),
    (
        "operator.prove_hint",
        "交替进行的 A/B 试验会判断改动是否改善你的目标。发现的改法会按这次运行确定取值，与 `oprobe prove RUN CODE` 相同；你手动修改的值按你设定的值证明。",
    ),
    ("operator.objective", "目标"),
    ("operator.budget", "预算"),
    ("operator.prove_run", "证明"),
    ("operator.trials", "已完成 {} 次试验"),
    ("operator.pair", "第 {} 对"),
    ("operator.pairs_count", "{} 对试验"),
    (
        "operator.secondary",
        "次要指标，不决定结论（占比为百分点差，pp）：{}",
    ),
    ("operator.proven_diff", "已证明的改动"),
    ("operator.stopped", "已停止。"),
    ("prove.improved", "有改善"),
    ("prove.worse", "变差"),
    ("prove.inconclusive", "无定论"),
    ("compare.pick", "要对比的运行"),
    ("compare.none", "请选择两次或更多运行"),
    (
        "compare.none_hint",
        "在边栏中按住 ⌘ 或 Ctrl 点选运行，或一次运行多个 operator。",
    ),
    ("compare.of", "对比 {} 次运行"),
    ("compare.findings", "发现"),
    ("compare.operator", "与第一个的 operator 差异"),
    ("compare.baseline", "（第一个）"),
    ("compare.same", "operator 相同"),
    ("profile.distribution", "延迟分布"),
    ("profile.verbs", "按操作类型"),
    ("plot.tail", "慢于该延迟的操作占比"),
    ("plot.tail_buckets", "慢于该延迟的操作占比（来自延迟分桶）"),
    ("plot.ops", "操作"),
    ("plot.throttled_secs", "被限流"),
    ("plot.error_secs", "出错"),
    ("plot.throttled_rate", "每秒被限流的响应"),
    ("plot.errors_rate", "每秒失败的操作"),
    ("plot.per_s", "每秒"),
    ("plot.by_inflight", "按在途请求数的被限流比例"),
    ("plot.throttled_share", "被限流"),
    ("plot.limit_interval", "上限区间"),
    ("plot.limit", "上限 ≈ {}"),
    ("plot.convergence", "目标指标的变化（逐对）"),
    ("plot.pair_delta", "每一对"),
    ("plot.estimate", "估计"),
    ("plot.interval", "95 % 区间"),
    ("plot.noise", "噪声底线（A/A）"),
    ("plot.evidence", "证据"),
    ("verb.ops", "{} 次"),
    ("compare.cdf", "延迟分布"),
    ("compare.goodput", "吞吐量随时间"),
    ("finding.throttling", "限流"),
    ("finding.rate_limit", "请求速率限制"),
    ("finding.key_limit", "单键限制"),
    ("finding.bandwidth_cap", "带宽上限"),
    ("finding.tail_stragglers", "服务端慢请求"),
    ("finding.hung_requests", "挂起的请求"),
    ("finding.small_ranges", "过小的范围读"),
    ("finding.slow_multipart", "分片写入缓慢"),
    ("finding.repeated_reads", "重复读取相同数据"),
    ("finding.cold_connections", "冷连接"),
    ("finding.client_bound", "受限于客户端"),
    ("finding.errors", "错误"),
    ("finding.invalid_run", "无效运行"),
    ("open.filter", "oprobe 运行与追踪"),
    ("open.button", "打开"),
    ("runs.operator_file", "Operator 文件"),
    ("delete.confirm", "删除"),
    ("delete.moved_one", "已将运行 {} 移到废纸篓。"),
    ("delete.moved_many", "已将 {} 次运行移到废纸篓。"),
    ("quit.title_run", "有运行正在进行。要停止它并退出吗？"),
    ("quit.title_prove", "有证明正在进行。要停止它并退出吗？"),
    ("quit.detail", "oprobe 会先停止它并清理测试数据，然后退出。"),
    ("quit.confirm", "停止并退出"),
    ("notify.run_done", "运行已完成"),
    ("notify.run_failed", "运行失败"),
    ("notify.run_stopped", "运行已停止"),
    ("notify.prove_done", "证明已完成"),
    ("menu.preferences", "偏好设置"),
    ("menu.about", "关于 oprobe"),
    ("menu.exit", "退出"),
    ("menu.quit_plain", "退出"),
    ("menu.quit", "退出 oprobe"),
    ("menu.hide", "隐藏 oprobe"),
    ("menu.hide_others", "隐藏其他"),
    ("menu.show_all", "全部显示"),
    ("menu.close", "关闭窗口"),
    ("menu.undo", "撤销"),
    ("menu.redo", "重做"),
    ("menu.cut", "剪切"),
    ("menu.copy", "拷贝"),
    ("menu.paste", "粘贴"),
    ("menu.select_all", "全选"),
    ("menu.full_screen", "全屏"),
    ("menu.minimize", "最小化"),
    ("menu.zoom", "缩放"),
    ("side.runs", "运行"),
    ("side.resize", "调整边栏宽度"),
    ("app.undo", "撤销"),
    ("runs.review_plan", "查看计划"),
    ("runs.configured", "已配置"),
    ("runs.recent", "最近使用"),
    ("runs.files", "文件"),
    ("runs.pick_file", "选择 op.toml…"),
    ("runs.choose", "选择"),
    (
        "plan.first",
        "真实目标会先生成计划：运行会写入、读取和删除什么，以及会让它停止的预算。",
    ),
    ("plan.planning", "正在生成计划…"),
    ("plan.title", "计划"),
    ("plan.traffic", "约 {} 次操作、{} 个请求、传输 {}。"),
    ("plan.read_only", "只读：不写入任何数据。"),
    ("plan.writes", "写入 {} 个对象（位于 {}），之后删除 {} 个。"),
    ("plan.eta", "约需 {}（含数据准备与清理）。"),
    ("plan.budgets", "达到 {} 时停止。"),
    ("plan.no_budgets", "未设置预算：运行在测量窗口结束时停止。"),
    ("plan.target", "{} · {} · {} 并发 · 预热 {} + {}"),
    ("plan.max_requests", "{} 个请求"),
    ("settings.config", "配置文件"),
    (
        "settings.config_hint",
        "已配置的 operator（[operators.NAME]）和默认值从配置文件读取。",
    ),
    ("settings.storage", "你的存储"),
    (
        "settings.storage_hint",
        "在 op.toml 文件中描述你的存储桶，并在“新建运行”中输入它的路径；凭证来自环境变量（AWS_*、GOOGLE_APPLICATION_CREDENTIALS、AZURE_STORAGE_*），或文件中引用的 ${变量}。",
    ),
    ("settings.storage_docs", "如何描述"),
    ("settings.preferences", "偏好设置"),
    ("dialog.close", "关闭"),
    ("about.version", "版本 {}"),
    (
        "about.tagline",
        "OpenDAL operator 的性能分析器：分析、解释、证明。",
    ),
    ("about.website", "网站"),
    ("about.license", "Apache License 2.0"),
    ("operator.proves_edits", "证明你的改动，使用你设定的值。"),
    (
        "operator.proves_finding",
        "证明 {} 的改法，取值按这次运行确定。",
    ),
    (
        "operator.proves_top",
        "证明首要发现的改法，取值按这次运行确定。",
    ),
    // 刷选、下钻与如实的图表（6.3 B5–B7、A4）
    ("plot.attempted", "尝试"),
    ("plot.throttled_responses", "被限流的响应"),
    ("plot.too_few", "n={}，样本太少，无法估计 {}"),
    ("plot.requests", "请求"),
    ("plot.retries", "重试"),
    ("chart.a11y_time", "{}，共 {}，{}–{}"),
    ("chart.a11y_empty", "{}，共 {}，无数据"),
    ("chart.a11y_range", "{}，{} 至 {}"),
    ("heat.ops_per_cell", "每格 {} 次操作"),
    ("heat.a11y", "{}，共 {}，每格最多 {} 次操作"),
    ("timeline.requests", "每秒请求"),
    ("timeline.all_verbs", "{} · 全部操作类型"),
    ("timeline.every_verb", "全部操作类型"),
    ("timeline.verb_filter", "只看一种操作类型的时间线"),
    ("range.title", "已选 {}–{}"),
    ("range.clear", "清除"),
    (
        "range.hint",
        "在图表或热力图上拖动可选择时间范围；按 Esc 或双击清除。",
    ),
    (
        "range.counts",
        "{} 次操作 · {} 个请求 · 所选 {} · 失败按操作计，限流按请求计",
    ),
    (
        "range.sampled",
        "记录行经过抽样（保留 {}）：这些计数是估计值，每行按其代表的操作数加权。",
    ),
    ("range.empty", "这段时间内没有开始的操作。"),
    ("profile.examples", "显示 {} 个示例"),
    ("profile.hide_examples", "隐藏示例"),
    ("drill.cell", "{} · {}–{} 中最慢的操作"),
    ("drill.api", "最慢的 {} 请求"),
    ("drill.none", "保留的记录行里这里没有操作。"),
    ("drill.close", "关闭"),
    ("drill.pick", "点击一个操作查看它的请求。"),
    ("drill.api_hint", "点击一个 API，列出它最慢的请求。"),
    ("col.verb", "操作类型"),
    ("col.latency", "延迟"),
    ("col.attempts", "尝试次数"),
    ("col.at", "时间"),
    ("col.total", "总耗时"),
    ("col.ttfb_only", "首字节"),
    ("col.code", "错误码"),
    ("col.throttle_share", "占全部限流"),
    ("sig.code", "错误码"),
    ("sig.retry_after", "Retry-After"),
    ("sig.rtt", "rtt"),
    ("sig.min_rtt", "最小 rtt"),
    ("sig.retransmits", "重传"),
    ("sig.server_timing", "Server-Timing"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_languages_have_every_key_once() {
        let keys = |t: &'static [(&'static str, &'static str)]| {
            t.iter().map(|(k, _)| *k).collect::<Vec<_>>()
        };
        let (en, zh) = (keys(EN), keys(ZH));
        let mut sorted = en.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), en.len(), "a key twice in EN");
        let missing: Vec<_> = en.iter().filter(|k| !zh.contains(k)).collect();
        let extra: Vec<_> = zh.iter().filter(|k| !en.contains(k)).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "zh missing {missing:?}, extra {extra:?}"
        );
        // The placeholders agree.
        for (k, v) in EN {
            let z = find(Lang::Zh, k).unwrap();
            assert_eq!(v.matches("{}").count(), z.matches("{}").count(), "{k}");
        }
    }

    #[test]
    fn every_key_the_views_use_is_in_the_catalog() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        let mut sources = Vec::new();
        for d in [dir.clone(), dir.join("views")] {
            for e in std::fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.extension().is_some_and(|x| x == "rs") {
                    sources.push(std::fs::read_to_string(p).unwrap());
                }
            }
        }
        let mut used = Vec::new();
        for src in &sources {
            for call in ["tr(\"", "fill(\"", "trf(\"", "t(\""] {
                for (i, _) in src.match_indices(call) {
                    // A call of this name, not the end of another (`insert("…`).
                    if src[..i]
                        .chars()
                        .last()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_')
                    {
                        continue;
                    }
                    let part = &src[i + call.len()..];
                    if let Some(k) = part.split('"').next()
                        && k.contains('.')
                        && !k.contains(' ')
                    {
                        used.push(k.to_owned());
                    }
                }
            }
        }
        assert!(used.len() > 50, "{}", used.len());
        let missing: Vec<_> = used
            .iter()
            .filter(|k| find(Lang::En, k).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "keys missing from the catalog: {missing:?}"
        );
    }

    #[test]
    fn finding_titles_are_the_catalogs_codes() {
        // A title for every code the findings engine documents, and none for a code it no
        // longer has (the 5.x vestiges).
        let codes = crate::analysis::catalog::codes();
        let titled: Vec<&str> = EN
            .iter()
            .filter_map(|(k, _)| k.strip_prefix("finding."))
            .collect();
        let dead: Vec<_> = titled.iter().filter(|t| !codes.contains(t)).collect();
        let missing: Vec<_> = codes.iter().filter(|c| !titled.contains(c)).collect();
        assert!(
            dead.is_empty(),
            "titles of codes not in the catalog: {dead:?}"
        );
        assert!(missing.is_empty(), "codes without a title: {missing:?}");
    }

    #[test]
    fn languages_parse_and_fill() {
        assert_eq!(Lang::parse("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(Lang::parse("zh-Hans"), Some(Lang::Zh));
        assert_eq!(Lang::parse("en-US"), Some(Lang::En));
        assert_eq!(Lang::parse("C"), None);
        set(Lang::Zh);
        assert_eq!(trf("runs.history", &[&3]), "历史 · 3 次运行");
        assert_eq!(finding_title("throttling"), "限流");
        set(Lang::En);
        assert_eq!(finding_title("some_new_code"), "some new code");
    }
}
