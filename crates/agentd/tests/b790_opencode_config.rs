//! The OpenCode arm's session configuration: the `opencode.json` agentd writes before the engine starts, the same
//! document in the environment, what the launch adds once the modules ran, and the two lines of the engine (v1 and v2).
#![allow(clippy::unwrap_used)]

mod b790_support;

use agentd::emit::{aws_env, emit_aws_config, emit_engine};
use agentd::engine::opencode::{self, Line};
use b790_support::{MODEL, MODEL_VALUE, ctx, read_json};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn emitted(model: &str) -> (agentd::emit::EmitContext, agentd::emit::Emitted) {
    let c = ctx(model);
    let out = opencode::emit(&c, Line::V1).unwrap();
    (c, out)
}

/// The file the engine finds as its global configuration (`XDG_CONFIG_HOME` is the run's configuration directory).
fn file_path(c: &agentd::emit::EmitContext) -> std::path::PathBuf {
    agentd::engine::layout::config_layout("opencode", &c.config_dir)
        .root
        .join("opencode.json")
}

fn main_file(c: &agentd::emit::EmitContext) -> Value {
    read_json(&file_path(c))
}

fn content(out: &agentd::emit::Emitted) -> Value {
    serde_json::from_str(&out.env["OPENCODE_CONFIG_CONTENT"]).unwrap()
}

#[test]
fn b790_the_document_names_bedrock_the_region_and_the_agent_profile() {
    let (c, _) = emitted(MODEL);
    let doc = main_file(&c);
    assert_eq!(doc["enabled_providers"], json!(["amazon-bedrock"]));
    let opts = &doc["provider"]["amazon-bedrock"]["options"];
    assert_eq!(opts["region"], "us-west-2");
    assert_eq!(opts["profile"], "agent");
    // No key or token of any kind sits in the file: the profile's credential_process is the only source.
    let text = std::fs::read_to_string(file_path(&c)).unwrap();
    for needle in [
        "apiKey",
        "accessKey",
        "secret",
        "bearer",
        "AWS_BEARER_TOKEN_BEDROCK",
    ] {
        assert!(!text.contains(needle), "{needle} must not be written");
    }
}

#[test]
fn b790_the_model_of_the_document_is_the_value_of_the_neutral_mapping() {
    assert_eq!(
        agentd::engine::config::acp_model_value("opencode", MODEL),
        MODEL_VALUE
    );
    assert_eq!(
        agentd::engine::config::catalog_model_id("opencode", MODEL_VALUE),
        MODEL
    );
    let (c, _) = emitted(MODEL);
    let doc = main_file(&c);
    assert_eq!(doc["model"], MODEL_VALUE);
    assert_eq!(doc["small_model"], MODEL_VALUE);
}

#[test]
fn b790_the_route_is_declared_with_its_window_and_output_limit() {
    let (c, _) = emitted(MODEL);
    let doc = main_file(&c);
    let entry = &doc["provider"]["amazon-bedrock"]["models"][MODEL];
    assert_eq!(entry["tool_call"], true);
    assert_eq!(entry["limit"]["context"], 1_000_000);
    assert_eq!(entry["limit"]["output"], 64_000);
}

#[test]
fn b790_no_permission_can_wait_for_an_answer_that_never_comes() {
    let (c, _) = emitted(MODEL);
    let doc = main_file(&c);
    let p = &doc["permission"];
    // The asks that default to "ask" in OpenCode: an unanswered one hangs the turn, and a child session's is never forwarded to the client.
    // A repeated identical call is refused by a rule, which the model reads as an error and answers by changing course; it never waits.
    assert_eq!(p["doom_loop"], "deny");
    assert_eq!(p["external_directory"], "allow");
    assert_eq!(p["read"], "allow");
    // The tools that would ask the person at the other end.
    for tool in ["question", "plan_enter", "plan_exit"] {
        assert_eq!(p[tool], "deny", "{tool}");
    }
    assert_eq!(p["websearch"], "deny");
    // No rule of the block is "ask", and edits and shell calls are not restricted (the auto-approve posture of the first engine).
    let all = p.as_object().unwrap();
    assert!(all.values().all(|v| v != "ask"), "{p}");
    assert!(all.get("edit").is_none_or(|v| v == "allow"));
    assert!(all.get("bash").is_none_or(|v| v == "allow"));
}

