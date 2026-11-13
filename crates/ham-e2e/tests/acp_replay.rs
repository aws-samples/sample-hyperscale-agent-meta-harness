//! Fake ACP engine replaying recorded and synthetic transcripts.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_e2e::acp::{Agentd, ReplayEngine, ReplayError, Script};
use serde_json::{Value, json};
use std::path::PathBuf;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/acp")
}

fn script(name: &str) -> Script {
    Script::load(&fixtures().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn recorded_handshakes_have_headers() {
    let c = script("cline-3.0.65-handshake-k0.jsonl");
    assert_eq!(
        (
            c.header.engine.as_str(),
            c.header.version.as_str(),
            c.header.recorded
        ),
        ("cline", "3.0.65", true)
    );
    let o = script("opencode-1.18.32-handshake-k0.jsonl");
    assert_eq!(
        (
            o.header.engine.as_str(),
            o.header.version.as_str(),
            o.header.recorded
        ),
        ("opencode", "1.18.32", true)
    );
    let t = script("cline-3.0.65-turn1-impl-claim.jsonl");
    assert!(!t.header.recorded && t.header.synthetic);
}

#[test]
fn replay_answers_initialize_with_the_request_id() {
    let mut engine = ReplayEngine::new(vec![script("cline-3.0.65-handshake-k0.jsonl")]);
    let out = engine.handle(&json!({"jsonrpc": "2.0", "id": 41, "method": "initialize", "params": {"protocolVersion": 1}})).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["id"], 41);
    assert_eq!(out[0]["result"]["agentInfo"]["version"], "3.0.65");
}

#[test]
fn replay_rejects_an_out_of_order_request() {
    let mut engine = ReplayEngine::new(vec![script("cline-3.0.65-handshake-k0.jsonl")]);
    let err = engine
        .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt", "params": {}}))
        .expect_err("order");
    assert!(matches!(err, ReplayError::Unexpected { .. }), "{err}");
}

#[test]
fn agentd_runs_handshake_and_turns_for_cline() {
    let mut engine = ReplayEngine::new(vec![
        script("cline-3.0.65-handshake-k0.jsonl"),
        script("cline-3.0.65-turn1-impl-claim.jsonl"),
        script("cline-3.0.65-turn2-tests.jsonl"),
    ]);
    let mut agentd = Agentd::default();
    let hs = agentd.handshake(&mut engine, "/work/w1").unwrap();
    assert_eq!(
        (hs.agent_name.as_str(), hs.agent_version.as_str()),
        ("cline", "3.0.65")
    );
    assert!(!hs.session_id.is_empty());
    let t1 = agentd
        .prompt(
            &mut engine,
            "Add a percent_change(a, b) function with tests",
        )
        .unwrap();
    assert_eq!(t1.stop_reason, "end_turn");
    assert_eq!(t1.edits.len(), 1);
    assert_eq!(t1.edits[0].path, "src/calc.py");
    assert!(t1.edits[0].new_text.contains("def percent_change(a, b):"));
    assert!(
        t1.messages.iter().any(|m| m.contains("Task complete")),
        "the completion claim is in the transcript"
    );
    assert!(
        t1.updates
            .iter()
            .all(|u| u["params"]["sessionId"] == hs.session_id.as_str()),
        "session id is filled in"
    );
    let t2 = agentd.prompt(&mut engine, "G2b red: add tests").unwrap();
    assert_eq!(t2.edits[0].path, "tests/test_calc.py");
    assert!(t2.edits[0].new_text.contains("percent_change(0, 5)"));
    assert!(matches!(
        agentd.prompt(&mut engine, "one more"),
        Err(ReplayError::Exhausted)
    ));
}

/// The OpenCode releases with a recorded session: the handshake of each, and per release one turn through real Bedrock and one through the
/// fake Bedrock.
const OPENCODE_RELEASES: [&str; 4] = ["1.18.32", "1.19.5", "2.2.5", "2.2.6"];

fn opencode_files(version: &str) -> [String; 3] {
    [
        format!("opencode-{version}-handshake-k0.jsonl"),
        format!("opencode-{version}-turn1-bedrock-sonnet-5-5.jsonl"),
        format!("opencode-{version}-turn1-fake-bedrock.jsonl"),
    ]
}

#[test]
fn b792_recorded_opencode_turns_replay_through_the_agentd_driver() {
    for version in OPENCODE_RELEASES {
        let [handshake, bedrock, fake] = opencode_files(version);
        for turn in [bedrock, fake] {
            let mut engine = ReplayEngine::new(vec![script(&handshake), script(&turn)]);
            let mut agentd = Agentd::default();
            let hs = agentd.handshake(&mut engine, "/work/w2").unwrap();
            assert_eq!(
                (hs.agent_name.as_str(), hs.agent_version.as_str()),
                ("OpenCode", version),
                "{turn}"
            );
            let t = agentd.prompt(&mut engine, "a prompt").unwrap();
            assert_eq!(t.stop_reason, "end_turn", "{turn}");
            assert!(
                !t.tool_titles.is_empty(),
                "{turn}: the recorded turn used tools"
            );
            assert!(
                t.messages.iter().any(|m| !m.trim().is_empty()),
                "{turn}: the engine said something"
            );
            assert!(
                t.updates
                    .iter()
                    .all(|u| u["params"]["sessionId"] == hs.session_id.as_str()),
                "{turn}: the session id is filled in"
            );
            let completed = t
                .updates
                .iter()
                .filter(|u| u["params"]["update"]["status"] == "completed")
                .count();
            assert!(completed >= 1, "{turn}: a tool call completed");
        }
    }
}

#[test]
fn b792_recorded_opencode_fixtures_are_marked_recorded_and_name_their_release() {
    for version in OPENCODE_RELEASES {
        for file in opencode_files(version) {
            let s = script(&file);
            assert_eq!(
                (
                    s.header.engine.as_str(),
                    s.header.version.as_str(),
                    s.header.recorded,
                    s.header.synthetic
                ),
                ("opencode", version, true, false),
                "{file}"
            );
        }
    }
}

#[test]
fn b792_the_goal_command_is_advertised_by_the_2_2_releases_only() {
    for version in OPENCODE_RELEASES {
        let hs = script(&opencode_files(version)[0]);
        let advertised: Vec<String> = hs
            .entries
            .iter()
            .filter(|e| e.msg["params"]["update"]["sessionUpdate"] == "available_commands_update")
            .flat_map(|e| {
                e.msg["params"]["update"]["availableCommands"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|c| c["name"].as_str().map(str::to_string))
            .collect();
        assert!(
            !advertised.is_empty(),
            "{version}: the handshake records the command list"
        );
        assert_eq!(
            advertised.iter().any(|n| n == "goal"),
            version.starts_with("2."),
            "{version}: {advertised:?}"
        );
    }
}

#[test]
fn b792_the_pinned_opencode_release_has_a_recorded_handshake_and_both_turns() {
    let profile = harness_core::engine_profile::load_profile(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../engine/profiles/opencode.toml"),
    )
    .unwrap();
    assert!(
        OPENCODE_RELEASES.contains(&profile.version.as_str()),
        "record a session of the pinned release {} before the pin moves",
        profile.version
    );
    for file in opencode_files(&profile.version) {
        assert!(fixtures().join(&file).is_file(), "{file}");
    }
}

#[test]
fn fixtures_hold_no_host_paths_or_credentials() {
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lower = text.to_lowercase();
        for bad in [
            "/users/",
            "testuser",
            "aws_secret",
            "secretaccesskey",
            "sessiontoken",
            "asia",
            "akia",
            "bearer ",
            "api_key\":\"",
        ] {
            assert!(!lower.contains(bad), "{} contains {bad:?}", path.display());
        }
    }
}

fn fake_scripts() -> Vec<Script> {
    Script::load_dir(&fixtures().join("../engines/fake-acp/transcripts")).unwrap()
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn reject_answer(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "selected", "optionId": "reject_once"}}})
}

