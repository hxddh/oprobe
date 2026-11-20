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
pub const ID_BASE: &str = "https://github.com/hxddh/oprobe/schemas/3/";

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

/// First line of a run file.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunFileHeader {
    /// Always `oprobe.run/3`.
    pub format: String,
    pub run_id: String,
    pub oprobe_version: String,
    /// RFC 3339.
    pub created_at: String,
}

/// One line of a `.oprobe` run file (zstd-compressed NDJSON, `oprobe.run/3`): a `header`,
/// the `report`, then `op` and `req` rows. Readers skip unknown kinds.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum RunFileLine {
    Header(RunFileHeader),
    Report(crate::model::Report),
    Op(crate::model::OpRow),
    Req(crate::model::ReqRow),
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
}

/// Every schema, in a stable order.
pub fn entries() -> Vec<Entry> {
    use crate::cli;
    vec![
        entry!(
            "report",
            Output,
            crate::model::Report,
            "Report v3 (oprobe.report/3): the result of one run"
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
            RunFileLine,
            "One line of a .oprobe run file (oprobe.run/3)"
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
        entry!("run", Output, cli::RunOutput, "Result of `oprobe run`"),
        entry!(
            "plan",
            Output,
            cli::run::PlanOutput,
            "Result of `oprobe run --dry-run` / `tune --dry-run`"
        ),
        entry!("tune", Output, cli::TuneOutput, "Result of `oprobe tune`"),
        entry!(
            "compare",
            Output,
            crate::analysis::compare::Comparison,
            "Result of `oprobe compare`"
        ),
        entry!(
            "analyze",
            Output,
            cli::analyze::AnalyzeOutput,
            "Result of `oprobe analyze`"
        ),
        entry!(
            "watch",
            Output,
            cli::watch::WatchOutput,
            "Result of `oprobe watch`"
        ),
        entry!(
            "matrix",
            Output,
            cli::matrix::MatrixOutput,
            "Result of `oprobe matrix`"
        ),
        entry!(
            "matrix-plan",
            Output,
            cli::matrix::MatrixPlanOutput,
            "Result of `oprobe matrix --dry-run`"
        ),
        entry!(
            "compat",
            Output,
            crate::compat::command::CompatOutput,
            "Result of `oprobe compat`"
        ),
        entry!(
            "compat-plan",
            Output,
            crate::compat::command::CompatPlan,
            "Result of `oprobe compat --dry-run`"
        ),
        entry!(
            "runs-export",
            Output,
            cli::export::ExportOutput,
            "Result of `oprobe runs export`"
        ),
        entry!(
            "run-repeat",
            Output,
            cli::repeat::RepeatOutput,
            "Result of `oprobe run --repeat N` / `--baseline NAME`"
        ),
        entry!(
            "baseline-show",
            Output,
            cli::compare::BaselineShow,
            "Result of `oprobe baseline show`"
        ),
        entry!(
            "baseline-list",
            Output,
            cli::compare::BaselineList,
            "Result of `oprobe baseline list`"
        ),
        entry!(
            "demo",
            Output,
            cli::demo::DemoOutput,
            "Result of `oprobe demo`"
        ),
        entry!(
            "self-update",
            Output,
            cli::self_update::UpdateOutput,
            "Result of `oprobe self-update`"
        ),
        entry!(
            "doctor",
            Output,
            cli::doctor::DoctorOutput,
            "Result of `oprobe doctor`"
        ),
        entry!(
            "probe",
            Output,
            crate::net::probe::ProbeReport,
            "Result of `oprobe probe`"
        ),
        entry!(
            "cleanup",
            Output,
            cli::misc::CleanupOutput,
            "Result of `oprobe cleanup`"
        ),
        entry!(
            "lab",
            Output,
            cli::misc::LabOutput,
            "What `oprobe lab` serves"
        ),
        entry!(
            "runs-list",
            Output,
            cli::runs::RunsList,
            "Result of `oprobe runs list`"
        ),
        entry!(
            "runs-show",
            Output,
            cli::runs::RunShow,
            "Result of `oprobe runs show`"
        ),
        entry!(
            "runs-rm",
            Output,
            cli::runs::RemovedRuns,
            "Result of `oprobe runs rm` / `runs gc`"
        ),
        entry!(
            "target-list",
            Output,
            cli::settings::TargetList,
            "Result of `oprobe target list`"
        ),
        entry!(
            "target-show",
            Output,
            cli::settings::TargetShow,
            "Result of `oprobe target show`"
        ),
        entry!(
            "config-show",
            Output,
            cli::settings::ConfigShow,
            "Result of `oprobe config show`"
        ),
        entry!(
            "config-path",
            Output,
            cli::settings::ConfigPaths,
            "Result of `oprobe config path`"
        ),
        entry!(
            "capabilities",
            Output,
            super::capabilities::Capabilities,
            "Result of `oprobe capabilities`"
        ),
        entry!(
            "explain",
            Output,
            super::explain::ExplainOutput,
            "Result of `oprobe explain` (a run or a finding code)"
        ),
        entry!(
            "schema-list",
            Output,
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