#[test]
fn b790_sharing_updates_snapshots_and_telemetry_spans_are_off() {
    let (c, _) = emitted(MODEL);
    let doc = main_file(&c);
    assert_eq!(doc["share"], "disabled");
    assert_eq!(doc["autoupdate"], false);
    assert_eq!(doc["snapshot"], false);
    assert_eq!(doc["lsp"], false);
    assert_eq!(doc["$schema"], "https://opencode.ai/config.json");
    // Telemetry spans are opt-in upstream and stay unset; no endpoint of ours or anyone's is written.
    assert!(doc.pointer("/experimental/openTelemetry").is_none());
    // The only address in the file is the name of the schema; the engine is pointed at no host, and no remote instruction is listed.
    let mut rest = doc.clone();
    rest.as_object_mut().unwrap().remove("$schema");
    assert!(!rest.to_string().contains("http"), "{rest}");
}

#[test]
fn b790_instructions_point_at_the_rules_and_the_skills_come_from_the_placed_folder() {
    let (c, _) = emitted(MODEL);
    let doc = main_file(&c);
    let layout = agentd::engine::layout::config_layout("opencode", &c.config_dir);
    let want = format!("{}/*.md", layout.rules.display());
    assert_eq!(doc["instructions"], json!([want]));
    assert_eq!(
        doc["skills"]["paths"],
        json!([layout.skills.display().to_string()])
    );
    // They sit in the root the harness owns, and that root is not the engine's own configuration folder.
    assert_ne!(layout.root, c.config_dir);
    assert!(layout.rules.starts_with(&layout.root) && layout.skills.starts_with(&layout.root));
}

#[test]
fn b790_the_environment_closes_the_project_config_and_the_default_plugins() {
    let (c, out) = emitted(MODEL);
    // A repository's own opencode.json or .opencode folder is not read at all, besides being moved aside before launch.
    assert_eq!(out.env["OPENCODE_DISABLE_PROJECT_CONFIG"], "1");
    // The default plugins are fetched from a package registry at start.
    assert_eq!(out.env["OPENCODE_DISABLE_DEFAULT_PLUGINS"], "1");
    // OpenCode asks min(model limit, this) output tokens.
    assert_eq!(out.env["OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX"], "64000");
    // The file the engine reads sits in the harness's root; no other configuration folder is named.
    assert_eq!(
        out.env["OPENCODE_CONFIG"],
        file_path(&c).display().to_string()
    );
    assert!(!out.env.contains_key("OPENCODE_CONFIG_DIR"));
    for k in out.env.keys() {
        assert!(
            !k.starts_with("AWS_") && !k.contains("OTEL") && k != "OPENCODE_PERMISSION",
            "the emitter sets no AWS, OTEL or permission variable: {k}"
        );
    }
    assert!(file_path(&c).is_file());
}

#[test]
fn b790_the_second_line_is_given_only_the_names_it_holds() {
    let c = ctx(MODEL);
    let out = opencode::emit(&c, Line::V2).unwrap();
    for name in [
        "OPENCODE_DISABLE_PROJECT_CONFIG",
        "OPENCODE_CONFIG",
        "OPENCODE_CONFIG_CONTENT",
    ] {
        assert!(out.env.contains_key(name), "{name}");
    }
    for name in [
        "OPENCODE_DISABLE_DEFAULT_PLUGINS",
        "OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX",
    ] {
        assert!(
            !out.env.contains_key(name),
            "{name} is a name of the first line only"
        );
    }
}

#[test]
fn b790_the_same_document_is_also_the_inline_layer_that_no_file_can_override() {
    let (c, out) = emitted(MODEL);
    // `OPENCODE_CONFIG_CONTENT` outranks every file the engine could write (a repository's, the home folder's `.opencode`).
    assert_eq!(content(&out), main_file(&c));
    assert_eq!(out.files, vec![file_path(&c)]);
}

