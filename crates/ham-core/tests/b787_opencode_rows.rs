//! The OpenCode rows of the model list: `engine/catalog/models.json` lists a model for OpenCode when, and only when, a passing OpenCode smoke
//! record (`engine/catalog/smoke-opencode-<model id>.json`) exists for it, and the row stands on that file. The pair check of the start request reads
//! these rows: a pair without a row is refused with the pair named.
#![allow(clippy::unwrap_used)]

use ham_core::models::{self, Model, ModelRefusal};
use harness_protocol::common::Engine;
use serde_json::Value;
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
    assert_eq!(passing.len(), 5, "five models passed on the pinned release");
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

#[test]
fn b787_each_opencode_row_stands_on_its_record_file() {
    let version = pinned_version();
    for (name, record) in smoke_records() {
        let id = record["model"]["id"].as_str().unwrap();
        let row = opencode_rows()
            .iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("{name} has no row"));
        assert_eq!(row.engine, Engine::OPENCODE, "{id}");
        assert_eq!(
            row.smoke_record_id.as_deref(),
            record["smokeRecordId"].as_str(),
            "{id}"
        );
        assert_eq!(
            row.smoked_at.as_deref(),
            record["smokedAt"].as_str(),
            "{id}"
        );
        assert_eq!(
            row.region,
            record["route"]["region"].as_str().unwrap(),
            "{id}"
        );
        assert_eq!(record["route"]["protocol"], "converse", "{id}");
        assert_eq!(
            row.alias.as_deref(),
            record["model"]["alias"].as_str(),
            "{id}"
        );
        assert_eq!(record["engine"], "opencode", "{id}");
        assert_eq!(record["engineVersion"].as_str().unwrap(), version, "{id}");
        assert!(row.selectable(), "{id}");
    }
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
        assert_eq!(
            (row.context_tokens, row.max_output_tokens),
            (cline.context_tokens, cline.max_output_tokens),
            "{}",
            row.id
        );
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
            "global.anthropic.claude-fable-5-2"
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
