//! The engine settings of a direct run, applied by agentd: the optional `engineConfig`
//! member of the stored start request becomes the engine's argv (`--thinking <level>`), one environment variable
//! (`CLINE_RUN_COMMANDS_TIMEOUT_MS` in the Cline profile), the compaction block of the provider file and the tools switched off in the locked
//! global settings. Each test runs the whole agent in process against the fake AWS and the fake ACP engine, with the product's module set.
//!
//! A run without the member is the run it was: the 13 golden streams of `direct_golden.rs` changed by exactly one line in package (the
//! protocol version in `summary.json`, 1.38.0 to 1.39.0) and by nothing in this package; no argv word, no variable, no key of the provider file, no
//! event and no summary member is added. A member the shared check refuses ends the run `stopped_setup_failed` ("did not start") before any model
//! call, with the key and the rule and never the value.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts, PROMPT, RUN, sha};
use harness_protocol::direct::{
    DIRECT_TURN, DirectEngineConfigRef, DirectOutcome, EngineConfig, s3 as keys,
};
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Puts members into the request the run fetches, and the hash the invocation carries.
fn with_members(d: &mut Direct, members: Value) {
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    for (k, val) in members.as_object().unwrap() {
        v[k] = val.clone();
    }
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    d.req.request.sha256 = sha(&bytes);
    d.request_json = bytes;
}

/// The product's module set: no `Deps.hooks`, so the driver registers `Hooks::standard`.
async fn run(d: &Direct) -> InvocationResponse {
    agentd::direct::run_with(&d.app, d.req.clone(), &agentd::direct::Deps::default()).await
}

/// The lines of one section of the probe file the engine wrapper writes (`--- argv`, `--- env`, `--- settings`).
fn probe_section(d: &Direct, name: &str) -> Vec<String> {
    let probe = d.engine_probe();
    let mut inside = false;
    let mut out = Vec::new();
    for line in probe.lines() {
        if let Some(h) = line.strip_prefix("--- ") {
            inside = h == name;
        } else if inside {
            out.push(line.to_string());
        }
    }
    out
}

fn argv_of(d: &Direct) -> Vec<String> {
    probe_section(d, "argv")
}

fn env_of(d: &Direct, name: &str) -> Option<String> {
    probe_section(d, "env")
        .iter()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .map(str::to_string)
}

fn providers_path(d: &Direct) -> PathBuf {
    d.agent_data().join("data/settings/providers.json")
}

