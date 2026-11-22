//! JSON Schemas (2020-12) of every machine-facing output, generated from the Rust types with
//! `schemars`: `oprobe schema [NAME]`, the MCP `oprobe://schema/{name}` resource and the
//! checked-in copies under `schemas/` (kept in sync by `tests/agent.rs`; regenerate with
//! `OPROBE_BLESS=1 cargo test --test agent`).

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::ErrorBody;

/// Base of every schema `$id`.
pub const ID_BASE: &str = "https://github.com/hxddh/oprobe/schemas/5/";

/// Whether a schema describes something oprobe writes (outputs) or reads (inputs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// oprobe writes it (command results, reports, events, run files).
    Output,
    /// oprobe reads it (the config file, MCP tool inputs).
    Input,
}

/// One registered schema.
pub struct Entry {
    pub name: &'static str,
    pub description: &'static str,
    pub direction: Direction,
    build: fn(SchemaSettings) -> Schema,
}

fn gen_for<T: JsonSchema>(s: SchemaSettings) -> Schema {
    s.into_generator().into_root_schema_for::<T>()
}

/// A command result: `T` in the common envelope (`next`, `warnings`; design v4 §8), titled
/// after `T`.
fn gen_cmd<T: JsonSchema>(s: SchemaSettings) -> Schema {
    let mut schema = s
        .into_generator()
        .into_root_schema_for::<crate::output::Envelope<T>>();
    schema.insert("title".into(), Value::String(T::schema_name().into_owned()));
    schema
}

/// The `{"type":"error","error":{…}}` document printed on failures in json/ndjson modes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ErrorEnvelope {
    pub r#type: ErrorTag,
    pub error: ErrorBody,
}

/// Always `error`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorTag {
    Error,
}

/// One block of a `.oprobe` run file (`oprobe.run/5`: a zstd stream of frames
/// `kind:u8 len:u32le payload`, the payload JSON): the `header`, columnar `segment`s of rows and
/// per-second `interval`s while the run goes, the `report` last. Readers skip unknown kinds.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum RunFileBlock {
    Header(crate::store::runfile::RunHeader),
    /// Columnar operation and request rows of one time slice (interned strings, `u32`
    /// microseconds after `base_us`; see `oprobe_core::store::runfile::Segment`).
    Segment(serde_json::Map<String, serde_json::Value>),
    Interval(crate::store::runfile::IntervalBlock),
    Report(crate::model::Report),
}

macro_rules! entry {
    ($name:literal, $dir:ident, $ty:ty, $desc:literal) => {
        Entry {
            name: $name,
            description: $desc,
            direction: Direction::$dir,
            build: gen_for::<$ty>,
        }
    };
    // A command result: wrapped in the common envelope.
    (cmd $name:literal, $ty:ty, $desc:literal) => {
        Entry {
            name: $name,
            description: $desc,
            direction: Direction::Output,
            build: gen_cmd::<$ty>,
        }
    };
}

