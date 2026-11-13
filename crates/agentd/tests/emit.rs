//! Engine config emitters.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::emit::{
    EmitContext, acp_model_value, aws_env, emit_aws_config, emit_engine, emit_profile, render,
};
use common::scratch;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn ctx(engine: &str, model: &str) -> EmitContext {
    let d = scratch(&format!("emit-{engine}"));
    EmitContext {
        engine: engine.into(),
        model: model.into(),
        region: "us-west-2".into(),
        data_dir: d.join("data"),
        config_dir: d.join("config"),
        agentd_exe: PathBuf::from("/opt/harness/bin/agentd"),
        socket: PathBuf::from("/run/agentd/creds.sock"),
        max_output_tokens: None,
        context_tokens: None,
        compaction: None,
    }
}

fn json(p: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn the_aws_profile_asks_agentd_for_credentials() {
    let c = ctx("cline", "m");
    let path = emit_aws_config(&c).unwrap();
    assert_eq!(path, c.config_dir.join("aws/config"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text,
        "[profile agent]\nregion = us-west-2\ncredential_process = /opt/harness/bin/agentd creds --socket /run/agentd/creds.sock\n"
    );
    let env = aws_env(&c, &path);
    assert_eq!(env["AWS_PROFILE"], "agent");
    assert_eq!(env["AWS_CONFIG_FILE"], path.display().to_string());
    assert_eq!(env["AWS_EC2_METADATA_DISABLED"], "true");
    assert_eq!(env["AWS_REGION"], "us-west-2");
    assert_eq!(env["AWS_DEFAULT_REGION"], "us-west-2");
    assert!(env["AWS_SHARED_CREDENTIALS_FILE"].ends_with("no-shared-credentials"));
    for k in env.keys() {
        assert!(
            !matches!(
                k.as_str(),
                "AWS_ACCESS_KEY_ID"
                    | "AWS_SECRET_ACCESS_KEY"
                    | "AWS_SESSION_TOKEN"
                    | "AWS_BEARER_TOKEN_BEDROCK"
            ),
            "{k}"
        );
    }
}

#[test]
fn cline_gets_iam_bedrock_providers_and_telemetry_off() {
    let c = ctx("cline", "us.anthropic.claude-sonnet-5");
    let out = emit_engine(&c).unwrap();
    let providers = json(&c.data_dir.join("settings/providers.json"));
    assert_eq!(providers["lastUsedProvider"], "bedrock");
    let s = &providers["providers"]["bedrock"]["settings"];
    assert_eq!(s["provider"], "bedrock");
    assert_eq!(s["model"], "us.anthropic.claude-sonnet-5");
    assert_eq!(s["aws"]["authentication"], "iam");
    assert_eq!(s["aws"]["region"], "us-west-2");
    let gs_path = c.config_dir.join("global-settings.json");
    let gs = json(&gs_path);
    assert_eq!(gs["telemetryOptOut"], true);
    assert_eq!(gs["autoUpdateEnabled"], false);
    assert_eq!(
        out.env["CLINE_GLOBAL_SETTINGS_PATH"],
        gs_path.display().to_string()
    );
    assert_eq!(out.env["CLINE_NO_AUTO_UPDATE"], "1");
    assert!(
        !out.env.contains_key("CLINE_API_KEY"),
        "patch 0007: no placeholder key"
    );
    assert_eq!(out.files.len(), 2);
}

#[test]
fn other_engines_get_no_built_in_files() {
    let out = emit_engine(&ctx("fake-acp", "m")).unwrap();
    assert!(out.files.is_empty());
    assert!(out.env.is_empty());
}

#[test]
fn acp_model_values_follow_each_engine() {
    assert_eq!(
        acp_model_value("cline", "us.anthropic.claude-sonnet-5"),
        "us.anthropic.claude-sonnet-5"
    );
    assert_eq!(
        acp_model_value("cline", "aws-mantle/qwen.qwen3-coder-next"),
        "aws-mantle/qwen.qwen3-coder-next"
    );
    assert_eq!(acp_model_value("fake-acp", "anything"), "anything");
}

#[test]
fn render_substitutes_known_placeholders_and_keeps_others() {
    let vars: BTreeMap<&str, String> = [
        ("model", "m1".to_string()),
        ("region", "us-east-1".to_string()),
    ]
    .into();
    assert_eq!(
        render("{model}@{region} {unknown} {{x}}", &vars),
        "m1@us-east-1 {unknown} {{x}}"
    );
}

#[test]
fn profile_emitters_render_templates_into_their_paths() {
    let d = scratch("emit-profile");
    std::fs::create_dir_all(d.join("engine/x")).unwrap();
    std::fs::write(
        d.join("engine/x/settings.json.tmpl"),
        "{\"model\": \"{model}\", \"region\": \"{region}\"}\n",
    )
    .unwrap();
    let profile = harness_core::engine_profile::parse_profile(
        r#"
schemaVersion = 1
name = "x"
version = "1"
[binary]
path = "x"
[transports.acp]
argv = ["x"]
stdin = "pipe"
cancel = { method = "acp-cancel", killAfterS = 5 }
[[configEmitters]]
path = "{configDir}/x/settings.json"
template = "engine/x/settings.json.tmpl"
format = "json"
[gate]
name = "QG-X"
[[rows]]
id = "K0"
class = "gating"
check = "c"
pass = { minPass = 1, runs = 1 }
"#,
    )
    .unwrap();
    let vars: BTreeMap<&str, String> = [
        ("configDir", d.join("cfg").display().to_string()),
        ("model", "m9".into()),
        ("region", "us-east-2".into()),
    ]
    .into();
    let files = emit_profile(&profile, &d, &vars).unwrap();
    assert_eq!(files, vec![d.join("cfg/x/settings.json")]);
    let v = json(&files[0]);
    assert_eq!(v["model"], "m9");
    assert_eq!(v["region"], "us-east-2");
    // A template outside the templates root is refused.
    let mut bad = profile.clone();
    bad.config_emitters[0].template = "../../outside.tpl".into();
    assert!(emit_profile(&bad, &d, &vars).is_err());
}

#[test]
fn a_follow_up_prompt_adds_reported_usage_and_never_invents_zeros() {
    use agentd::turn::prompt_usage;
    use harness_protocol::common::Usage;
    let u = |i, o| Usage {
        input_tokens: i,
        output_tokens: o,
        cache_read_input_tokens: 1,
        cache_write_input_tokens: 0,
    };
    let sum = Usage {
        input_tokens: 15,
        output_tokens: 7,
        cache_read_input_tokens: 2,
        cache_write_input_tokens: 0,
    };
    assert_eq!(prompt_usage(Some(u(10, 5)), Some(u(5, 2)), None), Some(sum));
    assert_eq!(prompt_usage(None, Some(u(5, 2)), None), Some(u(5, 2)));
    assert_eq!(
        prompt_usage(Some(u(10, 5)), None, Some(u(3, 3))),
        Some(u(3, 3))
    );
    assert_eq!(prompt_usage(Some(u(10, 5)), None, None), None);
}

#[test]
fn the_forks_providers_json_is_a_file_cline_accepts() {
    // Cline's StoredProviderSettingsEntrySchema (core/src/types/provider-settings.ts) requires
    // `updatedAt: z.string().datetime()` on every provider entry. Without it Cline ignores the
    // whole file with no error, so every setting agentd writes, maxTokens included, is dropped
    // (live 2026-09-28: a 24-token cap left the Converse request at the 38,400 default).
    let mut c = ctx("cline", "us.anthropic.claude-sonnet-5");
    c.max_output_tokens = Some(8_192);
    emit_engine(&c).unwrap();
    let p = json(&c.data_dir.join("settings/providers.json"));
    assert_eq!(p["version"], 1);
    let entry = &p["providers"]["bedrock"];
    let at = entry["updatedAt"]
        .as_str()
        .unwrap_or_else(|| panic!("no updatedAt: {entry}"));
    assert!(agentd::time::parse_rfc3339(at).is_some(), "{at}");
    assert!(at.ends_with('Z'), "UTC, as zod datetime() requires: {at}");
    assert_eq!(entry["tokenSource"], "manual");
    assert_eq!(entry["settings"]["maxTokens"], 8_192);
}

#[test]
fn the_role_output_cap_reaches_the_fork_through_providers_json() {
    // "Engine outputs" and: Cline's per-role maxTokens goes in providers.json;
    // on Mantle the fork otherwise sends no max-token field and Mantle reserves the model maximum.
    for model in [
        "aws-mantle/qwen.qwen3-coder-next",
        "us.anthropic.claude-sonnet-5",
    ] {
        let mut c = ctx("cline", model);
        c.max_output_tokens = Some(32_000);
        emit_engine(&c).unwrap();
        let s =
            &json(&c.data_dir.join("settings/providers.json"))["providers"]["bedrock"]["settings"];
        assert_eq!(s["maxTokens"], 32_000, "{model}");
        assert_eq!(s["model"], model);
    }
    let c = ctx("cline", "us.anthropic.claude-sonnet-5");
    emit_engine(&c).unwrap();
    let s = &json(&c.data_dir.join("settings/providers.json"))["providers"]["bedrock"]["settings"];
    assert!(s.get("maxTokens").is_none(), "no cap, no field: {s}");
}

// ---- context window ---------------------------------------------------

/// The models the engines' own catalogs do not know: Sonnet 5.5 and GPT-6.1 Sol.
const UNKNOWN_TO_THE_ENGINES: [&str; 2] = [
    "global.anthropic.claude-sonnet-5-5",
    "us.openai.gpt-6.1-sol",
];

fn cline_settings(c: &EmitContext) -> serde_json::Map<String, Value> {
    json(&c.data_dir.join("settings/providers.json"))["providers"]["bedrock"]["settings"]
        .as_object()
        .unwrap()
        .clone()
}

#[test]
fn cline_gets_the_context_window_when_the_turn_sets_one() {
    for model in UNKNOWN_TO_THE_ENGINES {
        let mut c = ctx("cline", model);
        c.context_tokens = Some(1_000_000);
        c.max_output_tokens = Some(8_192);
        emit_engine(&c).unwrap();
        let s = cline_settings(&c);
        assert_eq!(s["contextWindow"], 1_000_000, "{model}");
        assert_eq!(s["maxTokens"], 8_192, "the output cap is unchanged");
        assert_eq!(s["model"], model);
    }
    let mut c = ctx("cline", "us.anthropic.claude-opus-5-5");
    c.context_tokens = Some(150_000);
    emit_engine(&c).unwrap();
    assert_eq!(
        cline_settings(&c)["contextWindow"],
        150_000,
        "a lowered window"
    );
}

#[test]
fn cline_settings_are_unchanged_when_the_turn_sets_no_window() {
    for tokens in [None, Some(0)] {
        let mut c = ctx("cline", "global.anthropic.claude-sonnet-5-5");
        c.context_tokens = tokens;
        c.max_output_tokens = Some(8_192);
        emit_engine(&c).unwrap();
        let s = cline_settings(&c);
        let mut keys: Vec<&str> = s.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["aws", "maxTokens", "model", "provider"],
            "{tokens:?}"
        );
        assert_eq!(
            Value::Object(s),
            serde_json::json!({
                "provider": "bedrock",
                "model": "global.anthropic.claude-sonnet-5-5",
                "aws": {"authentication": "iam", "region": "us-west-2"},
                "maxTokens": 8192,
            })
        );
    }
}

// ---- the providers.json a run writes ---------------------------------------------------------------

/// providers.json as 1.25.0 wrote it for this context, read back as a JSON value with the
/// `updatedAt` write time replaced. The comparison is by value: whether `serde_json` keeps map keys sorted or in insertion
/// order depends on the features the whole workspace build unifies (`preserve_order`), so the text of one file can differ
/// between `cargo test -p agentd` and a workspace run while every member and value is the same.
fn providers_without_the_time(c: &EmitContext) -> Value {
    let mut v = json(&c.data_dir.join("settings/providers.json"));
    assert!(v["providers"]["bedrock"]["updatedAt"].is_string(), "{v}");
    v["providers"]["bedrock"]["updatedAt"] = Value::String("<time>".into());
    v
}

fn golden(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

const PROVIDERS_AS_WRITTEN_BY_1_25_0: &str = r#"{
  "lastUsedProvider": "bedrock",
  "modes": {},
  "providers": {
    "bedrock": {
      "settings": {
        "aws": {
          "authentication": "iam",
          "region": "us-west-2"
        },
        "maxTokens": 32000,
        "model": "us.anthropic.claude-opus-5-5",
        "provider": "bedrock"
      },
      "tokenSource": "manual",
      "updatedAt": "<time>"
    }
  },
  "version": 1
}
"#;

const PROVIDERS_WITH_A_WINDOW_AS_WRITTEN_BY_1_25_0: &str = r#"{
  "lastUsedProvider": "bedrock",
  "modes": {},
  "providers": {
    "bedrock": {
      "settings": {
        "aws": {
          "authentication": "iam",
          "region": "us-west-2"
        },
        "contextWindow": 1000000,
        "maxTokens": 32000,
        "model": "us.anthropic.claude-opus-5-5",
        "provider": "bedrock"
      },
      "tokenSource": "manual",
      "updatedAt": "<time>"
    }
  },
  "version": 1
}
"#;

#[test]
fn a_run_writes_the_providers_json_it_wrote_before() {
    let mut c = ctx("cline", "us.anthropic.claude-opus-5-5");
    c.max_output_tokens = Some(32_000);
    emit_engine(&c).unwrap();
    assert_eq!(
        providers_without_the_time(&c),
        golden(PROVIDERS_AS_WRITTEN_BY_1_25_0)
    );
    // A run with a window is as before too.
    let mut w = ctx("cline", "us.anthropic.claude-opus-5-5");
    w.max_output_tokens = Some(32_000);
    w.context_tokens = Some(1_000_000);
    emit_engine(&w).unwrap();
    assert_eq!(
        providers_without_the_time(&w),
        golden(PROVIDERS_WITH_A_WINDOW_AS_WRITTEN_BY_1_25_0)
    );
}
