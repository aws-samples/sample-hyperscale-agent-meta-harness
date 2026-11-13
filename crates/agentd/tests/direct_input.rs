//! The user correction path of the direct driver, whole: a
//! stopped run keeps its Cline session and its work tree, raises ONE question (`needs-input`), waits for the user's answer through the
//! control queue that carries Cancel, and acts on it: a message in the same session, a fresh session on the same files, another model,
//! or finish. `POST /invocations` against the fake AWS (S3 and the internal API, whose events responses carry the answers) and the fake
//! ACP engine, as in `direct_invocation.rs`. No AWS call is made.
//!
//! These are the acceptance tests of the stage loop (package A2b): written before it and red until it exists. The automatic triggers
//! (a fresh session without a question, the outage notice) are in `direct_input_auto.rs`. The windows here are seconds, not the
//! 30 minutes of the product: the config knobs `input_margin` and `input_min_window` are turned down in `tuned`. The window arithmetic
//! at its edges, with a paused clock, is the pure test `direct_input_machine.rs` (package A1).
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::Config;
use agentd::direct::retry::{RESUME_PROMPT, RESUME_PROMPT_INTERRUPTED};
use common::git;
use direct_support::{Answer, CfgEdit, Direct, Opts, PROMPT, RUN};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::common::Protocol;
use harness_protocol::direct::{
    DirectAttemptKind, DirectModelSpec, DirectOutcome, InstructAction, InstructOrigin, s3 as keys,
};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::TurnOutcome;
use serde_json::{Value, json};
use std::time::Duration;

/// The fresh session of the new-session tests: the process or the session answers from `fresh_session_done`, whichever agentd uses.
const FRESH: &str = "fresh_session_done";
/// A second route of the run, for the tests that may switch the model.
const OTHER: &str = "global.anthropic.claude-sonnet-5-5";

/// The config of an input test: the engine-stop resume and the provider retries on (a stop that the harness resumes first needs them),
/// short pauses, and a window that may be as short as a second and need not leave a margin before the run's deadline.
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

fn opts(scenario: &'static str, wait_s: u32, env: &[(&'static str, &str)]) -> Opts {
    Opts {
        scenario,
        engine_env: env.iter().map(|(k, v)| (*k, (*v).to_string())).collect(),
        input_wait_s: Some(wait_s),
        edit_cfg: tuned(|_| {}),
        ..Opts::default()
    }
}

/// The environment of a scenario whose fresh session answers from `fresh_session_done`, in the live process or in a restarted one.
fn fresh_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("FAKE_ACP_NEW_SESSION_SCENARIO", FRESH),
        ("FAKE_ACP_RESTART_SCENARIO", FRESH),
    ]
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

async fn run(d: &Direct) -> harness_protocol::runtime::InvocationResponse {
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    resp
}

/// The progress kinds of the question's life, in order (the other progress events are left out).
fn input_kinds(d: &Direct) -> Vec<String> {
    const OURS: [&str; 5] = [
        "needs-input",
        "input-received",
        "input-ignored",
        "input-closed",
        "new-session",
    ];
    d.aws
        .progress_kinds()
        .into_iter()
        .filter(|k| OURS.contains(&k.as_str()))
        .collect()
}

/// The one event of a kind: `(eventId, payload)`.
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

fn calc_py(d: &Direct) -> String {
    std::fs::read_to_string(d.worktree().join("src/calc.py")).unwrap_or_default()
}

fn has_work(d: &Direct) -> bool {
    calc_py(d).contains("def percent_change(a, b):")
}

fn attempt_kinds(d: &Direct) -> Vec<DirectAttemptKind> {
    d.summary().attempts.iter().map(|a| a.kind).collect()
}

/// The fake engine's per-session log: which session answered which prompt (written when a fresh-session switch is on).
fn sessions_log(d: &Direct) -> Vec<String> {
    d.engine_sessions()
}

// ---- no window, no question ---------------------------------------------------------------------------------------------------

/// A request without `inputWaitS` (scripts, the release gate, old clients) and a server without the setting end every stop as before.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_window_no_stop_asks_and_every_run_ends_as_it_did() {
    for scenario in [
        "gave_up_no_change",
        "end_turn_empty",
        "max_tokens",
        "max_turn_requests",
        "refusal",
        "weird_stop",
        "loop_guard_always",
    ] {
        let d = Direct::start(Opts {
            scenario,
            edit_cfg: tuned(|_| {}),
            ..Opts::default()
        })
        .await;
        run(&d).await;
        assert!(
            input_kinds(&d).is_empty(),
            "{scenario}: no window, no question: {:?}",
            d.aws.progress_kinds()
        );
        let s = d.summary();
        assert!(s.interventions.is_empty(), "{scenario}: {s:?}");
        assert!(s.fresh_sessions.is_empty(), "{scenario}: {s:?}");
        assert!(
            !s.detail.clone().unwrap_or_default().contains("Nobody"),
            "{scenario}: {s:?}"
        );
    }
}

