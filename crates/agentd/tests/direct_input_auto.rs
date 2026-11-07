//! What the harness does by itself before it asks anybody:
//! an automatic fresh session for a conversation Bedrock rejects and for a dead engine, never for Cline's own
//! loop-guard stop; the non-blocking outage notice when every route has been down; the second silence. Whole agent, in process, with the
//! fake engine (see `direct_input.rs` for the questions a person answers). These are the acceptance tests of package A5 and of PM3's
//! three hooks: written first, red until they exist. The windows are seconds, not the product's 30 minutes.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::Config;
use direct_support::{Answer, CfgEdit, Direct, Opts, RUN};
use harness_protocol::common::Protocol;
use harness_protocol::direct::{
    DirectAttemptKind, DirectModelSpec, DirectOutcome, InstructAction, s3 as keys,
};
use serde_json::Value;
use std::time::Duration;

const FRESH: &str = "fresh_session_done";
const OTHER: &str = "global.anthropic.claude-sonnet-5-5";

fn tuned(more: impl FnOnce(&mut Config) + 'static) -> Option<CfgEdit> {
    Some(Box::new(move |c: &mut Config| {
        c.direct.provider_retries = 3;
        c.direct.engine_stop_resumes = 1;
        c.direct.provider_retry_pause = Duration::from_millis(10);
        c.direct.input_margin = Duration::ZERO;
        c.direct.input_min_window = Duration::from_secs(1);
        more(c);
    }))
}

fn opts(scenario: &'static str, wait_s: Option<u32>, env: &[(&'static str, &str)]) -> Opts {
    Opts {
        scenario,
        engine_env: env.iter().map(|(k, v)| (*k, (*v).to_string())).collect(),
        input_wait_s: wait_s,
        edit_cfg: tuned(|_| {}),
        ..Opts::default()
    }
}

fn route(id: &str) -> DirectModelSpec {
    DirectModelSpec {
        id: id.into(),
        region: "us-east-1".into(),
        protocol: Protocol::Converse,
        context_tokens: 1_000_000,
        max_output_tokens: 64_000,
        default_reasoning_effort: None,
    }
}

async fn run(d: &Direct) {
    d.invoke().await;
    d.wait_healthy().await;
}

fn single(d: &Direct, kind: &str) -> (String, Value) {
    let mut v = d.aws.progress(kind);
    assert_eq!(
        v.len(),
        1,
        "exactly one {kind} event, got {:?}\nengine trace:\n{}",
        d.aws.progress_kinds(),
        d.engine_trace()
    );
    v.remove(0)
}

fn has_work(d: &Direct) -> bool {
    std::fs::read_to_string(d.worktree().join("src/calc.py"))
        .unwrap_or_default()
        .contains("def percent_change(a, b):")
}

fn sessions_log(d: &Direct) -> Vec<String> {
    d.engine_sessions()
}

// ---- the conversation Bedrock rejects ------------------------------------------------------------------------------------------

/// The A15 shape (Bedrock refuses the saved conversation, not retryable, and the resume fails the same way in no time): the harness starts
/// a fresh session on its own, with no model switch and no route excluded, and nobody is asked. This holds with and without a window.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_conversation_gets_an_automatic_fresh_session_and_nobody_is_asked() {
    for wait in [None, Some(30)] {
        let mut o = opts(
            "replay_rejected_then_fresh_done",
            wait,
            &[
                ("FAKE_ACP_NEW_SESSION_SCENARIO", FRESH),
                ("FAKE_ACP_RESTART_SCENARIO", FRESH),
            ],
        );
        // routes exist, so a ladder that rotated would switch: it must not
        o.failover = vec![route(OTHER)];
        let d = Direct::start(o).await;
        run(&d).await;
        let s = d.summary();
        assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{wait:?}: {s:?}");
        assert!(has_work(&d), "{wait:?}");
        assert!(
            d.aws.progress("needs-input").is_empty(),
            "{wait:?}: nobody is asked"
        );
        let (_, ns) = single(&d, "new-session");
        assert_eq!(ns["trigger"], "harness", "{ns}");
        assert_eq!(ns["reason"], "replay_rejected", "{ns}");
        assert_eq!(ns["session"], 2);
        assert!(ns["requestId"].is_null(), "{ns}");
        // never the model switch, never a route excluded because of it
        assert!(d.model_switches().is_empty(), "no model switch");
        assert!(s.switches.is_empty(), "{s:?}");
        assert_eq!(s.fresh_sessions.len(), 1);
        assert_eq!(s.fresh_sessions[0].trigger, "harness");
        assert_eq!(s.fresh_sessions[0].reason, "replay_rejected");
        assert_eq!(s.fresh_sessions[0].request_id, None);
        assert_eq!(
            s.attempts.last().map(|a| a.kind),
            Some(DirectAttemptKind::FreshSession)
        );
        assert!(s.interventions.is_empty());
        assert!(
            sessions_log(&d)
                .iter()
                .any(|l| l == "prompt fake-session-0002 history=0"),
            "{:?}",
            sessions_log(&d)
        );
        assert!(
            d.aws.get(&keys::fresh_session_key(RUN, 2)).is_some(),
            "audit copy"
        );
    }
}

