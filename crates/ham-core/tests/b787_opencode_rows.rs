//! The OpenCode rows of the model list: `engine/catalog/models.json` lists a model for OpenCode when, and only when, a passing OpenCode smoke
//! record (`engine/catalog/smoke-opencode-<model id>.json`) exists for it, and the row stands on that file. The pair check of the start request reads
//! these rows: a pair without a row is refused with the pair named.
#![allow(clippy::unwrap_used)]

use ham_core::models::{self, Catalog, Model, ModelRefusal};
use harness_protocol::common::Engine;
use serde_json::{Value, json};
use std::path::PathBuf;

const GLM: &str = "us.zai.glm-5.3";

fn catalog_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../engine/catalog")
}

/// Every `smoke-opencode-*.json` of the catalog directory: (file name, parsed record), in name order.
fn smoke_records() -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(catalog_dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name.starts_with("smoke-opencode-") && name.ends_with(".json") {
            let text = std::fs::read_to_string(&path).unwrap();
            out.push((name, serde_json::from_str(&text).unwrap()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn passing(records: &[(String, Value)]) -> Vec<&(String, Value)> {
    records
        .iter()
        .filter(|(_, r)| r["result"] == "pass")
        .collect()
}

fn opencode_rows() -> &'static [Model] {
    models::builtin_catalog()
        .for_engine(Engine::OPENCODE)
        .expect("the catalog lists models for OpenCode")
        .models()
}

fn cline_row(id: &str) -> &'static Model {
    models::builtin()
        .models()
        .iter()
        .find(|m| m.id == id)
        .unwrap()
}

fn pinned_version() -> String {
    let profile = std::fs::read_to_string(catalog_dir().join("../profiles/opencode.toml")).unwrap();
    let line = profile
        .lines()
        .find(|l| l.starts_with("version = "))
        .expect("the profile names its version");
    line.trim_start_matches("version = ")
        .trim_matches('"')
        .to_string()
}

#[test]
fn b787_opencode_has_a_row_for_each_model_with_a_passing_record_and_for_no_other() {
    let records = smoke_records();
    let passing = passing(&records);
    assert_eq!(
        passing.len(),
        7,
        "seven models passed on the pinned release"
    );
    let mut want: Vec<String> = passing
        .iter()
        .map(|(_, r)| r["model"]["id"].as_str().unwrap().to_string())
        .collect();
    want.sort();
    let mut have: Vec<String> = opencode_rows().iter().map(|m| m.id.clone()).collect();
    have.sort();
    assert_eq!(
        have, want,
        "a row for each passing record, none without one"
    );
    for (name, record) in &records {
        assert_eq!(
            name,
            &format!(
                "smoke-opencode-{}.json",
                record["model"]["id"].as_str().unwrap()
            )
        );
    }
}

/// What binds a row to its record file, or the parts in which the two differ: the id, the record id, the time, the Region, the engine and its
/// pinned release. The record's `model.alias` is not compared: it is the name the model had on the day of the record.
fn stands_on(row: &Model, record: &Value, version: &str) -> Result<(), String> {
    let id = record["model"]["id"].as_str().unwrap_or_default();
    let mut wrong: Vec<&str> = Vec::new();
    let mut need = |ok: bool, what: &'static str| {
        if !ok {
            wrong.push(what);
        }
    };
    need(row.id == id, "the id");
    need(row.engine == Engine::OPENCODE, "the engine of the row");
    need(
        row.smoke_record_id.as_deref() == record["smokeRecordId"].as_str(),
        "the record id",
    );
    need(
        row.smoked_at.as_deref() == record["smokedAt"].as_str(),
        "the time",
    );
    need(
        Some(row.region.as_str()) == record["route"]["region"].as_str(),
        "the Region",
    );
    need(record["route"]["protocol"] == "converse", "the protocol");
    need(record["engine"] == "opencode", "the engine of the record");
    need(
        record["engineVersion"].as_str() == Some(version),
        "the engine release",
    );
    need(row.selectable(), "a passing record");
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{id}: the row and its record differ in: {}",
            wrong.join(", ")
        ))
    }
}

#[test]
fn b787_each_opencode_row_stands_on_its_record_file() {
    let version = pinned_version();
    for (name, record) in smoke_records() {
        let id = record["model"]["id"].as_str().unwrap();
        let row = opencode_rows()
            .iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("{name} has no row"));
        stands_on(row, &record, &version).unwrap();
    }
}

