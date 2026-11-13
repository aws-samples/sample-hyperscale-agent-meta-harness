//! The OpenCode row of Claude Fable 5.2: the model is listed for OpenCode only because a passing OpenCode smoke record exists for it, the row stands on
//! that record file, and it carries the same agreement mark as the Cline row of the model.
#![allow(clippy::unwrap_used)]

use ham_core::models::builtin_catalog;
use harness_protocol::common::Engine;
use serde_json::Value;
use std::path::PathBuf;

const FABLE: &str = "global.anthropic.claude-fable-5-2";

fn record() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../engine/catalog")
        .join(format!("smoke-opencode-{FABLE}.json"));
    serde_json::from_str(
        &std::fs::read_to_string(path).expect("the OpenCode smoke record of Fable 5.2"),
    )
    .unwrap()
}

#[test]
fn b805_fable_is_listed_for_opencode_with_its_record_and_needs_sharing_there_too() {
    let catalog = builtin_catalog();
    let row = catalog
        .for_engine(Engine::OPENCODE)
        .unwrap()
        .models()
        .iter()
        .find(|m| m.id == FABLE)
        .expect("Claude Fable 5.2 has an OpenCode row");
    let record = record();
    assert_eq!(record["result"], "pass");
    assert_eq!(record["engine"], "opencode");
    assert_eq!(record["route"]["protocol"], "converse");
    assert_eq!(row.engine, Engine::OPENCODE);
    assert!(row.selectable());
    assert!(
        row.requires_provider_sharing,
        "the agreement is the model's, not the engine's"
    );
    assert_eq!(row.alias.as_deref(), Some("fable"));
    assert_eq!(
        row.smoke_record_id.as_deref(),
        record["smokeRecordId"].as_str()
    );
    assert_eq!(row.smoked_at.as_deref(), record["smokedAt"].as_str());
    assert_eq!(row.region, record["route"]["region"].as_str().unwrap());
    // Same window and output limit as the Cline row; OpenCode has no reasoning table and has smoked no second route.
    let cline = catalog
        .default_list()
        .models()
        .iter()
        .find(|m| m.id == FABLE)
        .unwrap();
    assert_eq!(
        (row.context_tokens, row.max_output_tokens),
        (cline.context_tokens, cline.max_output_tokens)
    );
    assert!(row.reasoning.is_none() && row.alternate_routes.is_empty() && !row.default);
}
