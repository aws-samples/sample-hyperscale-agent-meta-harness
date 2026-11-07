//! The user's engine settings as agentd types and places them.
//! `direct_engine_config.rs` runs the whole agent; these tests take the pieces one at a time:
//! the typed settings built from a member the shared check accepted, the argv word and the environment variable they become, the compaction block of
//! the provider file, the union of the tools switched off, and the read-back that proves the files hold what was meant.
//!
//! No user string reaches a file: every value is an enum member or a number in the engine's own range, so the text written is the harness's own.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::request::engine_settings;
use agentd::emit::{EmitContext, emit_engine};
use agentd::engine::direct::{direct_launch_argv, launch_env, lock_global_settings};
use agentd::engine::user_settings::{
    CommandSeconds, CompactionMode, Effort, Placed, Trigger, UserSettings, UserTool, read_back,
};
use common::{repo_root, scratch};
use harness_core::engine_profile::load_profile;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{EngineConfig, check_engine_config};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn words(a: &[&str]) -> Vec<String> {
    a.iter().map(ToString::to_string).collect()
}

fn profile_argv() -> Vec<String> {
    load_profile(&repo_root().join("engine/profiles/cline.toml"))
        .unwrap()
        .transports
        .acp
        .argv
}

/// A member of the Cline engine with these settings.
fn cfg(settings: Value) -> EngineConfig {
    serde_json::from_value(json!({"engine": "cline", "schema": 1, "settings": settings})).unwrap()
}

/// The typed settings of a document the shared check accepts.
fn settings_of(doc: Value) -> UserSettings {
    let c = cfg(doc);
    check_engine_config(&c).unwrap();
    UserSettings::from_checked(&c).unwrap()
}