/// The names move with a model update: an earlier model gets a name that carries its version, and its short name goes to the new model.
/// A committed record is evidence and is never edited, so it keeps the name the model had on the day of the record. The id binds a row to its
/// record, whatever the row is called.
#[test]
fn b862_an_opencode_row_binds_to_its_record_by_the_id() {
    let version = pinned_version();
    // The shipped file with the two earlier models of one vendor renamed, in the rows of both engines.
    let mut doc: Value =
        serde_json::from_str(include_str!("../../../engine/catalog/models.json")).unwrap();
    for row in doc["models"].as_array_mut().unwrap() {
        match row["id"].as_str() {
            Some("us.openai.gpt-6.1-sol") => row["alias"] = json!("sol-6.1"),
            Some("us.openai.gpt-6-astra") => row["alias"] = json!("astra-6"),
            _ => {}
        }
    }
    let renamed = Catalog::parse(&doc.to_string()).unwrap();
    let rows = renamed.for_engine(Engine::OPENCODE).unwrap().models();
    let records = smoke_records();
    let mut renamed_rows = 0;
    for (name, record) in &records {
        let id = record["model"]["id"].as_str().unwrap();
        let row = rows
            .iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("{name} has no row"));
        stands_on(row, record, &version).unwrap();
        if row.alias.as_deref() != record["model"]["alias"].as_str() {
            renamed_rows += 1;
        }
    }
    assert!(
        renamed_rows >= 2,
        "the two renamed rows carry another name than their records: {renamed_rows}"
    );
    // What does bind: the record of another model, or a record of another day, is refused with the part named.
    let row_of = |id: &str| rows.iter().find(|m| m.id == id).unwrap();
    let record_of = |id: &str| {
        &records
            .iter()
            .find(|(_, r)| r["model"]["id"] == id)
            .unwrap()
            .1
    };
    let sol = record_of("us.openai.gpt-6.1-sol");
    let e = stands_on(row_of("us.openai.gpt-6-astra"), sol, &version).unwrap_err();
    assert!(e.contains("the id") && e.contains("the record id"), "{e}");
    let mut other_day = sol.clone();
    other_day["smokedAt"] = json!("2026-01-01T00:00:00Z");
    let e = stands_on(row_of("us.openai.gpt-6.1-sol"), &other_day, &version).unwrap_err();
    assert!(e.ends_with("differ in: the time"), "{e}");
}

#[test]
fn b787_an_opencode_row_has_the_route_facts_of_the_cline_row_of_the_same_model() {
    for row in opencode_rows() {
        let cline = cline_row(&row.id);
        assert_eq!(
            (&row.name, &row.vendor, &row.base_model_id, row.protocol),
            (
                &cline.name,
                &cline.vendor,
                &cline.base_model_id,
                cline.protocol
            ),
            "{}",
            row.id
        );
        // The output limit is the model's. The window of an OpenCode row is the maker's figure. The Cline row of an OpenAI model carries the
        // input Bedrock accepts instead: the Cline engine reads the figure as the most input it may send, and OpenCode is told the
        // window and the output limit as two figures.
        assert_eq!(row.max_output_tokens, cline.max_output_tokens, "{}", row.id);
        if row.vendor == "openai" {
            assert_eq!(cline.context_tokens, 920_000, "{}", row.id);
            assert!(row.context_tokens > cline.context_tokens, "{}", row.id);
        } else {
            assert_eq!(row.context_tokens, cline.context_tokens, "{}", row.id);
        }
        // OpenCode has smoked no second route and has no reasoning record of its own: the row claims neither.
        assert!(row.alternate_routes.is_empty(), "{}", row.id);
        assert!(row.reasoning.is_none(), "{}", row.id);
        assert!(row.note.is_none(), "{}", row.id);
    }
}

#[test]
fn b787_the_opencode_list_has_one_default_the_same_model_as_the_default_engine_and_the_aliases_of_the_cline_list()
 {
    let list = models::builtin_catalog()
        .for_engine(Engine::OPENCODE)
        .unwrap();
    let default = list.default_model().expect("a default model for OpenCode");
    assert_eq!(default.id, models::builtin().default_model().unwrap().id);
    assert_eq!(list.models().iter().filter(|m| m.default).count(), 1);
    for row in list.models() {
        assert_eq!(row.alias, cline_row(&row.id).alias, "{}", row.id);
    }
    assert!(list.find("sol").is_some() && list.find("glm").is_none());
}

#[test]
fn b787_the_pair_check_accepts_the_five_models_and_refuses_glm_with_the_pair_named() {
    let catalog = models::builtin_catalog();
    for row in opencode_rows() {
        assert!(
            catalog.check(Engine::OPENCODE, &row.id).is_ok(),
            "{}",
            row.id
        );
        // A switch after a failure stays inside the engine: another OpenCode row, never GLM 5.3, which only the default engine lists.
        for spec in catalog.failover_routes(Engine::OPENCODE, &row.id) {
            assert!(
                opencode_rows().iter().any(|m| m.id == spec.id),
                "{}",
                spec.id
            );
            assert_ne!(spec.id, row.id);
        }
    }
    let refused = catalog.check(Engine::OPENCODE, GLM).unwrap_err();
    assert_eq!(refused.code(), "model-not-smoked");
    assert!(
        matches!(&refused, ModelRefusal::NotSmokedForEngine { engine, id } if engine == "opencode" && id == GLM)
    );
    let words = refused.to_string();
    assert!(words.contains("opencode") && words.contains(GLM), "{words}");
    // A model nobody lists is unknown, and the words name the models OpenCode does offer.
    let unknown = catalog
        .check(Engine::OPENCODE, "no.such.model")
        .unwrap_err();
    assert_eq!(unknown.code(), "unknown-model");
    assert!(unknown.to_string().contains("us.openai.gpt-6.1-sol"));
}

#[test]
fn b787_the_cline_list_is_what_it_was() {
    let ids: Vec<&str> = models::builtin()
        .models()
        .iter()
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "us.anthropic.claude-opus-5-5",
            "global.anthropic.claude-sonnet-5-5",
            "us.openai.gpt-6.1-sol",
            "us.openai.gpt-6-astra",
            "us.zai.glm-5.3",
            "global.anthropic.claude-fable-5-2",
            // Appended by the model update: file order is failover order, so the rows before them are where they were.
            "us.openai.gpt-6.2-sol",
            "us.openai.gpt-6.2-astra"
        ]
    );
    assert!(
        models::builtin()
            .models()
            .iter()
            .all(|m| m.engine == Engine::DEFAULT)
    );
    let engines = models::builtin_catalog().engines();
    assert_eq!(engines, [Engine::CLINE, Engine::OPENCODE]);
}
