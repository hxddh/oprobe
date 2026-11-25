//! JSON Schemas (2020-12) of the machine-facing documents (design v6 §8), generated from the
//! Rust types with `schemars`. The checked-in copies under `schemas/` are kept in sync by
//! `tests/cli.rs`; regenerate them with `OPROBE_BLESS=1 cargo test --test cli`.

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::ErrorBody;

/// Base of every schema `$id`.
pub const ID_BASE: &str = "https://github.com/hxddh/oprobe/schemas/9/";

/// The `{"type":"error","error":{…}}` document printed on failures under `-o json`.
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

/// One registered schema.
pub struct Entry {
    pub name: &'static str,
    pub description: &'static str,
    /// Read by oprobe (an input such as `op.toml`) rather than written.
    pub input: bool,
    build: fn(SchemaSettings) -> Schema,
}

fn gen_for<T: JsonSchema>(s: SchemaSettings) -> Schema {
    s.into_generator().into_root_schema_for::<T>()
}

/// A command result: `T` in the common envelope (`next`, `warnings`), titled after `T`.
fn gen_cmd<T: JsonSchema>(s: SchemaSettings) -> Schema {
    let mut schema = s
        .into_generator()
        .into_root_schema_for::<crate::output::Envelope<T>>();
    schema.insert("title".into(), Value::String(T::schema_name().into_owned()));
    schema
}

/// Every schema, in a stable order.
pub fn entries() -> Vec<Entry> {
    vec![
        Entry {
            name: "report",
            description: "Report v7 (oprobe.report/7): the profile of one run, as `run -o json` and `show -o json` carry it",
            input: false,
            build: gen_for::<crate::model::Report>,
        },
        Entry {
            name: "run",
            description: "The result of `oprobe run -o json` (and `demo`): one run per operator, each with its report, and the comparison when there are several; `oprobe show A B -o json` prints the same document",
            input: false,
            build: gen_cmd::<crate::app::RunSet>,
        },
        Entry {
            name: "plan",
            description: "The result of `oprobe run --dry-run -o json`: the plan of every operator, nothing sent",
            input: false,
            build: gen_cmd::<crate::app::RunPlan>,
        },
        Entry {
            name: "prove",
            description: "The result of `oprobe prove -o json`: the verdict on the objective and the operator diff",
            input: false,
            build: gen_cmd::<crate::app::ProveOutput>,
        },
        Entry {
            name: "prove-plan",
            description: "The result of `oprobe prove --dry-run -o json`: the estimate of the whole proof and the plan of one trial, nothing sent",
            input: false,
            build: gen_cmd::<crate::app::PlanOutput>,
        },
        Entry {
            name: "show",
            description: "The result of `oprobe show RUN -o json`: the report, where the run is stored, and `--by` / `--since --until` when given",
            input: false,
            build: gen_cmd::<crate::app::Profile>,
        },
        Entry {
            name: "run-list",
            description: "The result of `oprobe show --list -o json`: the stored runs",
            input: false,
            build: gen_cmd::<crate::app::RunList>,
        },
        Entry {
            name: "removed",
            description: "The result of `oprobe show --rm RUN -o json`: the deleted run ids",
            input: false,
            build: gen_cmd::<crate::app::RemovedRuns>,
        },
        Entry {
            name: "lab",
            description: "The startup document of `oprobe lab -o json`: endpoint, scenario and active faults",
            input: false,
            build: gen_cmd::<crate::app::LabOutput>,
        },
        Entry {
            name: "operator",
            description: "An operator spec (oprobe.operator/6, op.toml): service, layers, read/write and HTTP options",
            input: true,
            build: gen_for::<crate::operator::OperatorSpec>,
        },
        Entry {
            name: "error",
            description: "The error document printed under `-o json`",
            input: false,
            build: gen_for::<ErrorEnvelope>,
        },
        Entry {
            name: "events",
            description: "One event of a running command (oprobe.events/1), as the app receives them",
            input: false,
            build: gen_for::<crate::events::Event>,
        },
    ]
}

/// The `$id` of schema `name`.
pub fn id_of(name: &str) -> String {
    format!("{ID_BASE}{name}.json")
}

/// The schema of `e`, with its `$id`, title and description.
pub fn generate(e: &Entry) -> Value {
    let settings = if e.input {
        SchemaSettings::draft2020_12().for_deserialize()
    } else {
        SchemaSettings::draft2020_12().for_serialize()
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

/// The names of every schema.
pub fn names() -> Vec<&'static str> {
    entries().iter().map(|e| e.name).collect()
}

/// The schema called `name`.
pub fn schema(name: &str) -> Option<Value> {
    entries().iter().find(|e| e.name == name).map(generate)
}
