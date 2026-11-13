//! The skills card names the engine the run is on: an OpenCode run read "Cline loads a skill when a task matches its description." and
//! "Cline's own skills list was not available,...". The card reads the engine from the run's own `session.started` event, as the goal card does, and
//! a run on the default engine reads as it always did.

#![allow(clippy::unwrap_used)]

use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, ty: &str, authority: &str, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-11-11T08:15:00Z",
        "type": ty, "authority": authority, "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn started(engine: &str) -> Envelope {
    event(
        1,
        "session.started",
        "harness",
        json!({"agentId": "direct", "engine": engine, "modelId": "m"}),
    )
}

fn skills(preflight: Value) -> Envelope {
    event(
        2,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "skills", "skills": [{"name": "lifecycle-tdd", "digest": DIGEST, "files": 1, "bytes": 2048}],
               "preflight": preflight}),
    )
}

fn view_of(events: Vec<Envelope>) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s.view()
}

fn card_text(engine: &str, preflight: Value) -> String {
    let v = view_of(vec![started(engine), skills(preflight)]);
    v.skills
        .expect("the run has a skills card")
        .lines(200)
        .join("\n")
}

#[test]
fn b817_an_opencode_run_names_opencode_in_the_skills_card() {
    let text = card_text("opencode", json!({"ran": false}));
    assert!(
        text.contains("OpenCode loads a skill when a task matches its description."),
        "{text}"
    );
    assert!(
        text.contains(
            "OpenCode's own skills list was not available, so these skills were placed but not listed."
        ),
        "{text}"
    );
    assert!(!text.contains("Cline"), "{text}");
}

#[test]
fn b817_the_listing_sentence_of_an_opencode_run_names_opencode() {
    let text = card_text("opencode", json!({"ran": true, "ms": 1200}));
    assert!(
        text.contains("OpenCode listed all 1 before it started (1,200 ms)."),
        "{text}"
    );
    assert!(!text.contains("Cline"), "{text}");
}

#[test]
fn b817_a_cline_run_reads_as_before() {
    let text = card_text("cline", json!({"ran": false}));
    assert!(
        text.contains("Cline loads a skill when a task matches its description."),
        "{text}"
    );
    assert!(
        text.contains(
            "Cline's own skills list was not available, so these skills were placed but not listed."
        ),
        "{text}"
    );
}

#[test]
fn b817_a_run_with_no_session_event_names_the_default_engine() {
    let v = view_of(vec![skills(json!({"ran": false}))]);
    let text = v.skills.unwrap().lines(200).join("\n");
    assert!(text.contains("Cline loads a skill"), "{text}");
}
