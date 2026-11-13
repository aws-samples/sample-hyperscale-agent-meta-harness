#![allow(clippy::unwrap_used)]

mod b790_support;

use agentd::acp::Activity;
use agentd::direct::classify::{classify, stop_reason};
use agentd::direct::collector::DirectCollector;
use agentd::direct::driver::{NoProbe, PromptPlan, run_direct_prompt};
use agentd::direct::retry::RetryPolicy;
use agentd::direct::transcript::capability::{build, event_value};
use agentd::events::EventRecorder;
use b790_support::{MODEL, SESSION, TOP, fixture_lines, methods, read_json, replay, repo_root};
use harness_protocol::direct::DirectOutcome;
use serde_json::Value;
use std::time::Duration;

const HANDSHAKE: &str = "opencode-1.18.32-handshake-k0.jsonl";

fn plan() -> PromptPlan {
    PromptPlan {
        cwd: TOP.into(),
        model: MODEL.into(),
        prompt: "Add percent_change with tests".into(),
        nudge: None,
        cancel: None,
        timeout: Duration::from_secs(1000),
        silence: Duration::from_secs(900),
        retry: RetryPolicy::off(),
        failover: None,
        engine_stop_resumes: 0,
        engine: "opencode".into(),
    }
}

async fn play(
    handshake: &str,
    turn: &str,
) -> (
    agentd::direct::driver::DriverEnd,
    DirectCollector,
    b790_support::Received,
) {
    let (client, received, _) = b790_support::engine(replay(handshake, turn));
    let activity = Activity::new();
    let mut client = client.with_activity(activity.clone());
    let mut collector = DirectCollector::new(EventRecorder::new("run1", "direct"));
    let end = run_direct_prompt(&mut client, &mut collector, &activity, &plan(), &NoProbe).await;
    (end, collector, received)
}

/// The recorded sessions of the engine: a handshake and the turns recorded with it (real sessions: through the fake Bedrock and through
/// Bedrock itself, `fixtures/acp/AGENTS.md`).
const RECORDED: [(&str, [&str; 2]); 4] = [
    (
        "opencode-2.2.6-handshake-k0.jsonl",
        [
            "opencode-2.2.6-turn1-fake-bedrock.jsonl",
            "opencode-2.2.6-turn1-bedrock-sonnet-5-5.jsonl",
        ],
    ),
    (
        "opencode-1.18.32-handshake-k0.jsonl",
        [
            "opencode-1.18.32-turn1-fake-bedrock.jsonl",
            "opencode-1.18.32-turn1-bedrock-sonnet-5-5.jsonl",
        ],
    ),
    (
        "opencode-2.2.5-handshake-k0.jsonl",
        [
            "opencode-2.2.5-turn1-fake-bedrock.jsonl",
            "opencode-2.2.5-turn1-bedrock-sonnet-5-5.jsonl",
        ],
    ),
    (
        "opencode-1.19.5-handshake-k0.jsonl",
        [
            "opencode-1.19.5-turn1-fake-bedrock.jsonl",
            "opencode-1.19.5-turn1-bedrock-sonnet-5-5.jsonl",
        ],
    ),
];

#[tokio::test(start_paused = true)]
async fn b790_the_recorded_handshake_and_turn_pass_through_the_driver_to_done() {
    let (end, collector, received) = play(
        "opencode-2.2.6-handshake-k0.jsonl",
        "opencode-2.2.6-turn1-fake-bedrock.jsonl",
    )
    .await;
    assert_eq!(
        methods(&received),
        ["initialize", "session/new", "session/prompt"]
    );
    // The model the engine reports is the one the run asked for, in the engine's own spelling.
    assert_eq!(end.pinned.as_deref(), Some(MODEL));
    assert_eq!(end.session_id.as_deref(), Some(SESSION));
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
    assert_eq!(stop_reason(&end.raw).as_deref(), Some("end_turn"));
    assert!(!end.raw.message.trim().is_empty(), "{:?}", end.raw.message);
    assert!(collector.tool_calls() >= 1, "the recorded turn used a tool");
}

#[tokio::test(start_paused = true)]
async fn b790_every_recorded_turn_of_the_engine_ends_done_and_counts_its_tool_calls() {
    for (handshake, turns) in RECORDED {
        for turn in turns {
            let (end, collector, _) = play(handshake, turn).await;
            assert_eq!(
                classify(&end.raw).outcome,
                DirectOutcome::ReportedDone,
                "{turn}"
            );
            assert!(collector.tool_calls() >= 1, "{turn}");
            assert!(!end.raw.message.trim().is_empty(), "{turn}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn b790_a_model_the_engine_did_not_take_ends_the_run_before_the_prompt() {
    // The recorded engine starts on its own default model; when the pin is not what the run asked for, no prompt is sent.
    let mut script = replay(HANDSHAKE, "opencode-1.18.32-turn1-fake-bedrock.jsonl");
    script.model = "opencode/big-pickle".into();
    let (client, received, _) = b790_support::engine(script);
    let activity = Activity::new();
    let mut client = client.with_activity(activity.clone());
    let mut collector = DirectCollector::new(EventRecorder::new("run1", "direct"));
    let end = run_direct_prompt(&mut client, &mut collector, &activity, &plan(), &NoProbe).await;
    assert!(end.raw.setup_error.is_some(), "{:?}", end.raw.watched);
    assert_eq!(
        classify(&end.raw).outcome,
        DirectOutcome::StoppedSetupFailed
    );
    assert!(!methods(&received).contains(&"session/prompt".to_string()));
}

#[test]
fn b790_the_replayed_handshake_is_the_one_the_capability_golden_pins() {
    let lines = fixture_lines(HANDSHAKE);
    let init = lines.iter().find(|m| m["id"] == 1).unwrap()["result"].clone();
    let new = lines.iter().find(|m| m["id"] == 2).unwrap()["result"].clone();
    let commands: Vec<Value> = lines
        .iter()
        .filter(|m| m["params"]["update"]["sessionUpdate"] == "available_commands_update")
        .flat_map(|m| {
            m["params"]["update"]["availableCommands"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let built = event_value(build("the engine", 1, None, Some(&init), &new, &commands));
    let golden =
        read_json(&repo_root().join("crates/agentd/tests/golden_capability/opencode-1.18.32.json"));
    assert_eq!(built, golden);
}