fn all_five() -> Value {
    json!({
        "reasoning": {"effort": "high"},
        "commands": {"timeout_seconds": 900},
        "context": {"compaction": "basic", "compaction_trigger": 0.8},
        "tools": {"disable": ["fetch_web_content"]}
    })
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

// ---- typed settings -------------------------------------------------------------------------------------------------------------------

#[test]
fn a_checked_member_becomes_typed_settings() {
    let s = settings_of(all_five());
    assert_eq!(s.effort, Effort::High);
    assert_eq!(s.command_timeout.map(CommandSeconds::get), Some(900));
    assert_eq!(s.compaction, Some(CompactionMode::Basic));
    assert_eq!(s.compaction_trigger.map(Trigger::get), Some(0.8));
    assert_eq!(s.disable_tools, vec![UserTool::FetchWebContent]);
    assert!(!s.is_empty());
    // Each key alone fills only its own field.
    assert_eq!(
        settings_of(json!({"reasoning": {"effort": "low"}})).effort,
        Effort::Low
    );
    assert_eq!(
        settings_of(json!({"reasoning": {"effort": "medium"}})).effort,
        Effort::Medium
    );
    let only = settings_of(json!({"commands": {"timeout_seconds": 1}}));
    assert_eq!(only.command_timeout.map(CommandSeconds::get), Some(1));
    assert_eq!(only.effort, Effort::Default);
    assert!(
        only.compaction.is_none()
            && only.compaction_trigger.is_none()
            && only.disable_tools.is_empty()
    );
    // Nothing set, or only the level `default`, places nothing.
    assert!(settings_of(json!({})).is_empty());
    assert!(settings_of(json!({"reasoning": {"effort": "default"}})).is_empty());
    assert!(settings_of(json!({"tools": {"disable": []}})).is_empty());
    assert_eq!(UserSettings::default(), settings_of(json!({})));
}

#[test]
fn a_member_that_was_not_checked_is_not_trusted() {
    // `from_checked` is called after the shared check; if it is ever called without, it still refuses a value of the wrong kind and
    // names the key, not the value, instead of falling back to a default.
    for (doc, key) in [
        (json!({"reasoning": {"effort": "max"}}), "reasoning.effort"),
        (json!({"reasoning": {"effort": 3}}), "reasoning.effort"),
        (
            json!({"commands": {"timeout_seconds": 0}}),
            "commands.timeout_seconds",
        ),
        (
            json!({"commands": {"timeout_seconds": 1801}}),
            "commands.timeout_seconds",
        ),
        (
            json!({"commands": {"timeout_seconds": "900"}}),
            "commands.timeout_seconds",
        ),
        (
            json!({"context": {"compaction": "zip"}}),
            "context.compaction",
        ),
        (
            json!({"context": {"compaction_trigger": 1.5}}),
            "context.compaction_trigger",
        ),
        (
            json!({"context": {"compaction_trigger": 0.05}}),
            "context.compaction_trigger",
        ),
        (json!({"tools": {"disable": ["rm_rf"]}}), "tools.disable"),
        (
            json!({"tools": {"disable": "spawn_agent"}}),
            "tools.disable",
        ),
    ] {
        let e = UserSettings::from_checked(&cfg(doc.clone())).unwrap_err();
        assert!(e.contains(key), "{doc}: {e}");
        assert!(
            !e.contains("rm_rf") && !e.contains("zip") && !e.contains("1801"),
            "{doc}: {e}"
        );
    }
}

#[test]
fn the_bounds_of_the_numbers_hold_by_construction() {
    assert!(CommandSeconds::new(0).is_none());
    assert_eq!(CommandSeconds::new(1).map(CommandSeconds::get), Some(1));
    assert_eq!(
        CommandSeconds::new(1800).map(CommandSeconds::get),
        Some(1800)
    );
    assert!(CommandSeconds::new(1801).is_none());
    assert!(CommandSeconds::new(u32::MAX).is_none());
    assert!(Trigger::new(f64::NAN).is_none());
    assert!(Trigger::new(f64::INFINITY).is_none());
    assert!(Trigger::new(0.09).is_none());
    assert!(Trigger::new(1.0).is_none());
    assert_eq!(Trigger::new(0.1).map(Trigger::get), Some(0.1));
    assert_eq!(Trigger::new(0.99).map(Trigger::get), Some(0.99));
}

// ---- the launch ---------------------------------------------------------------------------------------------------------------------

#[test]
fn launch_argv_has_thinking_only_when_effort_is_not_default() {
    let base = direct_launch_argv("cline", profile_argv(), Effort::Default);
    // The contract's argv of a run without the setting (`direct_launch.rs` pins it too).
    assert_eq!(
        base,
        words(&[
            "/opt/harness/bin/cline",
            "--acp",
            "--auto-approve",
            "true",
            "--config",
            "{configDir}",
            "--data-dir",
            "{dataDir}"
        ])
    );
    for (effort, level) in [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
    ] {
        let argv = direct_launch_argv("cline", profile_argv(), effort);
        // `--auto-approve true` stays where it was; the level follows the profile's own arguments.
        let mut want = base.clone();
        want.extend(words(&["--thinking", level]));
        assert_eq!(argv, want, "{level}");
        // A second pass adds nothing.
        assert_eq!(
            direct_launch_argv("cline", argv.clone(), effort),
            argv,
            "{level}"
        );
    }
    // A profile that already names a level gets the user's, once.
    let argv = direct_launch_argv(
        "cline",
        words(&[
            "/opt/harness/bin/cline",
            "--acp",
            "--thinking",
            "high",
            "--config",
            "c",
        ]),
        Effort::Low,
    );
    assert_eq!(
        argv.iter().filter(|a| *a == "--thinking").count(),
        1,
        "{argv:?}"
    );
    assert_eq!(argv.last().map(String::as_str), Some("low"), "{argv:?}");
    // Only Cline has the flag; an empty argv has nothing to carry it.
    for engine in ["fake-acp", "opencode"] {
        assert_eq!(
            direct_launch_argv(engine, words(&["x", "--acp"]), Effort::High),
            words(&["x", "--acp"]),
            "{engine}"
        );
    }
    assert!(direct_launch_argv("cline", Vec::new(), Effort::High).is_empty());
    assert_eq!(Effort::Default.flag(), None);
    assert_eq!(Effort::Low.flag(), Some("low"));
    assert_eq!(Effort::Medium.flag(), Some("medium"));
    assert_eq!(Effort::High.flag(), Some("high"));
}

#[test]
fn launch_env_converts_seconds_to_milliseconds_with_checked_arithmetic() {
    for (seconds, ms) in [
        (1, "1000"),
        (600, "600000"),
        (900, "900000"),
        (1800, "1800000"),
    ] {
        let s = settings_of(json!({"commands": {"timeout_seconds": seconds}}));
        let env = launch_env("cline", &s);
        assert_eq!(env.len(), 1, "{env:?}");
        assert_eq!(env["CLINE_RUN_COMMANDS_TIMEOUT_MS"], ms, "{seconds} s");
        assert_eq!(
            s.command_timeout.unwrap().millis(),
            ms.parse::<u64>().unwrap()
        );
    }
    // The largest value the type holds cannot overflow the product, whatever the width: 1,800 s is 1,800,000 ms.
    assert_eq!(CommandSeconds::new(1800).unwrap().millis(), 1_800_000);
    // Nothing set, no variable; another engine, no variable.
    assert!(launch_env("cline", &UserSettings::default()).is_empty());
    assert!(
        launch_env(
            "cline",
            &settings_of(json!({"reasoning": {"effort": "low"}}))
        )
        .is_empty()
    );
    assert!(launch_env("fake-acp", &settings_of(all_five())).is_empty());
}

// ---- the provider file ----------------------------------------------------------------------------------------------------------------

fn emit_ctx(
    label: &str,
    compaction: Option<agentd::engine::user_settings::CompactionBlock>,
) -> EmitContext {
    let d = scratch(label);
    EmitContext {
        engine: "cline".into(),
        model: "us.anthropic.claude-sonnet-5".into(),
        region: "us-west-2".into(),
        data_dir: d.join("data"),
        config_dir: d.join("config"),
        agentd_exe: PathBuf::from("/opt/harness/bin/agentd"),
        socket: PathBuf::from("/run/agentd/creds.sock"),
        max_output_tokens: Some(64_000),
        context_tokens: Some(1_000_000),
        compaction,
    }
}

#[test]
fn providers_json_keeps_updated_at_and_carries_compaction_as_typed_values() {
    let s = settings_of(all_five());
    let c = emit_ctx("emit-compaction", s.compaction_block());
    emit_engine(&c).unwrap();
    let providers = read_json(&c.data_dir.join("settings/providers.json"));
    let entry = &providers["providers"]["bedrock"];
    // The datetime stays: Cline's schema requires it and drops the whole file without it.
    assert!(entry["updatedAt"].as_str().unwrap().ends_with('Z'));
    let block = &entry["settings"]["compaction"];
    assert_eq!(
        *block,
        json!({"enabled": true, "strategy": "basic", "thresholdRatio": 0.8})
    );
    assert!(block["enabled"].is_boolean());
    assert!(block["strategy"].is_string());
    assert!(block["thresholdRatio"].is_f64());
    // The other settings are the ones the harness wrote before.
    assert_eq!(entry["settings"]["maxTokens"], 64_000);
    assert_eq!(entry["settings"]["contextWindow"], 1_000_000);
    assert_eq!(entry["settings"]["aws"]["authentication"], "iam");
}

#[test]
fn a_run_without_compaction_settings_writes_the_file_it_wrote_before() {
    let without = emit_ctx("emit-none-a", None);
    emit_engine(&without).unwrap();
    let with_other_keys = emit_ctx(
        "emit-none-b",
        settings_of(json!({"reasoning": {"effort": "low"}})).compaction_block(),
    );
    emit_engine(&with_other_keys).unwrap();
    for c in [&without, &with_other_keys] {
        let providers = read_json(&c.data_dir.join("settings/providers.json"));
        assert!(
            providers["providers"]["bedrock"]["settings"]
                .get("compaction")
                .is_none()
        );
    }
    assert!(settings_of(json!({})).compaction_block().is_none());
}

#[test]
fn each_setting_names_only_its_own_compaction_keys() {
    for (doc, want) in [
        (
            json!({"context": {"compaction": "agentic"}}),
            json!({"enabled": true, "strategy": "agentic"}),
        ),
        (
            json!({"context": {"compaction": "basic"}}),
            json!({"enabled": true, "strategy": "basic"}),
        ),
        (
            json!({"context": {"compaction": "off"}}),
            json!({"enabled": false}),
        ),
        (
            json!({"context": {"compaction_trigger": 0.5}}),
            json!({"thresholdRatio": 0.5}),
        ),
        (
            json!({"context": {"compaction": "off", "compaction_trigger": 0.99}}),
            json!({"enabled": false, "thresholdRatio": 0.99}),
        ),
    ] {
        let block = settings_of(doc.clone()).compaction_block().unwrap();
        assert_eq!(block.to_json(), want, "{doc}");
    }
}

// ---- the tools switched off ---------------------------------------------------------------------------------------------------------

#[test]
fn disabled_tools_is_the_union_and_a_user_cannot_remove_a_stage_entry() {
    let stage = words(&["a_stage_tool", "spawn_agent"]);
    let subsets: [Vec<&str>; 6] = [
        vec![],
        vec!["spawn_agent"],
        vec!["fetch_web_content"],
        vec!["spawn_agent", "fetch_web_content"],
        vec!["fetch_web_content", "spawn_agent"],
        vec![],
    ];
    for user in subsets {
        let s = settings_of(json!({"tools": {"disable": user}}));
        let union = s.disabled_tools(&stage);
        // The stage's entries come first, in their order, and every one of them is there.
        assert_eq!(union[..2], stage[..], "{user:?}");
        // No entry twice; the user's tools follow in the table's order.
        let mut sorted = union.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), union.len(), "{union:?}");
        for t in &user {
            assert!(union.iter().any(|u| u == t), "{t} in {union:?}");
        }
    }
    let s = settings_of(json!({"tools": {"disable": ["fetch_web_content", "spawn_agent"]}}));
    assert_eq!(
        s.disabled_tools(&words(&["a_stage_tool"])),
        words(&["a_stage_tool", "spawn_agent", "fetch_web_content"])
    );
    // No stage list: the user's alone. No user list: the stage's alone, as before this change.
    assert_eq!(
        s.disabled_tools(&[]),
        words(&["spawn_agent", "fetch_web_content"])
    );
    assert_eq!(UserSettings::default().disabled_tools(&stage), stage);
    assert_eq!(UserTool::SpawnAgent.name(), "spawn_agent");
    assert_eq!(UserTool::FetchWebContent.name(), "fetch_web_content");
}