/// The server setting is the default, and the request's field replaces it.
#[tokio::test(flavor = "multi_thread")]
async fn the_server_setting_applies_when_the_request_sends_none_and_the_request_wins_when_it_does()
{
    let mut o = opts("gave_up_no_change", 0, &[]);
    o.input_wait_s = None;
    o.edit_cfg = tuned(|c| c.direct.input_wait = Duration::from_secs(3));
    let d = Direct::start(o).await;
    run(&d).await;
    let (_, q) = single(&d, "needs-input");
    assert_eq!(q["waitS"], 3, "{q}");

    let mut o = opts("gave_up_no_change", 4, &[]);
    o.edit_cfg = tuned(|c| c.direct.input_wait = Duration::from_secs(9));
    let d = Direct::start(o).await;
    run(&d).await;
    let (_, q) = single(&d, "needs-input");
    assert_eq!(
        q["waitS"], 4,
        "the request replaces the server setting: {q}"
    );
}

/// With the product's own limits (a 300 s smallest window, a 900 s margin) a 120 s wait leaves no window, so nothing is asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_window_under_the_smallest_one_is_not_opened() {
    let mut o = opts("gave_up_no_change", 120, &[]);
    o.edit_cfg = Some(Box::new(|c: &mut Config| {
        c.direct.provider_retries = 3;
        c.direct.provider_retry_pause = Duration::from_millis(10);
    }));
    let d = Direct::start(o).await;
    run(&d).await;
    assert!(input_kinds(&d).is_empty(), "{:?}", d.aws.progress_kinds());
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedNoChanges);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cap_of_zero_questions_never_asks() {
    let mut o = opts("stuck_engine_stop_then_done", 20, &[]);
    o.edit_cfg = tuned(|c| c.direct.input_max_requests = 0);
    let d = Direct::start(o).await;
    run(&d).await;
    assert!(input_kinds(&d).is_empty(), "{:?}", d.aws.progress_kinds());
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedEngineCancelled, "{s:?}");
    assert_eq!(d.prompts_received().len(), 2, "the prompt and one resume");
}

/// The endings that are not stops: Cline reported done asks nothing, and neither does the user's cancel.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_reported_done_or_was_cancelled_asks_nothing() {
    let d = Direct::start(opts("writes_files", 20, &[])).await;
    run(&d).await;
    assert!(input_kinds(&d).is_empty(), "{:?}", d.aws.progress_kinds());
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);

    let mut o = opts("end_turn_done", 20, &[("FAKE_ACP_HOLD_PROMPTS", "1")]);
    o.timeout_s = 60;
    let d = Direct::start(o).await;
    d.aws.cancel_on(EventType::TurnStarted);
    let resp = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Cancelled);
    assert!(input_kinds(&d).is_empty(), "{:?}", d.aws.progress_kinds());
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedByUser);
}

// ---- the question and the four answers ----------------------------------------------------------------------------------------

