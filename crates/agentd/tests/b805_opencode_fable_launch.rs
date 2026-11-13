//! How Claude Fable 5.2 is launched on OpenCode. The model goes through the same path as every other Claude row: the catalog id maps to the engine's
//! `amazon-bedrock/<id>` value at the ACP boundary only, the session document declares the route with the row's window and output limit, and OpenCode gets
//! no reasoning argument or variant (it has none: the `reasoning.effort` levels are a Cline setting, and an OpenCode row carries no reasoning table).
#![allow(clippy::unwrap_used)]

mod common;

use agentd::emit::{EmitContext, emit_engine_for};
use agentd::engine::config::{acp_model_value, catalog_model_id};
use agentd::engine::direct::{direct_launch_argv, has_reasoning_flag};
use agentd::engine::user_settings::Effort;
use serde_json::Value;
use std::path::PathBuf;

const FABLE: &str = "global.anthropic.claude-fable-5-2";

fn emitted(label: &str) -> (Value, String) {
    let base = common::scratch(label);
    let ctx = EmitContext {
        engine: "opencode".into(),
        model: FABLE.into(),
        region: "us-east-1".into(),
        data_dir: base.join("data"),
        config_dir: base.join("config"),
        agentd_exe: PathBuf::from("/opt/harness/bin/agentd"),
        socket: PathBuf::from("/run/agentd/creds.sock"),
        max_output_tokens: Some(128_000),
        context_tokens: Some(1_000_000),
        compaction: None,
    };
    let version =
        harness_core::engine_profile::bundled_profile(harness_protocol::common::Engine::OPENCODE)
            .unwrap()
            .unwrap()
            .version;
    let out = emit_engine_for(&ctx, &version).unwrap();
    let text = out.env.get("OPENCODE_CONFIG_CONTENT").unwrap().clone();
    (serde_json::from_str(&text).unwrap(), text)
}

#[test]
fn b805_the_acp_value_of_fable_on_opencode_is_the_bedrock_provider_form_and_maps_back() {
    let value = acp_model_value("opencode", FABLE);
    assert_eq!(value, "amazon-bedrock/global.anthropic.claude-fable-5-2");
    assert_eq!(catalog_model_id("opencode", &value), FABLE);
    // Cline takes the catalog id as it is.
    assert_eq!(acp_model_value("cline", FABLE), FABLE);
}

#[test]
fn b805_the_session_document_declares_the_fable_route_with_the_rows_window_and_output_limit() {
    let (doc, _) = emitted("b805-doc");
    assert_eq!(
        doc["model"],
        "amazon-bedrock/global.anthropic.claude-fable-5-2"
    );
    assert_eq!(doc["small_model"], doc["model"]);
    let provider = &doc["provider"]["amazon-bedrock"];
    assert_eq!(provider["options"]["region"], "us-east-1");
    assert_eq!(provider["options"]["profile"], "agent");
    let entry = &provider["models"][FABLE];
    assert_eq!(entry["tool_call"], true, "{entry}");
    assert_eq!(entry["limit"]["context"], 1_000_000, "{entry}");
    assert_eq!(entry["limit"]["output"], 128_000, "{entry}");
    assert_eq!(
        doc["enabled_providers"],
        serde_json::json!(["amazon-bedrock"])
    );
}

#[test]
fn b805_opencode_gets_no_reasoning_argument_variant_or_option_for_fable() {
    let (doc, text) = emitted("b805-noreason");
    for word in ["thinking", "reasoning", "variant", "effort"] {
        assert!(
            !text.to_ascii_lowercase().contains(word),
            "the OpenCode document names {word}: {doc}"
        );
    }
    assert!(!has_reasoning_flag("opencode") && has_reasoning_flag("cline"));
    for effort in [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh] {
        let argv = vec!["/opt/harness/bin/opencode".to_string(), "acp".to_string()];
        assert_eq!(
            direct_launch_argv("opencode", argv.clone(), effort),
            argv,
            "{effort:?}"
        );
    }
}
