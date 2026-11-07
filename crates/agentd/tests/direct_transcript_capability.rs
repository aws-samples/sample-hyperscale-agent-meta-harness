//! The engine capability event: what an engine says it can do, read from the ACP handshake as it is recorded
//! (`fixtures/acp/*-handshake-k0.jsonl`), in ACP's own words, bounded to the event payload limit, with the harness's own lists. The
//! golden files in `tests/golden_capability/` are the exact records; `GOLDEN_UPDATE=1` rewrites them.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::transcript::capability::{CAPABILITY_PAYLOAD_BYTES, build, event_value};
use harness_protocol::transcript::{HarnessCapabilities, SessionCapabilitiesPayload};
use serde_json::{Value, json};
use std::path::PathBuf;

/// The `initialize` result, the `session/new` result and the commands of the `available_commands_update` of a recorded handshake.
fn handshake(file: &str) -> (Value, Value, Vec<Value>) {
    let path = common::repo_root().join("fixtures/acp").join(file);
    let text = std::fs::read_to_string(&path).unwrap();
    let (mut init, mut new, mut commands) = (None, None, Vec::new());
    for line in text.lines().filter(|l| l.contains("\"dir\"")) {
        let entry: Value = serde_json::from_str(line).unwrap();
        let msg = &entry["msg"];
        if entry["dir"] == "agent->client" {
            match msg["id"].as_u64() {
                Some(1) => init = Some(msg["result"].clone()),
                Some(2) => new = Some(msg["result"].clone()),
                _ => {}
            }
            if msg["method"] == "session/update"
                && msg["params"]["update"]["sessionUpdate"] == "available_commands_update"
            {
                commands = msg["params"]["update"]["availableCommands"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
            }
        }
    }
    (init.unwrap(), new.unwrap(), commands)
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden_capability")
        .join(format!("{name}.json"))
}

/// `v` with every object's members in key order, whatever order `serde_json` keeps (the `preserve_order` feature is switched on when other
/// crates are built in the same run, and the golden files hold sorted keys).
fn canon(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: std::collections::BTreeMap<String, Value> =
                m.iter().map(|(k, v)| (k.clone(), canon(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canon).collect()),
        other => other.clone(),
    }
}

fn golden(name: &str, value: &Value) {
    let path = golden_path(name);
    let mut text = serde_json::to_string_pretty(&canon(value)).unwrap();
    text.push('\n');
    if std::env::var_os("GOLDEN_UPDATE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (GOLDEN_UPDATE=1 writes it)", path.display()));
    assert_eq!(text, want, "{name} differs from its golden file");
}

fn size(v: &Value) -> usize {
    serde_json::to_vec(v).unwrap().len()
}

#[test]
fn the_recorded_handshake_of_the_first_engine_makes_a_record_that_fits_and_equals_its_golden() {
    let (init, new, commands) = handshake("cline-3.0.65-handshake-k0.jsonl");
    assert!(
        size(&new) > 2_000,
        "the recorded session/new reply is about 2.3 KB ({} bytes)",
        size(&new)
    );
    let p = build("Cline", 1, Some("s-1"), Some(&init), &new, &commands);
    assert_eq!(p.v, 1);
    assert_eq!(p.engine, "Cline");
    assert_eq!(p.session, 1);
    assert_eq!(p.session_id.as_deref(), Some("s-1"));
    assert_eq!(p.acp_version, Some(1));
    assert_eq!(p.agent.as_ref().map(|a| a.name.as_str()), Some("cline"));
    assert_eq!(
        p.agent.as_ref().and_then(|a| a.version.as_deref()),
        Some("3.0.65")
    );
    assert_eq!(p.load_session, Some(true));
    assert_eq!(
        p.prompt_capabilities,
        Some(json!({"audio": false, "embeddedContext": false, "image": true}))
    );
    assert_eq!(p.session_capabilities, None, "the engine advertises none");
    let modes = p.modes.as_ref().unwrap();
    assert_eq!(modes.current.as_deref(), Some("act"));
    assert_eq!(
        modes
            .available
            .iter()
            .map(|m| (m.id.as_str(), m.name.as_str()))
            .collect::<Vec<_>>(),
        [("plan", "Plan"), ("act", "Act")]
    );
    let opts: Vec<(&str, Option<&str>, Option<Value>)> = p
        .config_options
        .iter()
        .map(|o| (o.id.as_str(), o.kind.as_deref(), o.current.clone()))
        .collect();
    assert_eq!(
        opts,
        [
            ("provider", Some("select"), Some(json!("bedrock"))),
            ("model", Some("select"), Some(json!("minimax.minimax-m2.5"))),
            ("mode", Some("select"), Some(json!("act"))),
            ("auto_approve", Some("boolean"), Some(json!(false))),
        ]
    );
    assert!(p.commands.is_empty(), "this engine advertises no command");
    assert_eq!(p.harness, HarnessCapabilities::this_build());
    assert!(!p.truncated);
    let v = event_value(p);
    assert!(size(&v) <= CAPABILITY_PAYLOAD_BYTES, "{} bytes", size(&v));
    golden("cline-3.0.65", &v);
}