/// A normal end that changed no file: the machine asks (`no_changes`), the user's message goes into the same Cline session word for word,
/// and the work is done there.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_after_a_give_up_goes_into_the_same_session_and_gets_the_work_done() {
    let say = "The tests contradict nothing: add the function and fix the test.";
    let d = Direct::start(opts("gave_up_then_done", 30, &[])).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::ContinueWithPrompt).text(say),
    );
    let resp = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);

    // the question
    let (id, q) = single(&d, "needs-input");
    assert_eq!(q["requestId"], id.as_str());
    assert_eq!(q["n"], 1);
    assert_eq!(q["reason"], "no_changes", "{q}");
    assert_eq!(q["stopOutcome"], "stopped_no_changes");
    assert_eq!(q["blocking"], true);
    assert_eq!(q["canContinue"], true);
    // no other route in this request, so no other model to offer
    assert_eq!(
        q["options"],
        json!(["continue_with_prompt", "new_session_with_prompt", "finish"])
    );
    assert_eq!(q["models"], json!([]));
    assert_eq!(q["waitS"], 30);
    assert!(q["expiresAt"].is_string(), "{q}");
    assert!(q["recommended"].is_null(), "no recommendation in v1: {q}");
    assert_eq!(q["evidence"]["attempts"], 1, "{q}");
    assert_eq!(q["evidence"]["toolCalls"], 1, "{q}");
    assert_eq!(q["evidence"]["filesChanged"], 0, "{q}");
    assert!(
        q["lastMessage"]
            .as_str()
            .unwrap()
            .contains("cannot be done"),
        "{q}"
    );
    // a client that predates this feature shows the one-line text as the agent's last words
    assert!(
        q["text"]
            .as_str()
            .unwrap()
            .starts_with("Needs your input: Cline ended without changing files"),
        "{q}"
    );

    // the acknowledgement
    let (_, rec) = single(&d, "input-received");
    assert_eq!(rec["requestId"], id.as_str());
    assert_eq!(rec["action"], "continue_with_prompt");
    assert_eq!(rec["origin"], "user");
    assert_eq!(rec["textBytes"], say.len());
    assert_eq!(input_kinds(&d), ["needs-input", "input-received"]);

    // Cline got the first prompt unchanged and then the user's words word for word
    assert_eq!(d.prompts_received(), [PROMPT, say]);
    assert_eq!(
        sessions_log(&d).len(),
        0,
        "no fresh-session switch was on: nothing to log"
    );

    // the end: the work is delivered and the record names the intervention without its text
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{s:?}");
    assert!(has_work(&d), "{}", calc_py(&d));
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());
    assert_eq!(
        attempt_kinds(&d),
        [DirectAttemptKind::Prompt, DirectAttemptKind::Instruction]
    );
    assert!(s.fresh_sessions.is_empty());
    assert_eq!(s.interventions.len(), 1, "{s:?}");
    let i = &s.interventions[0];
    assert_eq!(i.n, 1);
    assert_eq!(i.request_id, id);
    assert_eq!(i.reason, "no_changes");
    assert_eq!(i.outcome, "answered");
    assert_eq!(i.action, Some(InstructAction::ContinueWithPrompt));
    assert_eq!(i.origin, Some(InstructOrigin::User));
    assert_eq!(i.text_bytes, Some(say.len() as u64));
    assert_eq!(i.at_attempt, 2);
    assert!(
        !serde_json::to_string(&s).unwrap().contains(say),
        "the user's text is never recorded"
    );
    for (_, v) in d.aws.progress("input-received") {
        assert!(!v.to_string().contains(say), "{v}");
    }
}

/// The loop guard belongs to the Cline session: the harness resumed once and it tripped again, the machine asks
/// (`engine_stop`), and a message in the SAME session trips the guard after one more call, so the second question follows. A fresh
/// session works. The record names both answers.
#[tokio::test(flavor = "multi_thread")]
async fn after_a_loop_guard_stop_a_message_in_the_same_session_trips_it_again_and_a_fresh_session_works()
 {
    let d = Direct::start(opts("stuck_engine_stop_then_done", 30, &fresh_env())).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::ContinueWithPrompt).text("Poll the log file."),
    );
    // the answer to the second question, which names the second needs-input event
    d.aws.instruct_rule(
        "needs-input",
        2,
        "needs-input",
        vec![
            Answer::new(InstructAction::NewSessionWithPrompt)
                .text("Poll the log file in a new chat.")
                .control(),
        ],
        1,
    );
    run(&d).await;
    let asked = d.aws.progress("needs-input");
    assert_eq!(asked.len(), 2, "{:?}", d.aws.progress_kinds());
    let (id1, q1) = &asked[0];
    let (id2, q2) = &asked[1];
    assert_eq!(q1["reason"], "engine_stop", "{q1}");
    assert_eq!(q1["n"], 1);
    assert_eq!(q1["evidence"]["attempts"], 2, "{q1}");
    assert_eq!(q1["evidence"]["toolCalls"], 10, "{q1}");
    assert_eq!(
        q1["evidence"]["identicalCalls"], 5,
        "the loop guard's count: {q1}"
    );
    assert_eq!(q1["stopOutcome"], "stopped_engine_cancelled");
    assert_eq!(q2["reason"], "engine_stop", "{q2}");
    assert_eq!(q2["n"], 2);
    assert_eq!(q2["evidence"]["attempts"], 4, "{q2}");
    assert_ne!(id1, id2);
    assert_eq!(
        input_kinds(&d),
        [
            "needs-input",
            "input-received",
            "needs-input",
            "input-received",
            "new-session"
        ]
    );
    let prompts = d.prompts_received();
    assert_eq!(prompts.len(), 5, "{prompts:?}");
    assert_eq!(prompts[0], PROMPT);
    assert!([RESUME_PROMPT, RESUME_PROMPT_INTERRUPTED].contains(&prompts[1].as_str()));
    assert_eq!(prompts[2], "Poll the log file.");
    assert!([RESUME_PROMPT, RESUME_PROMPT_INTERRUPTED].contains(&prompts[3].as_str()));
    assert!(
        prompts[4].contains("Poll the log file in a new chat."),
        "{}",
        prompts[4]
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{s:?}");
    assert!(has_work(&d));
    assert_eq!(
        attempt_kinds(&d),
        [
            DirectAttemptKind::Prompt,
            DirectAttemptKind::Resume,
            DirectAttemptKind::Instruction,
            DirectAttemptKind::Resume,
            DirectAttemptKind::FreshSession
        ]
    );
    assert_eq!(s.interventions.len(), 2, "{s:?}");
    assert_eq!(
        s.interventions[0].action,
        Some(InstructAction::ContinueWithPrompt)
    );
    assert_eq!(
        s.interventions[1].action,
        Some(InstructAction::NewSessionWithPrompt)
    );
    assert_eq!(s.fresh_sessions.len(), 1);
    assert_eq!(
        s.fresh_sessions[0].request_id.as_deref(),
        Some(id2.as_str())
    );
    assert_eq!(s.fresh_sessions[0].at_attempt, 5);
    // the first session ended at 1900/200 (cumulative, per session), the second at 350/90
    let u = s.usage.expect("usage");
    assert_eq!((u.input_tokens, u.output_tokens), (2250, 290), "{u:?}");
}