/// Every schema, in a stable order.
pub fn entries() -> Vec<Entry> {
    use crate::app;
    vec![
        entry!(
            "report",
            Output,
            crate::model::Report,
            "Report v5 (oprobe.report/5): the result of one run"
        ),
        entry!(
            "events",
            Output,
            crate::events::Event,
            "One line of the event stream (oprobe.events/1, -o ndjson)"
        ),
        entry!(
            "error",
            Output,
            ErrorEnvelope,
            "The error document of json/ndjson modes"
        ),
        entry!(
            "run-file",
            Output,
            RunFileBlock,
            "One block of a .oprobe run file (oprobe.run/5)"
        ),
        entry!(
            "run-summary",
            Output,
            crate::store::RunSummary,
            "Sidecar summary of a stored run (runs/<id>.json)"
        ),
        entry!(
            "config",
            Input,
            crate::store::ConfigFile,
            "The config file (config.toml): [defaults] and [targets.NAME]"
        ),
        entry!(cmd "run", app::RunOutput, "Result of `oprobe run`"),
        entry!(
            cmd "plan",
            app::run::PlanOutput,
            "Result of `oprobe run --dry-run` / `tune --dry-run`"
        ),
        entry!(cmd "tune", app::TuneOutput, "Result of `oprobe tune`"),
        entry!(
            cmd "compare",
            crate::analysis::compare::Comparison,
            "Result of `oprobe compare`"
        ),
        entry!(
            cmd "verify",
            app::verify::VerifyResult,
            "Result of `oprobe verify`: confirmed, refuted or inconclusive per finding"
        ),
        entry!(
            cmd "fix",
            app::fix::FixOutput,
            "Result of `oprobe fix`: diagnose, tune, verify; a recommendation and a profile patch"
        ),
        entry!(
            cmd "baseline-calibrate",
            app::compare::CalibrateOutput,
            "Result of `oprobe baseline calibrate`: the target's noise floor"
        ),
        entry!(
            "noise-floors",
            Output,
            crate::store::baselines::NoiseFloors,
            "Noise floors learned per target and workload (<data>/noise.toml)"
        ),
        entry!(
            cmd "analyze",
            app::analyze::AnalyzeOutput,
            "Result of `oprobe analyze`"
        ),
        entry!(
            cmd "watch",
            app::watch::WatchOutput,
            "Result of `oprobe watch`"
        ),
        entry!(
            cmd "matrix",
            app::matrix::MatrixOutput,
            "Result of `oprobe matrix`"
        ),
        entry!(
            cmd "matrix-plan",
            app::matrix::MatrixPlanOutput,
            "Result of `oprobe matrix --dry-run`"
        ),
        entry!(
            cmd "compat",
            app::compat::CompatOutput,
            "Result of `oprobe compat`"
        ),
        entry!(
            cmd "compat-plan",
            app::compat::CompatPlan,
            "Result of `oprobe compat --dry-run`"
        ),
        entry!(
            cmd "runs-export",
            app::export::ExportOutput,
            "Result of `oprobe runs export`"
        ),
        entry!(
            cmd "run-repeat",
            app::repeat::RepeatOutput,
            "Result of `oprobe run --repeat N` / `--baseline NAME`"
        ),
        entry!(
            cmd "baseline-show",
            app::compare::BaselineShow,
            "Result of `oprobe baseline show`"
        ),
        entry!(
            cmd "baseline-list",
            app::compare::BaselineList,
            "Result of `oprobe baseline list`"
        ),
        entry!(
            cmd "demo",
            app::demo::DemoOutput,
            "Result of `oprobe demo`"
        ),
        entry!(
            cmd "self-update",
            crate::cli::self_update::UpdateOutput,
            "Result of `oprobe self-update`"
        ),
        entry!(
            cmd "doctor",
            app::doctor::DoctorOutput,
            "Result of `oprobe doctor`"
        ),
        entry!(
            cmd "probe",
            crate::net::probe::ProbeReport,
            "Result of `oprobe probe`"
        ),
        entry!(
            cmd "cleanup",
            app::misc::CleanupOutput,
            "Result of `oprobe cleanup`"
        ),
        entry!(
            cmd "lab",
            app::misc::LabOutput,
            "What `oprobe lab` serves"
        ),
        entry!(
            cmd "runs-list",
            app::runs::RunsList,
            "Result of `oprobe runs list`"
        ),
        entry!(
            cmd "runs-show",
            app::runs::RunShow,
            "Result of `oprobe runs show`"
        ),
        entry!(
            cmd "runs-rm",
            app::runs::RemovedRuns,
            "Result of `oprobe runs rm` / `runs gc`"
        ),
        entry!(
            cmd "target-list",
            app::settings::TargetList,
            "Result of `oprobe target list`"
        ),
        entry!(
            cmd "target-show",
            app::settings::TargetShow,
            "Result of `oprobe target show`"
        ),
        entry!(
            cmd "target-discover",
            app::discover::Discovery,
            "Result of `oprobe target discover` (MCP discover_targets): credential sources by name"
        ),
        entry!(
            cmd "target-sample",
            app::sample::ObjectSample,
            "Result of `oprobe target sample` (MCP sample_objects): prefixes, counts and sizes"
        ),
        entry!(
            cmd "config-show",
            app::settings::ConfigShow,
            "Result of `oprobe config show`"
        ),
        entry!(
            cmd "config-path",
            app::settings::ConfigPaths,
            "Result of `oprobe config path`"
        ),
        entry!(
            cmd "capabilities",
            super::capabilities::Capabilities,
            "Result of `oprobe capabilities`"
        ),
        entry!(
            cmd "explain",
            super::explain::ExplainOutput,
            "Result of `oprobe explain` (a run or a finding code)"
        ),
        entry!(
            cmd "schema-list",
            SchemaList,
            "Result of `oprobe schema` without a name"
        ),
        entry!(
            "attribution",
            Output,
            crate::analysis::outliers::Attribution,
            "Outlier attribution (MCP analyze_run, explain)"
        ),
        entry!(
            "selection",
            Input,
            crate::analysis::outliers::Selection,
            "Which requests to attribute (MCP analyze_run)"
        ),
        entry!(
            "run-spec",
            Output,
            crate::model::RunSpec,
            "The replayable definition of a run (report.spec; `oprobe run --from RUN`)"
        ),
        entry!(
            "overrides",
            Input,
            crate::model::Overrides,
            "A change to a run: `--set key=value` pairs by section (verifications, recommendations)"
        ),
        entry!(
            "capacity",
            Output,
            crate::analysis::capacity::Capacity,
            "The capacity model (report.capacity): in-flight ceiling and per-op request shape"
        ),
        entry!(
            "capacity-history",
            Output,
            crate::analysis::capacity::History,
            "Ceilings remembered per target and workload (<data>/capacity.toml)"
        ),
        entry!(
            "verification",
            Output,
            crate::analysis::verification::Verification,
            "How to test a finding's advice: overrides, commands and expectations"
        ),
        entry!(
            "recommendation",
            Output,
            crate::tune::recommend::Recommendation,
            "A tuned configuration with its deltas (tune, fix, MCP recommend_config)"
        ),
        entry!(
            "next-step",
            Output,
            crate::output::NextStep,
            "One suggested next step of a command result (CLI command and MCP call)"
        ),
        entry!(
            "target-id",
            Output,
            crate::store::TargetId,
            "The normalised identity of a storage target"
        ),
        entry!(
            "defaults",
            Output,
            crate::config::BuiltinDefaults,
            "Every built-in default (config::defaults)"
        ),
        entry!(
            cmd "worker",
            crate::distributed::WorkerOutput,
            "What `oprobe worker` serves (design v5 §8)"
        ),
        entry!(
            "client-hello",
            Output,
            crate::clients::protocol::HelloRequest,
            "oprobe client protocol (`--client exec:CMD`): the first line oprobe sends"
        ),
        entry!(
            "client-hello-reply",
            Input,
            crate::clients::protocol::HelloReply,
            "oprobe client protocol: the adapter's answer to hello"
        ),
        entry!(
            "client-op",
            Output,
            crate::clients::protocol::WireOp,
            "oprobe client protocol: one operation oprobe asks the adapter to run"
        ),
        entry!(
            "client-result",
            Input,
            crate::clients::protocol::WireResult,
            "oprobe client protocol: the adapter's result of one operation"
        ),
        // MCP tool results (design v5 §6: tools/list refers to these by name).
        entry!(
            "mcp-plan",
            Output,
            super::tools::PlanOut,
            "MCP plan_run: the dry-run plan, budgets and whether the server accepts it"
        ),
        entry!(
            "mcp-run-status",
            Output,
            super::tools::RunStatusOut,
            "MCP start_run, start_tune, verify_finding, recommend_config, get_run, cancel_run: a run or job"
        ),
        entry!(
            "mcp-runs",
            Output,
            super::tools::ListRunsOut,
            "MCP list_runs: one page of stored runs and the jobs this server started"
        ),
        entry!(
            "mcp-ask",
            Output,
            super::ask::AskOut,
            "MCP ask_run: answers citing report fields"
        ),
        entry!(
            "mcp-analyze",
            Output,
            super::tools::AnalyzeOut,
            "MCP analyze_run: outlier attribution of selected requests"
        ),
        entry!(
            "mcp-estimate",
            Output,
            super::tools::EstimateOut,
            "MCP estimate_cost: request cost at list prices"
        ),
        entry!(
            "mcp-targets",
            Output,
            super::tools::TargetsOut,
            "MCP list_targets: configured targets and the server's rails"
        ),
        entry!(
            "mcp-save-target",
            Output,
            super::tools::SaveTargetOut,
            "MCP save_target: the target saved"
        ),
        entry!(
            "mcp-apply-profile",
            Output,
            super::tools::ApplyProfileOut,
            "MCP apply_profile: the profile file written and its diff"
        ),
    ]
}