#[test]
fn b790_the_user_s_compaction_setting_reaches_the_engine_s_own_key() {
    let mut c = ctx(MODEL);
    c.compaction = Some(agentd::engine::user_settings::CompactionBlock {
        enabled: Some(false),
        strategy: None,
        threshold_ratio: None,
    });
    opencode::emit(&c, Line::V1).unwrap();
    let doc = main_file(&c);
    assert_eq!(doc["compaction"]["auto"], false);
    // Without a setting the document carries no compaction key at all.
    let (c2, _) = emitted(MODEL);
    assert!(main_file(&c2).get("compaction").is_none());
}

#[test]
fn b790_the_line_follows_the_profile_version() {
    assert_eq!(Line::of_version("1.18.32"), Line::V1);
    assert_eq!(Line::of_version("1.19.5"), Line::V1);
    assert_eq!(Line::of_version("2.2.6"), Line::V2);
    assert_eq!(Line::of_version("2.0.0-beta.1"), Line::V2);
    // A version that cannot be read is the line the pin has today.
    assert_eq!(Line::of_version(""), Line::V1);
    assert_eq!(Line::of_version("latest"), Line::V1);
}

#[test]
fn b790_the_second_line_gets_its_own_keys_and_none_it_warns_about() {
    let c = ctx(MODEL);
    let out = opencode::emit(&c, Line::V2).unwrap();
    let doc = main_file(&c);
    // v2 names the update switch and the snapshot switch differently, and has no doom_loop permission.
    assert_eq!(doc["update"], "disable");
    assert_eq!(doc["snapshots"], false);
    assert!(doc.get("autoupdate").is_none());
    assert!(doc.get("snapshot").is_none());
    assert!(doc["permission"].get("doom_loop").is_none());
    for unsupported in ["subagent_depth", "layout", "server", "logLevel"] {
        assert!(doc.get(unsupported).is_none(), "{unsupported}");
    }
    assert!(doc.pointer("/experimental/continue_loop_on_deny").is_none());
    // The kill switches and the inline layer are the same on both lines.
    assert_eq!(out.env["OPENCODE_DISABLE_PROJECT_CONFIG"], "1");
    assert_eq!(content(&out), doc);
}

#[test]
fn b790_the_generic_entry_is_answered_by_the_opencode_arm_and_the_aws_profile_is_untouched() {
    // `emit_engine` is the entry the launch calls for every engine.
    let c = ctx(MODEL);
    let out = emit_engine(&c).unwrap();
    assert_eq!(out.files, vec![file_path(&c)]);
    // The AWS profile agentd writes for every engine: agent, credential_process, no keys.
    let aws = emit_aws_config(&c).unwrap();
    let env = aws_env(&c, &aws);
    assert_eq!(env["AWS_PROFILE"], "agent");
    assert!(!env.contains_key("AWS_BEARER_TOKEN_BEDROCK"));
    let text = std::fs::read_to_string(aws).unwrap();
    assert!(text.contains(
        "credential_process = /opt/harness/bin/agentd creds --socket /run/agentd/creds.sock"
    ));
}

fn template() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join("engine/profiles/templates/mcp-opencode.json")).unwrap()
}

#[test]
fn b790_the_tool_server_registration_is_a_remote_mcp_entry() {
    let rendered =
        agentd::direct::tools::register::render(&template(), "ham", "http://127.0.0.1:40123/mcp")
            .unwrap();
    let v: Value = serde_json::from_str(&rendered).unwrap();
    let entry = &v["mcp"]["ham"];
    assert_eq!(entry["type"], "remote");
    assert_eq!(entry["url"], "http://127.0.0.1:40123/mcp");
    assert_eq!(entry["enabled"], true);
    assert_eq!(entry["oauth"], false);
    assert_eq!(entry["timeout"], 120_000);
}