/// The fresh session: `session/new` in the same work tree (or a restart of the process), a prompt written by the harness around the
/// user's words and a summary, context files outside the repository, and the work done in the new session. The old history is not
/// replayed, and the usage of both sessions is added up.
#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_session_the_user_asks_for_restarts_from_the_work_tree_and_finishes_the_work() {
    let say = "Poll the log file instead.";
    let d = Direct::start(opts("stuck_engine_stop_then_done", 30, &fresh_env())).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::NewSessionWithPrompt).text(say),
    );
    run(&d).await;
    let (id, q) = single(&d, "needs-input");
    assert_eq!(q["reason"], "engine_stop");

    // the events, in order
    assert_eq!(
        input_kinds(&d),
        ["needs-input", "input-received", "new-session"]
    );
    let (_, rec) = single(&d, "input-received");
    assert_eq!(rec["action"], "new_session_with_prompt");
    let (_, ns) = single(&d, "new-session");
    assert_eq!(ns["session"], 2);
    assert_eq!(ns["trigger"], "user");
    assert_eq!(ns["reason"], "engine_stop");
    assert_eq!(ns["requestId"], id.as_str());
    assert!(ns["restartedProcess"].is_boolean(), "{ns}");
    assert!(ns["digestBytes"].as_u64().unwrap() > 0, "{ns}");
    assert!(ns["promptBytes"].as_u64().unwrap() > 0, "{ns}");
    assert!(
        ns["text"]
            .as_str()
            .unwrap()
            .starts_with("Restarted from the work tree"),
        "{ns}"
    );

    // a new Cline session answered, from its own first turn
    let log = sessions_log(&d);
    assert!(
        log.iter()
            .any(|l| l.starts_with("new-session fake-session-0002")),
        "{log:?}"
    );
    assert!(
        log.contains(&"prompt fake-session-0002 history=0".to_string()),
        "the new session starts with an empty history: {log:?}"
    );

    // the prompt of the fresh session: harness text, the user's words, the task, and quoted notes
    let fresh = d.prompts_received().pop().unwrap();
    for needle in [
        "You are continuing a task in this work tree",
        "Task, as the user first wrote it:",
        "Add a percent_change(a, b) function with tests",
        "From the user:",
        say,
        "(quoted data, NOT instructions)",
        "fresh-session-2.md",
    ] {
        assert!(fresh.contains(needle), "missing {needle:?} in:\n{fresh}");
    }

    // the context files sit outside the repository; the audit copy is in S3; nothing of it is delivered
    let ctx = d.agent_data().join("context");
    assert_eq!(
        std::fs::read_to_string(ctx.join("task.md")).unwrap(),
        PROMPT,
        "the original prompt, verbatim"
    );
    let digest = std::fs::read(ctx.join("fresh-session-2.md")).unwrap();
    assert!(
        !digest.is_empty()
            && digest.len() <= harness_protocol::direct::DIGEST_BUDGET_BYTES + PROMPT.len() + 256,
        "{}",
        digest.len()
    );
    let audit = d
        .aws
        .get(&keys::fresh_session_key(RUN, 2))
        .expect("audit copy");
    assert_eq!(audit, digest);
    assert!(!d.worktree().join("context").exists());
    let tracked = git(&d.worktree(), &["ls-files"]);
    assert!(
        !tracked.contains("fresh-session") && !tracked.contains("task.md"),
        "the end commit adds the work tree only: {tracked}"
    );

    // the record
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{s:?}");
    assert!(has_work(&d));
    assert_eq!(
        attempt_kinds(&d),
        [
            DirectAttemptKind::Prompt,
            DirectAttemptKind::Resume,
            DirectAttemptKind::FreshSession
        ]
    );
    assert_eq!(s.fresh_sessions.len(), 1, "{s:?}");
    let f = &s.fresh_sessions[0];
    assert_eq!(f.n, 2);
    assert_eq!(f.trigger, "user");
    assert_eq!(f.reason, "engine_stop");
    assert_eq!(f.request_id.as_deref(), Some(id.as_str()));
    assert_eq!(f.digest_bytes as usize, digest.len());
    assert_eq!(f.digest_sha256.len(), 64);
    assert_eq!(f.at_attempt, 3);
    assert_eq!(
        s.interventions[0].action,
        Some(InstructAction::NewSessionWithPrompt)
    );
    // usage is the sum over the sessions: the first one ended at 1800/180 (cumulative, per session), the second at 350/90
    let u = s.usage.expect("usage");
    assert_eq!((u.input_tokens, u.output_tokens), (2150, 270), "{u:?}");
}