/// Every object key of a JSON document, at any depth.
fn all_keys(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                out.push(k.clone());
                all_keys(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| all_keys(x, out)),
        _ => {}
    }
}

#[test]
fn every_accepted_document_leaves_the_locked_keys_in_force_in_the_written_files() {
    // The whole space the table accepts, at its edges: 5 levels x 3 time limits x 4 compaction modes x 3 triggers x 6 tool lists = 1,080
    // documents. For each: the shared check accepts it, the files are written the way the startup writes them (provider file, then the
    // lock), and the three locked keys hold whatever the document said.
    let efforts = [
        None,
        Some("default"),
        Some("low"),
        Some("medium"),
        Some("high"),
    ];
    let timeouts = [None, Some(1), Some(1800)];
    let modes = [None, Some("agentic"), Some("basic"), Some("off")];
    let triggers = [None, Some(0.1), Some(0.99)];
    let tools: [Option<Vec<&str>>; 6] = [
        None,
        Some(vec![]),
        Some(vec!["spawn_agent"]),
        Some(vec!["fetch_web_content"]),
        Some(vec!["spawn_agent", "fetch_web_content"]),
        Some(vec!["fetch_web_content", "spawn_agent"]),
    ];
    let allowed: Vec<&str> = [
        "version",
        "lastUsedProvider",
        "modes",
        "providers",
        "bedrock",
        "settings",
        "provider",
        "model",
        "aws",
        "authentication",
        "region",
        "contextWindow",
        "maxTokens",
        "compaction",
        "enabled",
        "strategy",
        "thresholdRatio",
        "updatedAt",
        "tokenSource",
        "telemetryOptOut",
        "autoUpdateEnabled",
        "disabledTools",
    ]
    .to_vec();
    let root = scratch("every-document");
    let stage = words(&["a_stage_tool"]);
    let mut n = 0;
    for effort in efforts {
        for timeout in timeouts {
            for mode in modes {
                for trigger in triggers {
                    for tool_list in &tools {
                        let mut doc = serde_json::Map::new();
                        if let Some(e) = effort {
                            doc.insert("reasoning".into(), json!({"effort": e}));
                        }
                        if let Some(t) = timeout {
                            doc.insert("commands".into(), json!({"timeout_seconds": t}));
                        }
                        let mut context = serde_json::Map::new();
                        if let Some(m) = mode {
                            context.insert("compaction".into(), json!(m));
                        }
                        if let Some(t) = trigger {
                            context.insert("compaction_trigger".into(), json!(t));
                        }
                        if !context.is_empty() {
                            doc.insert("context".into(), Value::Object(context));
                        }
                        if let Some(l) = tool_list {
                            doc.insert("tools".into(), json!({"disable": l}));
                        }
                        let c = cfg(Value::Object(doc));
                        check_engine_config(&c).unwrap();
                        let s = UserSettings::from_checked(&c).unwrap();
                        let d = root.join(format!("d{n}"));
                        n += 1;
                        let ctx = EmitContext {
                            engine: "cline".into(),
                            model: "m".into(),
                            region: "us-east-1".into(),
                            data_dir: d.join("data"),
                            config_dir: d.join("config"),
                            agentd_exe: PathBuf::from("/opt/harness/bin/agentd"),
                            socket: PathBuf::from("/run/agentd/creds.sock"),
                            max_output_tokens: Some(8192),
                            context_tokens: Some(200_000),
                            compaction: s.compaction_block(),
                        };
                        emit_engine(&ctx).unwrap();
                        let mut env = BTreeMap::new();
                        let locked = lock_global_settings(
                            "cline",
                            &ctx.config_dir,
                            &s.disabled_tools(&stage),
                            &mut env,
                        )
                        .unwrap()
                        .unwrap();
                        let doc_locked = read_json(&locked);
                        assert_eq!(doc_locked["telemetryOptOut"], true, "{c:?}");
                        assert_eq!(doc_locked["autoUpdateEnabled"], false, "{c:?}");
                        let list = doc_locked["disabledTools"].as_array().unwrap();
                        assert!(list.contains(&json!("a_stage_tool")), "{c:?}");
                        let providers = read_json(&ctx.data_dir.join("settings/providers.json"));
                        assert_eq!(
                            providers["providers"]["bedrock"]["settings"]["provider"],
                            "bedrock"
                        );
                        // Only keys the harness names are in the files: nothing of the document's own spelling.
                        let mut keys = Vec::new();
                        all_keys(&providers, &mut keys);
                        all_keys(&doc_locked, &mut keys);
                        for k in &keys {
                            assert!(
                                allowed.contains(&k.as_str()),
                                "key {k} in the written files"
                            );
                        }
                    }
                }
            }
        }
    }
    assert_eq!(n, 5 * 3 * 4 * 3 * 6);
}