#[test]
fn the_recorded_handshake_of_another_engine_renders_with_no_code_change() {
    let (init, new, commands) = handshake("opencode-1.18.32-handshake-k0.jsonl");
    assert_eq!(commands.len(), 3);
    let p = build("the engine", 1, None, Some(&init), &new, &commands);
    assert_eq!(p.agent.as_ref().map(|a| a.name.as_str()), Some("OpenCode"));
    assert_eq!(
        p.session_capabilities,
        Some(json!({"close": {}, "fork": {}, "list": {}, "resume": {}}))
    );
    assert!(
        p.modes.is_none(),
        "no modes member in this reply: none made up"
    );
    assert_eq!(
        p.config_options
            .iter()
            .map(|o| o.id.as_str())
            .collect::<Vec<_>>(),
        ["model", "mode"]
    );
    assert_eq!(
        p.commands
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["customize-opencode", "init", "review"]
    );
    assert!(
        p.commands.iter().all(|c| c
            .description
            .as_ref()
            .is_none_or(|d| d.chars().count() <= 160)),
        "a description is cut"
    );
    let v = event_value(p);
    assert!(size(&v) <= CAPABILITY_PAYLOAD_BYTES);
    golden("opencode-1.18.32", &v);
}

#[test]
fn a_record_over_the_limit_drops_commands_first_and_says_so() {
    let (init, new, _) = handshake("cline-3.0.65-handshake-k0.jsonl");
    let many: Vec<Value> = (0..60)
        .map(|i| json!({"name": format!("command-{i}"), "description": format!("What command number {i} does, with enough words to take room {}", "x".repeat(60))}))
        .collect();
    let p = build("Cline", 2, Some("s-2"), Some(&init), &new, &many);
    assert_eq!(p.commands.len(), 60, "the builder keeps what fits the caps");
    let before = size(&serde_json::to_value(&p).unwrap());
    assert!(before > CAPABILITY_PAYLOAD_BYTES, "{before} bytes");
    let v = event_value(p);
    assert!(size(&v) <= CAPABILITY_PAYLOAD_BYTES);
    assert_eq!(v["truncated"], true);
    assert!(v.get("commands").is_none(), "commands are dropped first");
    assert!(
        v["configOptions"].as_array().is_some_and(|a| a.len() == 4),
        "the options stay"
    );
    assert_eq!(
        v["harness"],
        serde_json::to_value(HarnessCapabilities::this_build()).unwrap()
    );
    assert_eq!(v["session"], 2);
}

#[test]
fn what_the_engine_sends_is_bounded_before_it_is_kept() {
    let long = "w".repeat(5_000);
    let init = json!({
        "protocolVersion": 1,
        "agentInfo": {"name": long, "version": long},
        "agentCapabilities": {"loadSession": true, "promptCapabilities": {"image": true}},
    });
    let options: Vec<Value> = (0..200)
        .map(|i| json!({"id": format!("opt-{i}-{long}"), "name": long, "type": "select", "currentValue": long, "options": [{"value": "a", "name": "A"}]}))
        .collect();
    let modes: Vec<Value> = (0..100)
        .map(|i| json!({"id": format!("m{i}"), "name": long}))
        .collect();
    let new = json!({"configOptions": options, "modes": {"availableModes": modes, "currentModeId": "m1"}});
    let commands: Vec<Value> = (0..500)
        .map(|i| json!({"name": format!("{long}{i}"), "description": long}))
        .collect();
    let p = build("Cline", 1, Some(&long), Some(&init), &new, &commands);
    assert!(p.agent.as_ref().unwrap().name.chars().count() <= 64);
    assert!(p.session_id.as_ref().unwrap().chars().count() <= 128);
    assert!(p.config_options.len() <= 32, "{}", p.config_options.len());
    assert!(
        p.config_options
            .iter()
            .all(|o| o.id.chars().count() <= 64 && o.name.chars().count() <= 64)
    );
    assert!(p.config_options.iter().all(|o| {
        o.current
            .as_ref()
            .is_none_or(|c| c.as_str().is_none_or(|s| s.chars().count() <= 128))
    }));
    assert!(p.modes.as_ref().unwrap().available.len() <= 16);
    assert!(p.commands.len() <= 64);
    assert!(p.commands.iter().all(|c| c.name.chars().count() <= 64));
    let v = event_value(p);
    assert!(size(&v) <= CAPABILITY_PAYLOAD_BYTES, "{} bytes", size(&v));
}

#[test]
fn a_reply_that_is_not_a_handshake_makes_the_smallest_record() {
    for new in [
        json!({}),
        json!(null),
        json!("text"),
        json!([1, 2]),
        json!({"configOptions": "x", "modes": 5}),
    ] {
        let p = build("Cline", 3, None, None, &new, &[]);
        let want = SessionCapabilitiesPayload {
            v: 1,
            engine: "Cline".into(),
            session: 3,
            harness: HarnessCapabilities::this_build(),
            ..SessionCapabilitiesPayload::default()
        };
        assert_eq!(p, want, "{new}");
    }
}

#[test]
fn a_value_that_looks_like_a_secret_is_not_kept() {
    // built at run time: no token-shaped literal is committed
    let token = format!("gh{}_{}", 'p', "aB3dE5gH7jK9mN1pQ3sT5vW7yZ2cF4hJ6kL8");
    let new = json!({"configOptions": [{"id": "x", "name": "X", "type": "select", "currentValue": token}]});
    let v = event_value(build("Cline", 1, None, None, &new, &[]));
    assert!(!v.to_string().contains(&token), "{v}");
    assert!(v.to_string().contains("<redacted>"));
}

#[test]
fn the_event_type_and_the_authority_are_the_ones_the_protocol_names() {
    use harness_protocol::envelope::{Authority, EventType};
    assert_eq!(
        EventType::SessionCapabilities.required_authority(),
        Authority::Agent
    );
}