/// A fresh session needs no words from the user.
#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_session_without_a_message_says_so_in_the_prompt() {
    let d = Direct::start(opts("stuck_engine_stop_then_done", 30, &fresh_env())).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::NewSessionWithPrompt),
    );
    run(&d).await;
    let fresh = d.prompts_received().pop().unwrap();
    assert!(fresh.contains("From the user:\n(no message)"), "{fresh}");
    let (_, rec) = single(&d, "input-received");
    assert_eq!(rec["textBytes"], 0);
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
}

/// Finish keeps what exists, and the label keeps the stop's words with one lead sentence.
#[tokio::test(flavor = "multi_thread")]
async fn finish_ends_the_run_with_what_exists_and_says_so() {
    let d = Direct::start(opts("gave_up_then_done", 30, &[])).await;
    d.aws
        .instruct_on("needs-input", Answer::new(InstructAction::Finish));
    run(&d).await;
    let (id, q) = single(&d, "needs-input");
    assert_eq!(q["reason"], "no_changes");
    assert_eq!(q["stopOutcome"], "stopped_no_changes");
    assert_eq!(
        input_kinds(&d),
        ["needs-input", "input-received", "input-closed"]
    );
    let (_, closed) = single(&d, "input-closed");
    assert_eq!(closed["requestId"], id.as_str());
    assert_eq!(closed["how"], "finish");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedNoChanges, "{s:?}");
    assert_eq!(s.label, DirectOutcome::StoppedNoChanges.label());
    assert!(
        s.detail
            .clone()
            .unwrap()
            .starts_with("You chose to finish."),
        "{s:?}"
    );
    assert_eq!(d.prompts_received().len(), 1, "no further prompt");
    assert!(!has_work(&d));
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_some(),
        "delivered as for any stop"
    );
    assert_eq!(s.interventions.len(), 1);
    assert_eq!(s.interventions[0].outcome, "finish");
    assert_eq!(s.interventions[0].action, Some(InstructAction::Finish));
}

/// Nobody answers: the window closes and the run ends as the stop did, with one sentence that says nobody answered.
#[tokio::test(flavor = "multi_thread")]
async fn when_nobody_answers_the_run_finishes_with_what_exists_and_says_so() {
    let d = Direct::start(opts("gave_up_then_done", 3, &[])).await;
    let t0 = std::time::Instant::now();
    run(&d).await;
    assert!(t0.elapsed() >= Duration::from_secs(3), "{:?}", t0.elapsed());
    assert_eq!(input_kinds(&d), ["needs-input", "input-closed"]);
    let (id, q) = single(&d, "needs-input");
    assert_eq!(q["waitS"], 3);
    let (_, closed) = single(&d, "input-closed");
    assert_eq!(closed["requestId"], id.as_str());
    assert_eq!(closed["how"], "expired");
    assert!(closed["waitedS"].as_u64().unwrap() >= 3, "{closed}");
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedNoChanges,
        "the stop's own label stays: {s:?}"
    );
    assert!(
        s.detail
            .clone()
            .unwrap()
            .starts_with("Nobody answered in 1 min, so the run finished with what exists."),
        "{s:?}"
    );
    assert_eq!(s.interventions[0].outcome, "expired");
    assert_eq!(d.prompts_received().len(), 1);
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());
}