/// One automatic fresh session per run: when the fresh session meets the same rejection, the human is asked (`replay_rejected`).
#[tokio::test(flavor = "multi_thread")]
async fn a_second_rejection_after_the_automatic_session_asks_the_user() {
    let o = opts(
        "replay_rejected_then_fresh_done",
        Some(30),
        &[
            // the fresh session replays the failing scenario again: its turns 1 and 2 are the rejection
            (
                "FAKE_ACP_NEW_SESSION_SCENARIO",
                "replay_rejected_then_fresh_done",
            ),
            (
                "FAKE_ACP_RESTART_SCENARIO",
                "replay_rejected_then_fresh_done",
            ),
        ],
    );
    let d = Direct::start(o).await;
    d.aws
        .instruct_on("needs-input", Answer::new(InstructAction::Finish));
    run(&d).await;
    let (_, q) = single(&d, "needs-input");
    assert_eq!(q["reason"], "replay_rejected", "{q}");
    assert_eq!(q["canContinue"], true);
    assert_eq!(
        d.aws.progress("new-session").len(),
        1,
        "the automatic session was the only one"
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedProviderError, "{s:?}");
    assert_eq!(s.fresh_sessions.len(), 1);
}

/// Cline's own loop-guard stop never gets an automatic fresh session: a fresh chat would walk around the brake. With no window the run
/// ends at the second stop as before; with a window the human is asked (`direct_input.rs`).
#[tokio::test(flavor = "multi_thread")]
async fn a_loop_guard_stop_never_gets_an_automatic_fresh_session() {
    let d = Direct::start(opts(
        "stuck_engine_stop_then_done",
        None,
        &[
            ("FAKE_ACP_NEW_SESSION_SCENARIO", FRESH),
            ("FAKE_ACP_RESTART_SCENARIO", FRESH),
        ],
    ))
    .await;
    run(&d).await;
    assert!(d.aws.progress("new-session").is_empty());
    assert!(d.aws.progress("needs-input").is_empty());
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedEngineCancelled, "{s:?}");
    assert!(s.fresh_sessions.is_empty());
    assert!(!has_work(&d));
}

// ---- the dead engine ------------------------------------------------------------------------------------------------------------

/// The engine process exits during the first prompt: the harness restarts it as a fresh session once, on its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_engine_is_restarted_as_a_fresh_session_by_the_harness() {
    let d = Direct::start(opts(
        "engine_dies_then_fresh",
        None,
        &[
            ("FAKE_ACP_DIE_ON_PROMPT", "3"),
            ("FAKE_ACP_RESTART_SCENARIO", FRESH),
        ],
    ))
    .await;
    run(&d).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{s:?}");
    assert!(has_work(&d));
    let (_, ns) = single(&d, "new-session");
    assert_eq!(ns["trigger"], "harness");
    assert_eq!(ns["reason"], "engine_died");
    assert_eq!(ns["restartedProcess"], true);
    assert!(d.aws.progress("needs-input").is_empty());
    assert_eq!(s.fresh_sessions.len(), 1);
    assert!(s.fresh_sessions[0].restarted_process);
}