fn launched_env(c: &agentd::emit::EmitContext) -> BTreeMap<String, String> {
    opencode::emit(c, Line::V1).unwrap().env
}

#[test]
fn b790_once_the_tool_server_is_up_its_entry_joins_the_inline_layer_only() {
    let c = ctx(MODEL);
    let mut env = launched_env(&c);
    env.insert("HAMH_TOOLS_URL".into(), "http://127.0.0.1:40123/mcp".into());
    opencode::finalize_env(Line::V1, Some(&template()), &[], &mut env).unwrap();
    let inline: Value = serde_json::from_str(&env["OPENCODE_CONFIG_CONTENT"]).unwrap();
    assert_eq!(inline["mcp"]["ham"]["url"], "http://127.0.0.1:40123/mcp");
    // The rest of the document is what the file holds.
    let mut rest = inline.clone();
    rest.as_object_mut().unwrap().remove("mcp");
    assert_eq!(rest, main_file(&c));
}

#[test]
fn b790_with_no_tool_server_nothing_is_registered() {
    let c = ctx(MODEL);
    let mut env = launched_env(&c);
    let before = env.clone();
    opencode::finalize_env(Line::V1, Some(&template()), &[], &mut env).unwrap();
    assert_eq!(env, before, "no URL, no entry");
}

#[test]
fn b790_a_stage_kill_switch_for_a_tool_closes_the_matching_permission() {
    let c = ctx(MODEL);
    let mut env = launched_env(&c);
    let disabled = vec![
        "spawn_agent".to_string(),
        "fetch_web_content".to_string(),
        "no_such_tool".to_string(),
    ];
    opencode::finalize_env(Line::V1, None, &disabled, &mut env).unwrap();
    let inline: Value = serde_json::from_str(&env["OPENCODE_CONFIG_CONTENT"]).unwrap();
    // The helper-agent tool is `task` here, the fetch tool is `webfetch`; a name the arm does not know is ignored.
    assert_eq!(inline["permission"]["task"], "deny");
    assert_eq!(inline["permission"]["webfetch"], "deny");
    assert!(inline["permission"].get("no_such_tool").is_none());
    // Everything else in the block is as it was.
    assert_eq!(inline["permission"]["doom_loop"], "deny");
}

#[test]
fn b790_on_the_second_line_the_tool_server_is_an_ordinary_tool_and_not_a_code_mode_server() {
    let c = ctx(MODEL);
    let mut env = opencode::emit(&c, Line::V2).unwrap().env;
    env.insert("HAMH_TOOLS_URL".into(), "http://127.0.0.1:40123/mcp".into());
    opencode::finalize_env(Line::V2, Some(&template()), &[], &mut env).unwrap();
    let inline: Value = serde_json::from_str(&env["OPENCODE_CONFIG_CONTENT"]).unwrap();
    let entry = &inline["mcp"]["servers"]["ham"];
    assert_eq!(entry["type"], "remote");
    assert_eq!(entry["url"], "http://127.0.0.1:40123/mcp");
    // Without this the engine offers the server's tools only through its single `execute` tool.
    assert_eq!(entry["codemode"], false);
    assert_eq!(entry["oauth"], false);
    assert_eq!(entry["timeout"]["execution"], 120_000);
    assert!(
        inline["mcp"].get("ham").is_none(),
        "no first-line member beside it"
    );
}

#[test]
fn b790_a_broken_template_is_an_error_the_launch_can_log_and_the_environment_stays_whole() {
    let c = ctx(MODEL);
    let mut env = launched_env(&c);
    env.insert("HAMH_TOOLS_URL".into(), "http://127.0.0.1:40123/mcp".into());
    let before = env.clone();
    assert!(
        opencode::finalize_env(Line::V1, Some("{ not json {toolsUrl}"), &[], &mut env).is_err()
    );
    assert!(
        opencode::finalize_env(
            Line::V1,
            Some("{\"mcp\": {\"{toolsName}\": \"{toolsUrl} {toolsX}\"}}"),
            &[],
            &mut env
        )
        .is_err()
    );
    assert_eq!(env, before);
}