/// While the run waits, the silence watchdog sleeps, the events poll and the heartbeat go on.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_is_not_silence_and_the_flusher_keeps_running() {
    let mut o = opts("gave_up_no_change", 5, &[]);
    o.provider_silence_s = Some(2);
    let d = Direct::start(o).await;
    run(&d).await;
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedNoChanges,
        "a 2 s silence limit must not fire during a 5 s wait: {s:?}"
    );
    assert!(
        s.detail.clone().unwrap().contains("Nobody answered"),
        "{s:?}"
    );
    assert!(
        d.aws.heartbeats().len() >= 10,
        "heartbeats go on while the run waits: {}",
        d.aws.heartbeats().len()
    );
    assert!(
        d.aws.batches().len() >= 30,
        "the events poll goes on: {}",
        d.aws.batches().len()
    );
}

/// Try another model: the question names the run's other routes; the answer switches the model and the run goes on in the same session.
#[tokio::test(flavor = "multi_thread")]
async fn try_model_switches_the_model_and_the_run_goes_on_in_the_same_session() {
    let mut o = opts("try_model_then_done", 30, &[]);
    o.failover = vec![route(OTHER)];
    let d = Direct::start(o).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::TryModel).model(OTHER),
    );
    run(&d).await;
    let (_, q) = single(&d, "needs-input");
    assert_eq!(q["reason"], "refused", "{q}");
    assert_eq!(q["stopOutcome"], "stopped_refusal");
    let offered: Vec<&str> = q["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        offered,
        [
            "continue_with_prompt",
            "new_session_with_prompt",
            "try_model",
            "finish"
        ]
    );
    assert_eq!(q["models"].as_array().unwrap().len(), 1, "{q}");
    assert_eq!(q["models"][0]["id"], OTHER);
    let (_, rec) = single(&d, "input-received");
    assert_eq!(rec["action"], "try_model");
    assert_eq!(rec["model"], OTHER);

    // the engine was switched, the conversation kept, and the fixed sentence went out because the user wrote nothing
    assert_eq!(d.model_switches(), [OTHER]);
    let prompts = d.prompts_received();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(
        [RESUME_PROMPT, RESUME_PROMPT_INTERRUPTED].contains(&prompts[1].as_str()),
        "{prompts:?}"
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{s:?}");
    assert!(has_work(&d));
    assert_eq!(s.final_model.as_deref(), Some(OTHER));
    assert_eq!(s.switches.len(), 1, "{s:?}");
    assert_eq!(s.switches[0].class, "user");
    assert_eq!(s.switches[0].to, OTHER);
    assert_eq!(
        attempt_kinds(&d),
        [DirectAttemptKind::Prompt, DirectAttemptKind::Instruction]
    );
    assert_eq!(s.interventions[0].model.as_deref(), Some(OTHER));
}

/// Every stop that ends a run has a question, with its own reason and the words of the stop it holds; finish ends each with that stop's
/// label.
#[tokio::test(flavor = "multi_thread")]
async fn every_stop_that_ends_a_run_asks_with_its_reason() {
    // (scenario, reason, held outcome, provider retries)
    let table = [
        ("gave_up_no_change", "no_changes", "stopped_no_changes", 3),
        ("end_turn_empty", "no_message", "stopped_no_message", 3),
        ("max_tokens", "output_limit", "stopped_max_tokens", 3),
        (
            "max_turn_requests",
            "request_limit",
            "stopped_max_requests",
            3,
        ),
        ("refusal", "refused", "stopped_refusal", 3),
        ("weird_stop", "other_stop", "stopped_other", 3),
        ("rpc_error", "provider_error", "stopped_provider_error", 0),
    ];
    for (scenario, reason, outcome, retries) in table {
        let mut o = opts(scenario, 20, &[]);
        o.edit_cfg = tuned(move |c| c.direct.provider_retries = retries);
        let d = Direct::start(o).await;
        d.aws
            .instruct_on("needs-input", Answer::new(InstructAction::Finish));
        run(&d).await;
        let (_, q) = single(&d, "needs-input");
        assert_eq!(q["reason"], reason, "{scenario}: {q}");
        assert_eq!(q["stopOutcome"], outcome, "{scenario}: {q}");
        assert_eq!(q["blocking"], true, "{scenario}");
        assert_eq!(q["canContinue"], true, "{scenario}");
        let offered: Vec<&str> = q["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            ["continue_with_prompt", "new_session_with_prompt", "finish"]
                .iter()
                .all(|a| offered.contains(a)),
            "{scenario}: {offered:?}"
        );
        assert!(!q["title"].as_str().unwrap().is_empty(), "{scenario}");
        let s = d.summary();
        assert_eq!(s.outcome.as_str(), outcome, "{scenario}: {s:?}");
        assert!(
            s.detail
                .clone()
                .unwrap_or_default()
                .starts_with("You chose to finish."),
            "{scenario}: {s:?}"
        );
        assert!(d.aws.get(&keys::bundle_key(RUN)).is_some(), "{scenario}");
    }
}