/// The names, in registry order.
pub fn names() -> Vec<&'static str> {
    entries().iter().map(|e| e.name).collect()
}

/// The `$id` of a schema.
pub fn id_of(name: &str) -> String {
    format!("{ID_BASE}{name}.json")
}

/// Generate a schema with its `$id`, `title` and `description`.
pub fn generate(e: &Entry) -> Value {
    let settings = match e.direction {
        Direction::Output => SchemaSettings::draft2020_12().for_serialize(),
        Direction::Input => SchemaSettings::draft2020_12().for_deserialize(),
    };
    let mut s = (e.build)(settings);
    let inner_title = s.get("title").cloned();
    s.insert("$id".into(), Value::String(id_of(e.name)));
    s.insert("title".into(), Value::String(format!("oprobe {}", e.name)));
    s.insert("description".into(), Value::String(e.description.into()));
    if let Some(t) = inner_title {
        s.insert("x-rust-type".into(), t);
    }
    s.to_value()
}

/// The schema called `name`.
pub fn schema(name: &str) -> Option<Value> {
    entries().iter().find(|e| e.name == name).map(generate)
}

/// A schema for a type, without registry metadata (MCP tool input/output schemas).
pub fn schema_of<T: JsonSchema>(direction: Direction) -> serde_json::Map<String, Value> {
    let settings = match direction {
        Direction::Output => SchemaSettings::draft2020_12().for_serialize(),
        Direction::Input => SchemaSettings::draft2020_12().for_deserialize(),
    };
    match gen_for::<T>(settings).to_value() {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    }
}