#[test]
fn the_fake_acp_directory_records_an_agent_permission_request_and_the_clients_answer() {
    let scripts = fake_scripts();
    assert!(
        scripts.len() >= 3,
        "handshake, turn 1 and the permission turn"
    );
    let entries: Vec<_> = scripts.iter().flat_map(|s| s.entries.iter()).collect();
    let at = entries
        .iter()
        .position(|e| {
            e.msg.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .expect("an agent request session/request_permission");
    assert_eq!(entries[at].dir, ham_e2e::acp::Dir::AgentToClient);
    assert!(entries[at].msg.get("id").is_some());
    let answer = entries[at + 1];
    assert_eq!(answer.dir, ham_e2e::acp::Dir::ClientToAgent);
    assert!(answer.msg.get("method").is_none());
    assert_eq!(answer.msg["id"], entries[at].msg["id"]);
}

#[test]
fn replay_pauses_at_an_agent_request_and_resumes_on_the_matching_response() {
    let mut engine = ReplayEngine::new(fake_scripts()).with_workspace("/work/w");
    engine
        .handle(&request(1, "initialize", json!({"protocolVersion": 1})))
        .unwrap();
    let new = engine
        .handle(&request(
            2,
            "session/new",
            json!({"cwd": "/work/w", "mcpServers": []}),
        ))
        .unwrap();
    let sid = new[0]["result"]["sessionId"].as_str().unwrap().to_string();
    let prompt = |n: u64, text: &str| {
        request(
            n,
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
        )
    };
    let one = engine
        .handle(&prompt(3, "Add a percent_change(a, b) function with tests"))
        .unwrap();
    assert_eq!(one.last().unwrap()["id"], 3);
    // Turn two stops at the agent's request: the prompt is not answered yet.
    let two = engine
        .handle(&prompt(4, "Install the requests package"))
        .unwrap();
    let last = two.last().unwrap();
    assert_eq!(last["method"], "session/request_permission", "{two:?}");
    let rid = last["id"].clone();
    assert_ne!(rid, json!(4));
    assert!(
        two.iter().all(|m| m.get("id") != Some(&json!(4))),
        "{two:?}"
    );
    // Another client request while the engine waits is out of order.
    assert!(matches!(
        engine.handle(&prompt(5, "and another")),
        Err(ReplayError::Unexpected { .. })
    ));
    // A response to another id changes nothing.
    assert!(
        engine
            .handle(&reject_answer(&json!(999)))
            .unwrap()
            .is_empty()
    );
    // The matching response resumes the recorded turn and ends with the prompt's own response.
    let rest = engine.handle(&reject_answer(&rid)).unwrap();
    let end = rest.last().unwrap();
    assert_eq!(
        (end["id"].clone(), end["result"]["stopReason"].clone()),
        (json!(4), json!("end_turn")),
        "{rest:?}"
    );
    assert!(
        rest.iter()
            .any(|m| m["params"]["update"]["toolCallId"] == "fake-call-4"),
        "the further tool call follows the answer: {rest:?}"
    );
    // The transcript is spent.
    assert!(matches!(
        engine.handle(&prompt(6, "once more")),
        Err(ReplayError::Exhausted)
    ));
}

const PERMISSION_PROMPT: &str = "Install the requests package with pip, then read src/calc.py";
const TURN_ONE_PROMPT: &str = "Add a percent_change(a, b) function with tests";

/// A fresh replay engine of the fake-acp directory after initialize and session/new, and its session id.
fn after_handshake() -> (ReplayEngine, String) {
    let mut engine = ReplayEngine::new(fake_scripts()).with_workspace("/work/w");
    engine
        .handle(&request(1, "initialize", json!({"protocolVersion": 1})))
        .unwrap();
    let new = engine
        .handle(&request(
            2,
            "session/new",
            json!({"cwd": "/work/w", "mcpServers": []}),
        ))
        .unwrap();
    let sid = new[0]["result"]["sessionId"].as_str().unwrap().to_string();
    (engine, sid)
}

fn prompt(sid: &str, id: u64, text: &str) -> Value {
    request(
        id,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
    )
}

#[test]
fn a_first_prompt_is_answered_from_the_turn_recorded_for_its_text() {
    // agentd starts a fresh engine per invocation, so the permission turn must be reachable as the
    // first prompt of a process: the turn whose recorded prompt text equals the request's answers it.
    let (mut engine, sid) = after_handshake();
    let out = engine.handle(&prompt(&sid, 3, PERMISSION_PROMPT)).unwrap();
    let last = out.last().unwrap();
    assert_eq!(last["method"], "session/request_permission", "{out:?}");
    assert!(
        out.iter()
            .any(|m| m["params"]["update"]["toolCallId"] == "fake-call-3"),
        "{out:?}"
    );
    let rest = engine.handle(&reject_answer(&last["id"])).unwrap();
    assert_eq!(rest.last().unwrap()["id"], 3, "{rest:?}");
    // The unused percent_change turn is still there, and only once.
    let one = engine.handle(&prompt(&sid, 4, TURN_ONE_PROMPT)).unwrap();
    assert_eq!(
        one.last().unwrap()["result"]["stopReason"],
        "end_turn",
        "{one:?}"
    );
    assert!(
        one.iter()
            .any(|m| m["params"]["update"]["toolCallId"] == "fake-call-1"),
        "{one:?}"
    );
    assert!(matches!(
        engine.handle(&prompt(&sid, 5, TURN_ONE_PROMPT)),
        Err(ReplayError::Exhausted)
    ));
}

#[test]
fn a_prompt_no_turn_records_gets_the_next_unused_turn_in_name_order() {
    let (mut engine, sid) = after_handshake();
    let first = engine
        .handle(&prompt(&sid, 3, "something no turn records"))
        .unwrap();
    assert!(
        first
            .iter()
            .any(|m| m["params"]["update"]["toolCallId"] == "fake-call-1"),
        "turn 1 first: {first:?}"
    );
    let second = engine
        .handle(&prompt(&sid, 4, "and another unrecorded text"))
        .unwrap();
    assert_eq!(
        second.last().unwrap()["method"],
        "session/request_permission",
        "{second:?}"
    );
}

#[test]
fn routing_leaves_the_sequential_order_of_an_in_order_session_alone() {
    let (mut engine, sid) = after_handshake();
    let one = engine.handle(&prompt(&sid, 3, TURN_ONE_PROMPT)).unwrap();
    assert!(
        one.iter()
            .any(|m| m["params"]["update"]["toolCallId"] == "fake-call-1"),
        "{one:?}"
    );
    let two = engine.handle(&prompt(&sid, 4, PERMISSION_PROMPT)).unwrap();
    assert_eq!(
        two.last().unwrap()["method"],
        "session/request_permission",
        "{two:?}"
    );
}