// ---- answers that the machine does not take -----------------------------------------------------------------------------------

/// Finishes the run after an `input-ignored`, so a test can look at the refusal without waiting out the window.
fn finish_after_ignore(d: &Direct) {
    d.aws.instruct_after(
        "input-ignored",
        "needs-input",
        Answer::new(InstructAction::Finish),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_for_another_request_is_ignored_and_the_window_stays_open() {
    let d = Direct::start(opts("gave_up_then_done", 30, &[])).await;
    d.aws.instruct_batch(
        "needs-input",
        vec![PendingControl::Instruct {
            request_id: "01M5UNKNOWN0000000000000000".into(),
            action: InstructAction::Finish,
            text: None,
            model: None,
            origin: InstructOrigin::User,
        }],
    );
    finish_after_ignore(&d);
    run(&d).await;
    let (_, ign) = single(&d, "input-ignored");
    assert_eq!(ign["why"], "unknown-request", "{ign}");
    assert_eq!(
        input_kinds(&d),
        [
            "needs-input",
            "input-ignored",
            "input-received",
            "input-closed"
        ]
    );
    assert_eq!(d.summary().interventions[0].outcome, "finish");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_the_question_did_not_offer_is_ignored() {
    // no other route in the request, so try_model is not among the options
    let d = Direct::start(opts("gave_up_then_done", 30, &[])).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::TryModel).model(OTHER),
    );
    finish_after_ignore(&d);
    run(&d).await;
    let (_, ign) = single(&d, "input-ignored");
    assert_eq!(ign["why"], "bad-action", "{ign}");
    assert_eq!(single(&d, "needs-input").1["models"], json!([]));
    assert_eq!(d.summary().interventions[0].outcome, "finish");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_that_is_not_a_route_of_this_run_is_ignored() {
    let mut o = opts("gave_up_then_done", 30, &[]);
    o.failover = vec![route(OTHER)];
    let d = Direct::start(o).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::TryModel).model("us.anthropic.claude-not-a-route-of-this-run"),
    );
    finish_after_ignore(&d);
    run(&d).await;
    let (_, ign) = single(&d, "input-ignored");
    assert_eq!(ign["why"], "bad-model", "{ign}");
    assert!(d.model_switches().is_empty(), "the engine was not switched");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_users_fresh_session_cap_refuses_one_more() {
    let mut o = opts("gave_up_then_done", 30, &fresh_env());
    o.edit_cfg = tuned(|c| c.direct.user_sessions_max = 0);
    let d = Direct::start(o).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::NewSessionWithPrompt).text("start over"),
    );
    finish_after_ignore(&d);
    run(&d).await;
    let (_, ign) = single(&d, "input-ignored");
    // a question may leave the action out of its options once the cap is zero (bad-action) or offer it and refuse it (limit)
    assert!(
        ["limit", "bad-action"].contains(&ign["why"].as_str().unwrap()),
        "{ign}"
    );
    assert!(d.aws.progress("new-session").is_empty());
    assert!(d.summary().fresh_sessions.is_empty());
}

// ---- races --------------------------------------------------------------------------------------------------------------------

/// A Cancel and an answer in one batch: the cancel wins for the whole batch, the answer is acknowledged as ignored, and the work stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_in_the_same_batch_as_the_answer_wins() {
    let d = Direct::start(opts("gave_up_then_done", 30, &[])).await;
    d.aws.instruct_batch(
        "needs-input",
        vec![
            PendingControl::Cancel,
            Answer::new(InstructAction::ContinueWithPrompt)
                .text("go on")
                .control(),
        ],
    );
    let resp = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Cancelled);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedByUser, "{s:?}");
    assert_eq!(s.label, "Cline stopped: you stopped the run");
    let (_, ign) = single(&d, "input-ignored");
    assert_eq!(ign["why"], "window-closed", "{ign}");
    let (_, closed) = single(&d, "input-closed");
    assert_eq!(closed["how"], "cancelled");
    assert_eq!(d.prompts_received().len(), 1, "the answer was not acted on");
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());
    assert_eq!(s.interventions[0].outcome, "cancelled");
}