// ---- the request as agentd reads it ---------------------------------------------------------------------------------------------------

fn request_with(member: Option<Value>) -> StartDirectRunRequest {
    let mut v = json!({"prompt": "p", "target": {"mode": "s3-only"}, "model": "m", "rules": []});
    if let Some(m) = member {
        v["engineConfig"] = m;
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn the_request_without_the_member_gives_no_settings() {
    assert_eq!(
        engine_settings(&request_with(None), "cline").unwrap(),
        UserSettings::default()
    );
    // An engine this build has no table for places nothing and says nothing when the request has no member.
    assert_eq!(
        engine_settings(&request_with(None), "fake-acp").unwrap(),
        UserSettings::default()
    );
}

#[test]
fn the_request_check_is_the_shared_one_and_never_echoes_a_value() {
    let ok = request_with(Some(
        json!({"engine": "cline", "schema": 1, "settings": all_five()}),
    ));
    assert_eq!(
        engine_settings(&ok, "cline").unwrap(),
        settings_of(all_five())
    );
    for (settings, key, rule, value) in [
        (
            json!({"aws": {"profile": "marker-profile-9x"}}),
            "aws.profile",
            "credentials",
            "marker-profile-9x",
        ),
        (
            json!({"reasoning": {"effort": "marker-level-9x"}}),
            "reasoning.effort",
            "must be one of",
            "marker-level-9x",
        ),
        (
            json!({"tools": {"disable": ["spawn_agent", "spawn_agent"]}}),
            "tools.disable",
            "lists a value twice",
            "no-such-marker-9x",
        ),
        (
            json!({"context": {"compaction_trigger": 1}}),
            "context.compaction_trigger",
            "must be a number from",
            "no-such-marker-9x",
        ),
    ] {
        let r = request_with(Some(
            json!({"engine": "cline", "schema": 1, "settings": settings}),
        ));
        let e = engine_settings(&r, "cline").unwrap_err();
        assert!(e.contains(key) && e.contains(rule), "{e}");
        assert!(!e.contains(value), "{e}");
    }
    // The member's own shape: a schema that is not 1, a key with unusual characters (named as such), a list far over the bound.
    let bad_schema = request_with(Some(
        json!({"engine": "cline", "schema": 2, "settings": {}}),
    ));
    assert!(
        engine_settings(&bad_schema, "cline")
            .unwrap_err()
            .contains("schema must be 1")
    );
    let odd = request_with(Some(
        json!({"engine": "cline", "schema": 1, "settings": {"reasoning": {"eff\u{43e}rt": "high"}}}),
    ));
    let e = engine_settings(&odd, "cline").unwrap_err();
    assert!(e.contains("a key with unusual characters"), "{e}");
    let long: Vec<String> = (0..10_000).map(|i| format!("t{i}")).collect();
    let big = request_with(Some(
        json!({"engine": "cline", "schema": 1, "settings": {"tools": {"disable": long}}}),
    ));
    assert!(engine_settings(&big, "cline").is_err());
}

#[test]
fn a_member_for_another_engine_than_the_runs_is_refused_and_not_ignored() {
    let r = request_with(Some(
        json!({"engine": "cline", "schema": 1, "settings": {"reasoning": {"effort": "low"}}}),
    ));
    let e = engine_settings(&r, "fake-acp").unwrap_err();
    assert!(e.contains("another engine"), "{e}");
    let other = request_with(Some(
        json!({"engine": "fake-acp", "schema": 1, "settings": {}}),
    ));
    assert!(
        engine_settings(&other, "cline")
            .unwrap_err()
            .contains("no settings for that engine")
    );
}

// ---- the read-back ----------------------------------------------------------------------------------------------------------------------

/// A startup's worth of placement for these settings, written the way the startup writes it.
struct Placement {
    config_dir: PathBuf,
    data_dir: PathBuf,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    stage: Vec<String>,
    settings: UserSettings,
}

fn placement(label: &str, doc: Value) -> Placement {
    let settings = settings_of(doc);
    let stage = words(&["a_stage_tool"]);
    let c = emit_ctx(label, settings.compaction_block());
    emit_engine(&c).unwrap();
    let mut env = launch_env("cline", &settings);
    lock_global_settings(
        "cline",
        &c.config_dir,
        &settings.disabled_tools(&stage),
        &mut env,
    )
    .unwrap()
    .unwrap();
    Placement {
        config_dir: c.config_dir,
        data_dir: c.data_dir,
        argv: direct_launch_argv("cline", profile_argv(), settings.effort),
        env,
        stage,
        settings,
    }
}

impl Placement {
    fn read_back(&self) -> Result<(), String> {
        read_back(
            &Placed {
                engine: "cline",
                config_dir: &self.config_dir,
                data_dir: &self.data_dir,
                argv: &self.argv,
                env: &self.env,
                stage_disabled: &self.stage,
            },
            &self.settings,
        )
    }

    fn providers(&self) -> PathBuf {
        self.data_dir.join("settings/providers.json")
    }

    fn locked(&self) -> PathBuf {
        self.config_dir.join("locked/global-settings.json")
    }

    /// Rewrites a file the startup made read-only, as a test that stands for the engine user or a module.
    fn rewrite(&self, path: &Path, edit: impl FnOnce(&mut Value)) {
        use std::os::unix::fs::PermissionsExt;
        let dir = path.parent().unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut v = read_json(path);
        edit(&mut v);
        std::fs::write(path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }
}

#[test]
fn the_read_back_accepts_what_was_written() {
    placement("rb-ok", all_five()).read_back().unwrap();
    // A run with only some keys, and with none that place anything, reads back clean too.
    placement("rb-low", json!({"reasoning": {"effort": "low"}}))
        .read_back()
        .unwrap();
    placement("rb-empty", json!({})).read_back().unwrap();
}

#[test]
fn a_read_back_mismatch_names_the_file_and_never_a_value() {
    let p = placement("rb-providers", all_five());
    p.rewrite(&p.providers(), |v| {
        v["providers"]["bedrock"]["settings"]["compaction"]["thresholdRatio"] = json!(0.5);
    });
    let e = p.read_back().unwrap_err();
    assert!(
        e.contains("engine settings") && e.contains("provider file"),
        "{e}"
    );
    assert!(!e.contains("0.5") && !e.contains("0.8"), "{e}");

    let p = placement("rb-providers-missing", all_five());
    p.rewrite(&p.providers(), |v| {
        v["providers"]["bedrock"]["settings"]
            .as_object_mut()
            .unwrap()
            .remove("compaction");
    });
    assert!(p.read_back().unwrap_err().contains("provider file"));

    let p = placement("rb-providers-date", all_five());
    p.rewrite(&p.providers(), |v| {
        v["providers"]["bedrock"]
            .as_object_mut()
            .unwrap()
            .remove("updatedAt");
    });
    assert!(p.read_back().unwrap_err().contains("provider file"));

    let p = placement("rb-locked-tools", all_five());
    p.rewrite(&p.locked(), |v| v["disabledTools"] = json!([]));
    let e = p.read_back().unwrap_err();
    assert!(e.contains("global settings"), "{e}");
    assert!(
        !e.contains("fetch_web_content") && !e.contains("a_stage_tool"),
        "{e}"
    );

    let p = placement("rb-locked-telemetry", all_five());
    p.rewrite(&p.locked(), |v| v["telemetryOptOut"] = json!(false));
    assert!(p.read_back().unwrap_err().contains("global settings"));

    let p = placement("rb-locked-update", all_five());
    p.rewrite(&p.locked(), |v| v["autoUpdateEnabled"] = json!(true));
    assert!(p.read_back().unwrap_err().contains("global settings"));

    let mut p = placement("rb-path", all_five());
    p.env.insert(
        "CLINE_GLOBAL_SETTINGS_PATH".into(),
        "/elsewhere/global-settings.json".into(),
    );
    assert!(p.read_back().unwrap_err().contains("global settings"));

    let mut p = placement("rb-env", all_five());
    p.env.remove("CLINE_RUN_COMMANDS_TIMEOUT_MS");
    let e = p.read_back().unwrap_err();
    assert!(e.contains("environment"), "{e}");
    let mut p = placement("rb-env-value", all_five());
    p.env
        .insert("CLINE_RUN_COMMANDS_TIMEOUT_MS".into(), "1000".into());
    assert!(p.read_back().unwrap_err().contains("environment"));

    let mut p = placement("rb-argv", all_five());
    p.argv.truncate(p.argv.len() - 2);
    let e = p.read_back().unwrap_err();
    assert!(e.contains("launch"), "{e}");
    // What the user did not set is not looked at: a stage may set its own default for the variable or the level.
    let mut p = placement("rb-unset", json!({"reasoning": {"effort": "low"}}));
    p.env
        .insert("CLINE_RUN_COMMANDS_TIMEOUT_MS".into(), "1000".into());
    p.read_back().unwrap();
}

#[test]
fn an_engine_this_build_cannot_place_settings_for_is_refused() {
    let p = placement("rb-engine", all_five());
    let e = read_back(
        &Placed {
            engine: "fake-acp",
            config_dir: &p.config_dir,
            data_dir: &p.data_dir,
            argv: &p.argv,
            env: &p.env,
            stage_disabled: &p.stage,
        },
        &p.settings,
    )
    .unwrap_err();
    assert!(e.contains("engine settings"), "{e}");
}

// ---- the event ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn the_event_drops_settings_from_the_end_until_it_fits_the_inline_limit() {
    use agentd::direct::engine_config::event;
    let c = cfg(all_five());
    let mut reference = c.to_ref();
    let full = event(&c, &reference);
    assert_eq!(full["settings"].as_array().unwrap().len(), 5);
    assert!(serde_json::to_vec(&full).unwrap().len() < 4096);
    // A reference that leaves little room: entries go from the end, so the ones that stay are the first of the table, in its order.
    reference.sha256 = "a".repeat(3800);
    let ev = event(&c, &reference);
    let kept: Vec<&str> = ev["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["key"].as_str().unwrap())
        .collect();
    let table = [
        "reasoning.effort",
        "commands.timeout_seconds",
        "context.compaction",
        "context.compaction_trigger",
        "tools.disable",
    ];
    assert!(kept.len() < 5, "{kept:?}");
    assert_eq!(kept, table[..kept.len()]);
    assert!(kept.is_empty() || serde_json::to_vec(&ev).unwrap().len() <= 4096);
    // No room at all: no entry is left, and the event still names the hash and the count.
    reference.sha256 = "a".repeat(5000);
    let none = event(&c, &reference);
    assert!(none["settings"].as_array().unwrap().is_empty());
    assert_eq!(none["keys"], 5);
    assert_eq!(none["readBack"], "ok");
}

// ---- the typed values and the protocol's key table ----------------------------------------------------------------------------------

#[test]
fn the_typed_values_match_the_key_table_of_the_protocol() {
    use harness_protocol::direct::{CLINE_KEYS, KeyKind};
    // The protocol's table decides what the shared check accepts; the typed values decide what agentd can place. A key added to the table without
    // a typed value here, or a range widened on one side only, would make a member that passed the check fail at startup: this test names it first.
    assert_eq!(
        CLINE_KEYS.len(),
        5,
        "a new key needs a typed value in agentd"
    );
    for row in CLINE_KEYS {
        match (row.path, row.kind) {
            ("reasoning.effort", KeyKind::Choice(levels)) => {
                for level in levels {
                    let s = settings_of(json!({"reasoning": {"effort": level}}));
                    // Every level of the table is typed; `default` and `off` are the two that add no argument themselves.
                    let word = match s.effort {
                        Effort::Default => "default",
                        Effort::Off => "off",
                        other => other.flag().unwrap(),
                    };
                    assert_eq!(word, *level);
                }
                for refused in ["none", "max", "minimal"] {
                    assert!(!levels.contains(&refused), "{refused}");
                    let c = cfg(json!({"reasoning": {"effort": refused}}));
                    assert!(UserSettings::from_checked(&c).is_err(), "{refused}");
                }
            }
            ("commands.timeout_seconds", KeyKind::Int { min, max }) => {
                assert_eq!(i64::from(CommandSeconds::MIN), min);
                assert_eq!(i64::from(CommandSeconds::MAX), max);
            }
            ("context.compaction", KeyKind::Choice(modes)) => {
                assert_eq!(modes.len(), 3);
                for mode in modes {
                    let s = settings_of(json!({"context": {"compaction": mode}}));
                    assert!(s.compaction.is_some(), "{mode}");
                }
            }
            ("context.compaction_trigger", KeyKind::Float { min, max }) => {
                assert_eq!(Trigger::MIN, min);
                assert_eq!(Trigger::MAX, max);
            }
            ("tools.disable", KeyKind::Set(tools)) => {
                assert_eq!(tools.len(), 2);
                for tool in tools {
                    let s = settings_of(json!({"tools": {"disable": [tool]}}));
                    assert_eq!(s.disable_tools.len(), 1, "{tool}");
                    assert_eq!(s.disable_tools[0].name(), *tool);
                }
            }
            (path, kind) => panic!("{path} ({kind:?}) has no typed value in agentd"),
        }
    }
}