/// A second death after the automatic restart asks, and the question offers no "continue" because the pipe is closed.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_dead_engine_asks_and_offers_no_continue() {
    // every process dies on its prompt (no cursor kept, so the death repeats): the restart dies too
    let d = Direct::start(opts(
        "engine_dies_then_fresh",
        Some(30),
        &[("FAKE_ACP_DIE_ON_PROMPT", "3")],
    ))
    .await;
    d.aws
        .instruct_on("needs-input", Answer::new(InstructAction::Finish));
    run(&d).await;
    let (_, q) = single(&d, "needs-input");
    assert_eq!(q["reason"], "engine_died", "{q}");
    assert_eq!(q["canContinue"], false);
    assert_eq!(
        q["options"],
        serde_json::json!(["new_session_with_prompt", "finish"])
    );
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedEngineDied);
}

// ---- silence -----------------------------------------------------------------------------------------------------------------------

/// The engine goes silent twice: after the cancel and one resume the second silence asks (`silence`), offering no "continue" because
/// the process may be stuck; a fresh session starts a new process on the same files.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_silence_asks_and_a_fresh_session_restarts_the_process() {
    let mut o = opts(
        "silence_then_done",
        Some(30),
        &[
            ("FAKE_ACP_SILENCE_S", "30"),
            ("FAKE_ACP_SILENCE_PROMPTS", "2"),
            // the engine notices the cancel of the stalled prompt and answers it after a moment, as the real one does
            ("FAKE_ACP_CANCEL_DELAY_MS", "200"),
            ("FAKE_ACP_RESTART_SCENARIO", FRESH),
            ("FAKE_ACP_NEW_SESSION_SCENARIO", FRESH),
        ],
    );
    o.provider_silence_s = Some(1);
    let d = Direct::start(o).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::NewSessionWithPrompt).text("the build step hangs"),
    );
    run(&d).await;
    let (_, q) = single(&d, "needs-input");
    assert_eq!(q["reason"], "silence", "{q}");
    assert_eq!(q["canContinue"], false, "{q}");
    assert_eq!(
        q["options"],
        serde_json::json!(["new_session_with_prompt", "finish"])
    );
    let (_, ns) = single(&d, "new-session");
    assert_eq!(ns["trigger"], "user");
    assert_eq!(
        ns["restartedProcess"], true,
        "a silent engine is not trusted with a second session: {ns}"
    );
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(has_work(&d));
}

// ---- the outage ---------------------------------------------------------------------------------------------------------------------

/// Every route down for the notice time: the run raises one NON-blocking notice (no window, no options, not counted among the
/// questions), keeps probing, and closes the notice as recovered when a route answers again. The run then finishes its work.
#[tokio::test(flavor = "multi_thread")]
async fn an_outage_raises_a_notice_that_does_not_block_and_closes_it_when_a_route_answers() {
    let mut o = opts("writes_files", None, &[("FAKE_ACP_OUTAGE_S", "6")]);
    o.failover = vec![route(OTHER)];
    o.timeout_s = 90;
    o.edit_cfg = tuned(|c| {
        c.direct.provider_retries = 1;
        c.direct.failover_retries = 1;
        c.direct.failover_probe = false;
        c.direct.failover_wait = Duration::from_millis(500);
        c.direct.failover_wait_cap = Duration::from_secs(1);
        c.direct.outage_notice = Duration::from_secs(1);
    });
    let d = Direct::start(o).await;
    run(&d).await;
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "the run waited the outage out: {s:?}"
    );
    assert!(has_work(&d));
    let (id, n) = single(&d, "needs-input");
    assert_eq!(n["reason"], "provider_outage", "{n}");
    assert_eq!(n["blocking"], false, "{n}");
    assert_eq!(n["options"], serde_json::json!([]));
    assert_eq!(n["waitS"], 0);
    assert!(n["expiresAt"].is_null(), "{n}");
    assert_eq!(n["title"], "Every model route is down");
    let (_, closed) = single(&d, "input-closed");
    assert_eq!(closed["requestId"], id.as_str());
    assert_eq!(closed["how"], "recovered", "{closed}");
    assert!(
        s.interventions.is_empty(),
        "a notice is not a question: {s:?}"
    );
    // the notice comes before the recovery
    let kinds = d.aws.progress_kinds();
    let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
    assert!(at("needs-input") < at("input-closed"), "{kinds:?}");
}