/// One entry of `oprobe schema` without a name.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SchemaInfo {
    pub name: String,
    #[serde(rename = "$id")]
    pub id: String,
    pub direction: Direction,
    pub description: String,
}

/// Result of `oprobe schema` without a name.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SchemaList {
    pub schemas: Vec<SchemaInfo>,
}

impl SchemaList {
    pub fn current() -> Self {
        Self {
            schemas: entries()
                .iter()
                .map(|e| SchemaInfo {
                    name: e.name.to_owned(),
                    id: id_of(e.name),
                    direction: e.direction,
                    description: e.description.to_owned(),
                })
                .collect(),
        }
    }
}

impl crate::output::CommandOutput for SchemaList {
    fn text(&self) -> String {
        let w = self.schemas.iter().map(|s| s.name.len()).max().unwrap_or(0);
        let mut o = String::new();
        for s in &self.schemas {
            o += &format!("{:<w$}  {}\n", s.name, s.description);
        }
        o += &crate::report::style::dim(
            "print one with `oprobe schema NAME` (JSON Schema 2020-12)\n",
        );
        o
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_are_integers_in_every_schema() {
        fn walk(v: &serde_json::Value, path: &str, bad: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(m) => {
                    if let Some(serde_json::Value::Object(props)) = m.get("properties") {
                        for (k, p) in props {
                            let number = p.get("type").is_some_and(|t| {
                                t == "number"
                                    || t.as_array()
                                        .is_some_and(|a| a.iter().any(|x| x == "number"))
                            });
                            let map_of_numbers = p
                                .get("additionalProperties")
                                .and_then(|a| a.get("type"))
                                .is_some_and(|t| t == "number");
                            if k.ends_with("_count") && (number || map_of_numbers) {
                                bad.push(format!("{path}.{k}"));
                            }
                        }
                    }
                    for (k, x) in m {
                        walk(x, &format!("{path}/{k}"), bad);
                    }
                }
                serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, path, bad)),
                _ => {}
            }
        }
        let mut bad = Vec::new();
        for e in super::entries() {
            if let Some(s) = super::schema(e.name) {
                walk(&s, e.name, &mut bad);
            }
        }
        assert!(bad.is_empty(), "fractional `_count` fields: {bad:#?}");
    }

    use super::*;

    #[test]
    fn every_schema_generates_with_an_id() {
        let mut seen = std::collections::BTreeSet::new();
        for e in entries() {
            assert!(seen.insert(e.name), "duplicate {}", e.name);
            let v = generate(&e);
            assert_eq!(v["$id"], id_of(e.name));
            assert_eq!(
                v["$schema"], "https://json-schema.org/draft/2020-12/schema",
                "{}",
                e.name
            );
        }
        assert!(schema("nope").is_none());
    }
}
