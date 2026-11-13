//! The words control-api answers with name the engine of the run, not Cline. A run of Cline reads what it always read; a run of another engine reads its own
//! name in the 409s of messages and answers, in the cancel text and in the ledger's refusals; and a model chosen in an answer is checked against the list of
//! the run's engine.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, World, agent_item, post, run_item};
use harness_protocol::common::Engine;
use message_ledger::error::Refusal;
use serde_json::{Value, json};

const RUN: &str = "01M5A2R7K3AAAAAAAAAAAAAAAA";
const REQUEST_ID: &str = "01M5A2R7K3CCCCCCCCCCCCCCCC";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

fn world(engine: Option<&str>, with_agent: bool) -> World {
    let w = World::new();
    let mut run = run_item(
        RUN,
        ME,
        "active",
        "2026-11-08T10:00:00Z",
        json!({"mode": "s3-only"}),
    );
    run.engine = engine.map(|e| Engine::new(e).unwrap());
    w.add_run(run);
    if with_agent {
        w.agents
            .lock()
            .unwrap()
            .push(agent_item(RUN, SONNET, "running"));
    }
    w
}

async fn answer(w: &World, body: Value) -> (u16, Value) {
    let (s, v, _) = post(
        w,
        ME,
        &format!("/v1/runs/{RUN}/input"),
        &[],
        &body.to_string(),
    )
    .await;
    (s, v)
}

#[tokio::test]
async fn b788_an_answer_to_a_run_with_no_live_session_names_the_engine_of_the_run() {
    let body = json!({"requestId": REQUEST_ID, "action": "finish"});
    for (engine, name) in [(None, "Cline"), (Some("opencode"), "OpenCode")] {
        let (status, v) = answer(&world(engine, false), body.clone()).await;
        assert_eq!(status, 409, "{v}");
        assert_eq!(
            v["message"],
            format!("the run has no live {name} session to answer")
        );
    }
}

#[tokio::test]
async fn b788_a_blank_answer_says_who_it_was_written_for() {
    let body = json!({"requestId": REQUEST_ID, "action": "continue_with_prompt", "text": "  "});
    for (engine, name) in [(None, "Cline"), (Some("opencode"), "OpenCode")] {
        let (status, v) = answer(&world(engine, true), body.clone()).await;
        assert_eq!(status, 400, "{v}");
        assert_eq!(
            v["message"],
            format!("the message is blank; write something for {name} or leave the message out")
        );
    }
    // A run the caller cannot read gets the validation first, as before, and the Cline words.
    let (status, v) = answer(&World::new(), body).await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("write something for Cline"),
        "{v}"
    );
}

#[tokio::test]
async fn b788_a_model_in_an_answer_is_checked_against_the_list_of_the_runs_engine() {
    // `fake-acp` is an engine with a profile and no model in the catalog: a Cline model is not a choice for its run.
    let w = world(Some("fake-acp"), true);
    let body = json!({"requestId": REQUEST_ID, "action": "try_model", "model": SONNET});
    let (status, v) = answer(&w, body.clone()).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["code"], "model-not-smoked");
    let text = v["message"].as_str().unwrap();
    assert!(text.contains("fake-acp") && text.contains(SONNET), "{text}");
    // For a Cline run the same body gets past the model check (the next check is the question itself).
    let (status, v) = answer(&world(None, true), body).await;
    assert_ne!(status, 422, "{v}");
}

#[test]
fn b788_the_ledger_words_name_the_engine_and_the_cline_words_are_unchanged() {
    for refusal in [Refusal::TooManyPending, Refusal::AlreadyDispatching] {
        assert_eq!(refusal.message(), refusal.message_for("Cline"));
        let open = refusal.message_for("OpenCode");
        assert!(
            open.contains("OpenCode") && !open.contains("Cline"),
            "{open}"
        );
    }
    assert_eq!(
        Refusal::AlreadyDispatching.message(),
        "Cline is already taking this message; it cannot be withdrawn"
    );
}