/// The user's client sends the same answer again (it saw no acknowledgement): the request id makes it a no-op.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_answer_twice_is_acted_on_once() {
    let d = Direct::start(opts("gave_up_then_done", 30, &[])).await;
    // the client sent its answer again: both reach agentd in one pending list (a later response could arrive after the short run ended)
    let answer = Answer::new(InstructAction::ContinueWithPrompt)
        .text("try again")
        .control();
    d.aws
        .instruct_batch("needs-input", vec![answer.clone(), answer]);
    run(&d).await;
    assert_eq!(
        d.aws.sent_controls().len(),
        2,
        "the plane handed it out twice"
    );
    single(&d, "input-received");
    let (_, ign) = single(&d, "input-ignored");
    assert_eq!(ign["why"], "already-answered", "{ign}");
    assert_eq!(
        d.prompts_received().len(),
        2,
        "the prompt and the answer, once (acted on twice it would be 3)"
    );
    assert_eq!(d.summary().interventions.len(), 1);
}

/// The response that carried the answer is lost: the plane (as control-api does after twelve seconds) hands it out again, and the run
/// acts on it once.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_response_does_not_lose_the_answer() {
    let d = Direct::start(opts("gave_up_then_done", 30, &[])).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::ContinueWithPrompt).text("please do it"),
    );
    d.aws.swallow_response();
    run(&d).await;
    assert_eq!(d.aws.swallowed(), 1);
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("events post failed")),
        "the unreadable response is logged: {:?}",
        d.log.lines()
    );
    // the question was posted once even though its batch was posted again
    assert_eq!(d.aws.progress("needs-input").len(), 1);
    single(&d, "input-received");
    assert!(has_work(&d));
    assert_eq!(d.prompts_received().last().unwrap(), "please do it");
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
}

/// A reader of the pending list that cannot parse one control still takes the others: an unknown control kind beside the answer.
#[tokio::test(flavor = "multi_thread")]
async fn the_answer_is_taken_when_the_response_also_carries_a_control_agentd_cannot_read() {
    let d = Direct::start(opts("gave_up_then_done", 30, &[])).await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::ContinueWithPrompt).text("go ahead"),
    );
    d.aws
        .add_unreadable_control_on("needs-input", json!({"kind": "future-thing", "x": 1}));
    run(&d).await;
    single(&d, "input-received");
    assert!(has_work(&d));
}

// ---- the dying engine -----------------------------------------------------------------------------------------------------------

/// The engine process exits while the harness waits: the request is closed `superseded`, and the harness starts a fresh session by
/// restarting the process (the automatic one), which finishes the work.
#[tokio::test(flavor = "multi_thread")]
async fn an_engine_that_dies_while_the_run_waits_closes_the_question_and_restarts() {
    let mut env = fresh_env();
    env.push(("FAKE_ACP_EXIT_AFTER_PROMPT", "1:1"));
    env.push(("FAKE_ACP_KEEP_CURSOR", "1"));
    let d = Direct::start(opts("gave_up_then_done", 30, &env)).await;
    run(&d).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{s:?}");
    assert!(has_work(&d));
    let (_, ns) = single(&d, "new-session");
    assert_eq!(ns["trigger"], "harness");
    assert_eq!(ns["reason"], "engine_died");
    assert_eq!(ns["restartedProcess"], true);
    assert!(ns["requestId"].is_null(), "{ns}");
    // a question that was open when the pipe closed is closed as superseded, never left open
    for (id, _) in d.aws.progress("needs-input") {
        let closed = d
            .aws
            .progress("input-closed")
            .into_iter()
            .find(|(_, v)| v["requestId"] == id.as_str())
            .unwrap_or_else(|| panic!("question {id} was left open: {:?}", d.aws.progress_kinds()));
        assert_eq!(closed.1["how"], "superseded", "{closed:?}");
    }
    assert_eq!(s.fresh_sessions.len(), 1);
    assert_eq!(s.fresh_sessions[0].trigger, "harness");
    assert!(s.fresh_sessions[0].restarted_process);
}