fn locked_path(d: &Direct) -> PathBuf {
    d.agent_data().join("config/locked/global-settings.json")
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn engine_config_events(d: &Direct) -> Vec<Value> {
    d.aws
        .progress("engine-config")
        .into_iter()
        .map(|(_, v)| v)
        .collect()
}

// ---- a run without the member is the run it was ----------------------------------------------------------------------------------------

/// The text of every golden stream, by file name.
fn goldens() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut all: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read_to_string(&p).unwrap(),
            )
        })
        .collect();
    all.sort();
    all
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_without_the_member_leaves_the_13_golden_streams_unchanged() {
    // The streams: 13 of them, each with the one protocol line the latest bumps changed and nothing of
    // the engine settings. The byte-for-byte comparison with a fresh run is `direct_golden.rs`; this test pins what may NOT appear in them.
    let all = goldens();
    assert_eq!(
        all.len(),
        13,
        "{:?}",
        all.iter().map(|g| &g.0).collect::<Vec<_>>()
    );
    for (name, text) in &all {
        assert_eq!(
            text.matches("\"protocol\": \"1.59.0\"").count(),
            1,
            "{name}: one protocol line, the one the latest bump changed"
        );
        assert!(!text.contains("\"protocol\": \"1.40.0\""), "{name}");
        for word in [
            "engine-config",
            "engineConfig",
            "--thinking",
            "CLINE_RUN_COMMANDS_TIMEOUT_MS",
            "thresholdRatio",
        ] {
            assert!(!text.contains(word), "{name} holds {word}");
        }
    }
    // A run with no member: nothing is added to the launch, the environment, the provider file, the events or the summary.
    let d = Direct::start(Opts::default()).await;
    let resp = run(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    let argv = argv_of(&d);
    assert!(argv.iter().any(|a| a == "--auto-approve"), "{argv:?}");
    assert!(!argv.iter().any(|a| a == "--thinking"), "{argv:?}");
    assert!(env_of(&d, "CLINE_RUN_COMMANDS_TIMEOUT_MS").is_none());
    let providers = read_json(&providers_path(&d));
    let settings = &providers["providers"]["bedrock"]["settings"];
    assert!(settings.get("compaction").is_none(), "{settings}");
    assert_eq!(read_json(&locked_path(&d))["disabledTools"], json!([]));
    assert!(engine_config_events(&d).is_empty());
    let summary = serde_json::to_value(d.summary()).unwrap();
    assert!(summary.get("engineConfig").is_none(), "{summary}");
    assert!(
        d.log.lines().iter().all(|l| !l.contains("engine settings")),
        "{:?}",
        d.log.lines()
    );
    println!(
        "ENGINE-CONFIG NEGATIVE PRESENT: no member -> 13 goldens hold only the protocol line, no --thinking, no variable, no compaction key, no event, no summary member"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_with_no_key_places_nothing_and_says_nothing() {
    // The client sends nothing for a file with only `schema = 1`; a hand-made request can carry a member that sets nothing. It is accepted
    // and changes nothing: the run is the run without the member.
    let mut d = Direct::start(Opts::default()).await;
    with_members(
        &mut d,
        json!({"engineConfig": {"engine": "cline", "schema": 1, "settings": {}}}),
    );
    let resp = run(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert!(!argv_of(&d).iter().any(|a| a == "--thinking"));
    assert!(env_of(&d, "CLINE_RUN_COMMANDS_TIMEOUT_MS").is_none());
    assert!(
        read_json(&providers_path(&d))["providers"]["bedrock"]["settings"]
            .get("compaction")
            .is_none()
    );
    assert!(engine_config_events(&d).is_empty());
    let summary = serde_json::to_value(d.summary()).unwrap();
    assert!(summary.get("engineConfig").is_none(), "{summary}");
}

// ---- the member, applied ---------------------------------------------------------------------------------------------------------------

/// The `engineConfig` member of the Cline engine with these settings.
fn member(settings: Value) -> Value {
    json!({"engineConfig": {"engine": "cline", "schema": 1, "settings": settings}})
}

/// All five keys of the table, with values that are not the defaults.
fn all_five() -> Value {
    json!({
        "reasoning": {"effort": "high"},
        "commands": {"timeout_seconds": 900},
        "context": {"compaction": "basic", "compaction_trigger": 0.8},
        "tools": {"disable": ["fetch_web_content"]}
    })
}

/// A stage that already switches one tool off (`AGENTD_DISABLED_TOOLS`).
fn opts_with_stage_tool() -> Opts {
    Opts {
        edit_cfg: Some(Box::new(|c| {
            c.direct.disabled_tools = vec!["a_stage_tool".to_string()];
        })),
        ..Opts::default()
    }
}

/// One whole run of the product with this member in the stored request.
async fn run_with_member(opts: Opts, settings: Value) -> Direct {
    let mut d = Direct::start(opts).await;
    with_members(&mut d, member(settings));
    let resp = run(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    d
}

/// The value after `flag` in the engine's argv, when the flag is there.
fn flag_value(argv: &[String], flag: &str) -> Option<String> {
    let at = argv.iter().position(|a| a == flag)?;
    argv.get(at + 1).cloned()
}

#[tokio::test(flavor = "multi_thread")]
async fn launch_argv_has_thinking_only_when_effort_is_not_default() {
    for level in ["low", "medium", "high"] {
        let d = run_with_member(Opts::default(), json!({"reasoning": {"effort": level}})).await;
        let argv = argv_of(&d);
        assert_eq!(
            flag_value(&argv, "--thinking").as_deref(),
            Some(level),
            "{argv:?}"
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--thinking").count(),
            1,
            "{argv:?}"
        );
        // The permission flag the direct launch always adds stays where it was.
        assert_eq!(
            flag_value(&argv, "--auto-approve").as_deref(),
            Some("true"),
            "{argv:?}"
        );
    }
    // `default` is a key that is set and adds nothing to the launch (the engine uses its own level).
    let d = run_with_member(Opts::default(), json!({"reasoning": {"effort": "default"}})).await;
    let argv = argv_of(&d);
    assert!(!argv.iter().any(|a| a == "--thinking"), "{argv:?}");
    assert_eq!(
        flag_value(&argv, "--auto-approve").as_deref(),
        Some("true"),
        "{argv:?}"
    );
    println!(
        "ENGINE-CONFIG TRIGGER PRESENT: --thinking low|medium|high reached the engine argv; default added nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn launch_env_converts_seconds_to_milliseconds_with_checked_arithmetic() {
    for (seconds, ms) in [(1, "1000"), (900, "900000"), (1800, "1800000")] {
        let d = run_with_member(
            Opts::default(),
            json!({"commands": {"timeout_seconds": seconds}}),
        )
        .await;
        assert_eq!(
            env_of(&d, "CLINE_RUN_COMMANDS_TIMEOUT_MS").as_deref(),
            Some(ms),
            "{seconds} s"
        );
    }
    // Another key set, this one not: the variable is not made up.
    let d = run_with_member(Opts::default(), json!({"reasoning": {"effort": "low"}})).await;
    assert!(env_of(&d, "CLINE_RUN_COMMANDS_TIMEOUT_MS").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn providers_json_keeps_updated_at_and_carries_compaction_as_typed_values() {
    let d = run_with_member(Opts::default(), all_five()).await;
    let providers = read_json(&providers_path(&d));
    let entry = &providers["providers"]["bedrock"];
    // The datetime Cline's schema requires: without it Cline drops the whole file with no error.
    let updated = entry["updatedAt"].as_str().unwrap();
    let shape = regex::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z$").unwrap();
    assert!(shape.is_match(updated), "{updated}");
    assert_eq!(entry["tokenSource"], "manual");
    let settings = &entry["settings"];
    // The compaction block holds typed values: a bool, a text of the closed set, a number.
    assert_eq!(
        settings["compaction"],
        json!({"enabled": true, "strategy": "basic", "thresholdRatio": 0.8})
    );
    assert!(settings["compaction"]["enabled"].is_boolean());
    assert!(settings["compaction"]["thresholdRatio"].is_f64());
    // What the harness wrote before stays as it was.
    assert_eq!(settings["provider"], "bedrock");
    assert_eq!(settings["model"], direct_support::MODEL);
    assert_eq!(settings["aws"]["authentication"], "iam");
    assert_eq!(settings["contextWindow"], 1_000_000);
    assert_eq!(settings["maxTokens"], 128_000);
    assert_eq!(providers["lastUsedProvider"], "bedrock");
}

#[tokio::test(flavor = "multi_thread")]
async fn each_compaction_setting_writes_only_its_own_keys() {
    let cases = [
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
    ];
    for (settings, want) in cases {
        let d = run_with_member(Opts::default(), settings.clone()).await;
        let providers = read_json(&providers_path(&d));
        assert_eq!(
            providers["providers"]["bedrock"]["settings"]["compaction"], want,
            "{settings}"
        );
    }
    // Keys that are not about compaction leave the provider file as the harness wrote it.
    let d = run_with_member(Opts::default(), json!({"reasoning": {"effort": "low"}})).await;
    assert!(
        read_json(&providers_path(&d))["providers"]["bedrock"]["settings"]
            .get("compaction")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_tools_is_the_union_and_a_user_cannot_remove_a_stage_entry() {
    let disabled = |d: &Direct| read_json(&locked_path(d))["disabledTools"].clone();
    // Stage [a_stage_tool], user [spawn_agent]: both.
    let d = run_with_member(
        opts_with_stage_tool(),
        json!({"tools": {"disable": ["spawn_agent"]}}),
    )
    .await;
    assert_eq!(disabled(&d), json!(["a_stage_tool", "spawn_agent"]));
    // Both of the user's tools, in the order of the table whichever order the file used.
    let d = run_with_member(
        opts_with_stage_tool(),
        json!({"tools": {"disable": ["fetch_web_content", "spawn_agent"]}}),
    )
    .await;
    assert_eq!(
        disabled(&d),
        json!(["a_stage_tool", "spawn_agent", "fetch_web_content"])
    );
    // A tool both name is listed once.
    let opts = Opts {
        edit_cfg: Some(Box::new(|c| {
            c.direct.disabled_tools = vec!["spawn_agent".to_string()];
        })),
        ..Opts::default()
    };
    let d = run_with_member(opts, json!({"tools": {"disable": ["spawn_agent"]}})).await;
    assert_eq!(disabled(&d), json!(["spawn_agent"]));
    // A member that names no tool leaves the stage list as it is, and an empty list removes nothing.
    let d = run_with_member(opts_with_stage_tool(), json!({"tools": {"disable": []}})).await;
    assert_eq!(disabled(&d), json!(["a_stage_tool"]));
    let d = run_with_member(
        opts_with_stage_tool(),
        json!({"reasoning": {"effort": "low"}}),
    )
    .await;
    assert_eq!(disabled(&d), json!(["a_stage_tool"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn any_valid_member_leaves_the_three_locked_keys_in_force() {
    // Documents at the edges of every range and with every key set. The enumerated property over the whole accepted space is in
    // `engine_user_settings.rs`; this one goes through the real startup.
    let documents = [
        json!({"reasoning": {"effort": "low"}, "commands": {"timeout_seconds": 1}, "context": {"compaction": "off", "compaction_trigger": 0.1}, "tools": {"disable": []}}),
        json!({"commands": {"timeout_seconds": 1800}, "context": {"compaction_trigger": 0.99}, "tools": {"disable": ["spawn_agent", "fetch_web_content"]}}),
        all_five(),
        json!({"context": {"compaction": "agentic"}}),
    ];
    for doc in documents {
        let d = run_with_member(opts_with_stage_tool(), doc.clone()).await;
        let locked = read_json(&locked_path(&d));
        assert_eq!(locked["telemetryOptOut"], true, "{doc}");
        assert_eq!(locked["autoUpdateEnabled"], false, "{doc}");
        let tools = locked["disabledTools"].as_array().unwrap();
        assert!(tools.contains(&json!("a_stage_tool")), "{doc}: {tools:?}");
        // The engine reads that file, and no other one.
        assert_eq!(
            env_of(&d, "CLINE_GLOBAL_SETTINGS_PATH").as_deref(),
            Some(locked_path(&d).display().to_string().as_str()),
            "{doc}"
        );
        // The provider and the credentials are not a user key.
        let entry = &read_json(&providers_path(&d))["providers"]["bedrock"]["settings"];
        assert_eq!(entry["provider"], "bedrock", "{doc}");
        assert_eq!(entry["aws"]["authentication"], "iam", "{doc}");
    }
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

#[tokio::test(flavor = "multi_thread")]
async fn no_user_key_name_appears_in_any_rendered_file_and_no_string_is_interpolated() {
    let d = run_with_member(Opts::default(), all_five()).await;
    let mut keys = Vec::new();
    all_keys(&read_json(&providers_path(&d)), &mut keys);
    all_keys(&read_json(&locked_path(&d)), &mut keys);
    for user_key in [
        "reasoning",
        "effort",
        "commands",
        "timeout_seconds",
        "context",
        "compaction_trigger",
        "tools",
        "disable",
        "schema",
        "engine",
    ] {
        assert!(
            !keys.iter().any(|k| k == user_key),
            "{user_key} in {keys:?}"
        );
    }
    // The values the user's strings became are words of the harness's own: nothing the file said is copied.
    let text = std::fs::read_to_string(providers_path(&d)).unwrap()
        + &std::fs::read_to_string(locked_path(&d)).unwrap();
    for user_text in ["timeout_seconds", "compaction_trigger", "\"schema\""] {
        assert!(!text.contains(user_text), "{user_text}");
    }
    // The sources that write the files take typed values through `json!`; the template renderer (`emit::render`, raw text into a template)
    // is never used for them.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in [
        "engine/user_settings.rs",
        "engine/config.rs",
        "direct/engine_config/mod.rs",
    ] {
        let text =
            std::fs::read_to_string(src.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert!(
            !text.contains("render("),
            "{file} uses the template renderer"
        );
        assert!(
            !text.contains(".replace("),
            "{file} builds text by replacement"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tampered_member_with_a_denied_key_ends_stopped_setup_failed_before_any_model_call() {
    // Each case: the settings a hand-made request can carry (ham and control-api would have refused them), the words the detail must hold, and
    // the value that must not be in it.
    let cases = [
        (
            json!({"aws": {"profile": "marker-profile-9x"}}),
            "aws.profile",
            "the harness supplies credentials itself",
            "marker-profile-9x",
        ),
        (
            json!({"hooks": {"command": "marker-cmd-9x"}}),
            "hooks.command",
            "this would run code or move a protected folder",
            "marker-cmd-9x",
        ),
        (
            json!({"env": {"x": "marker-env-9x"}}),
            "env.x",
            "this would run code or move a protected folder",
            "marker-env-9x",
        ),
        (
            json!({"reasoning": {"effort": "marker-level-9x"}}),
            "reasoning.effort",
            "must be one of",
            "marker-level-9x",
        ),
        (
            json!({"commands": {"timeout_seconds": 1801}}),
            "commands.timeout_seconds",
            "a whole number from 1 to 1800",
            "1801",
        ),
    ];
    for (settings, key, rule, value) in cases {
        let mut d = Direct::start(Opts::default()).await;
        with_members(&mut d, member(settings.clone()));
        let resp = run(&d).await;
        assert_ne!(resp.outcome, TurnOutcome::Completed, "{settings}: {resp:?}");
        let s = d.summary();
        assert_eq!(
            s.outcome,
            DirectOutcome::StoppedSetupFailed,
            "{settings}: {s:?}"
        );
        assert_eq!(s.label, "Cline did not start", "{settings}");
        let detail = s.detail.clone().unwrap();
        assert!(detail.contains(key), "{settings}: {detail}");
        assert!(detail.contains(rule), "{settings}: {detail}");
        assert!(!detail.contains(value), "{settings}: {detail}");
        assert!(
            !d.log.lines().iter().any(|l| l.contains(value)),
            "{settings}"
        );
        // Before any model call and before anything is placed: the engine never ran and no file was written for it.
        assert!(d.prompts_received().is_empty(), "{settings}");
        assert!(
            !d.agent_data().join("home/engine-probe.txt").exists(),
            "{settings}"
        );
        assert!(!providers_path(&d).exists(), "{settings}");
        assert!(engine_config_events(&d).is_empty(), "{settings}");
        assert!(
            serde_json::to_value(&s)
                .unwrap()
                .get("engineConfig")
                .is_none()
        );
    }
    println!(
        "ENGINE-CONFIG TRIGGER PRESENT: 5 tampered members ended stopped_setup_failed with the key and the rule and no value"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_for_another_engine_than_the_runs_is_refused_and_not_ignored() {
    // The run's engine has no key table in this build: the member would be set and never applied.
    let mut d = Direct::start(Opts {
        engine: "fake-acp",
        ..Opts::default()
    })
    .await;
    with_members(&mut d, member(json!({"reasoning": {"effort": "low"}})));
    let resp = run(&d).await;
    assert_ne!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    assert!(
        s.detail.unwrap().contains("another engine"),
        "the detail names the mismatch"
    );
    assert!(d.prompts_received().is_empty());
    // A member whose engine has no table at all is the check's own refusal.
    let mut d = Direct::start(Opts::default()).await;
    with_members(
        &mut d,
        json!({"engineConfig": {"engine": "fake-acp", "schema": 1, "settings": {"reasoning": {"effort": "low"}}}}),
    );
    run(&d).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    assert!(
        s.detail.unwrap().contains("no settings for that engine"),
        "the check's words"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_event_lists_the_placed_keys_and_the_summary_carries_the_hash_and_count_only() {
    let d = run_with_member(Opts::default(), all_five()).await;
    let cfg: EngineConfig =
        serde_json::from_value(member(all_five())["engineConfig"].clone()).unwrap();
    let hash = cfg.canonical_sha256();
    // Exactly one event, of the kind the card reads, with the keys in the table's order and every value as a short text.
    let events = engine_config_events(&d);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0],
        json!({
            "turn": DIRECT_TURN,
            "kind": "engine-config",
            "engine": "cline",
            "sha256": hash,
            "keys": 5,
            "settings": [
                {"key": "reasoning.effort", "value": "high"},
                {"key": "commands.timeout_seconds", "value": "900"},
                {"key": "context.compaction", "value": "basic"},
                {"key": "context.compaction_trigger", "value": "0.8"},
                {"key": "tools.disable", "value": "fetch_web_content"}
            ],
            "readBack": "ok"
        })
    );
    // Inside the inline limit of an event; at most 24 settings; no value over 64 characters.
    assert!(serde_json::to_vec(&events[0]).unwrap().len() < 4096);
    let listed = events[0]["settings"].as_array().unwrap();
    assert!(listed.len() <= 24);
    assert!(
        listed
            .iter()
            .all(|s| s["value"].as_str().unwrap().chars().count() <= 64)
    );
    // The summary: engine, schema, hash and count. No value.
    let s = d.summary();
    assert_eq!(
        s.engine_config,
        Some(DirectEngineConfigRef {
            engine: "cline".to_string(),
            schema: 1,
            sha256: hash,
            keys: 5
        })
    );
    let echoed = serde_json::to_value(&s).unwrap()["engineConfig"].clone();
    let mut names: Vec<&str> = echoed
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["engine", "keys", "schema", "sha256"]);
    assert!(!echoed.to_string().contains("fetch_web_content"));
    // The log says that the settings were placed and read back, without a value; the user's prompt reached the engine as typed.
    let placed: Vec<String> = d
        .log
        .lines()
        .into_iter()
        .filter(|l| l.contains("engine settings placed"))
        .collect();
    assert_eq!(placed.len(), 1, "{placed:?}");
    assert!(
        !placed[0].contains("fetch_web_content") && !placed[0].contains("900"),
        "{}",
        placed[0]
    );
    assert_eq!(d.prompts_received(), [PROMPT.to_string()]);
    println!(
        "ENGINE-CONFIG TRIGGER PRESENT: one engine-config event (5 keys, readBack ok) and summary engineConfig {{engine, schema, sha256, keys}}"
    );
}
