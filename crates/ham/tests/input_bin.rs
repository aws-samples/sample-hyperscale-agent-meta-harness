//! `ham say`, `new-session`, `try-model` and `finish` through the real binary against the local `/v1` API, which runs the real
//! control-api handler: the answer reaches the run's control queue as an `Instruct` for the direct agent, the simulated agentd
//! acknowledges it, and the command prints the result. Standard input is the text of `ham say <run> -`.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use harness_protocol::api::internal::PendingControl;
use harness_protocol::direct::InstructAction;
use home::{Home, ham, ham_env, ham_stdin, home, json_lines, new_run, text};
use serde_json::json;
use std::sync::Arc;
use support::Fake;

/// A fake plane whose agentd takes every answer at once, and a run that asked a question.
async fn asked(label: &str) -> (Arc<Fake>, Home, String, String) {
    let fake = Fake::start().await;
    fake.mem.inner.lock().unwrap().auto_ack_input = true;
    let h = home(&fake, label);
    let run = new_run(&fake, label).await;
    fake.mem.add_agent(&run, "direct", "root", "running");
    let q = fake.mem.needs_input(&run, "engine_stop");
    (fake, h, run, q)
}

fn instruct(
    fake: &Fake,
    run: &str,
) -> Vec<(String, InstructAction, Option<String>, Option<String>)> {
    fake.mem
        .controls_of(run)
        .into_iter()
        .filter_map(|c| match c.control {
            PendingControl::Instruct {
                request_id,
                action,
                text,
                model,
                ..
            } => {
                assert_eq!(c.agent_id, "direct");
                assert_eq!(c.control_id, request_id, "the request id is the control id");
                Some((request_id, action, text, model))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn say_queues_the_message_for_the_open_question_and_prints_the_acknowledgement() {
    let (fake, h, run, q) = asked("bin-say").await;
    let o = ham(&h, &["say", &run, "Run the sleeps in the background."]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        text(&o.stdout).trim(),
        "Cline took your message (same session)."
    );
    assert_eq!(
        instruct(&fake, &run),
        vec![(
            q,
            InstructAction::ContinueWithPrompt,
            Some("Run the sleeps in the background.".to_string()),
            None
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lone_dash_reads_the_message_from_standard_input() {
    let (fake, h, run, _q) = asked("bin-stdin").await;
    let o = ham_stdin(&h, &["say", &run, "-"], &[], b"first line\nsecond line\n").await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let sent = instruct(&fake, &run);
    assert_eq!(
        sent[0].2.as_deref(),
        Some("first line\nsecond line"),
        "the final line break is not part of the message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_session_try_model_and_finish_send_what_they_say() {
    let (fake, h, run, _q) = asked("bin-new").await;
    let o = ham(&h, &["new-session", &run, "Start with the failing test."]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        text(&o.stdout).trim(),
        "Cline took your request: a fresh session starts on the same files."
    );
    let sent = instruct(&fake, &run);
    assert_eq!(sent[0].1, InstructAction::NewSessionWithPrompt);
    assert_eq!(sent[0].2.as_deref(), Some("Start with the failing test."));

    // The question is answered; the next question takes the model switch.
    let q2 = fake.mem.needs_input(&run, "no_changes");
    let o = ham(&h, &["try-model", &run, "opus"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        text(&o.stdout).trim(),
        "Switched to Claude Opus 5.5. Cline took the fixed continue message."
    );
    let sent = instruct(&fake, &run);
    assert_eq!(sent[1].0, q2);
    assert_eq!(sent[1].1, InstructAction::TryModel);
    assert_eq!(sent[1].3.as_deref(), Some("us.anthropic.claude-opus-5-5"));

    let q3 = fake.mem.needs_input(&run, "no_changes");
    let o = ham(&h, &["finish", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        text(&o.stdout).trim(),
        "The run will finish with what exists."
    );
    let sent = instruct(&fake, &run);
    assert_eq!(
        (sent[2].0.as_str(), sent[2].1),
        (q3.as_str(), InstructAction::Finish)
    );
    assert_eq!((&sent[2].2, &sent[2].3), (&None, &None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_prints_one_line_with_the_result() {
    let (_fake, h, run, q) = asked("bin-json").await;
    let o = ham(&h, &["finish", &run, "--json"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let lines = json_lines(&o);
    assert_eq!(lines.len(), 1, "{}", text(&o.stdout));
    assert_eq!(
        lines[0],
        json!({"runId": run, "requestId": q, "action": "finish", "state": "queued", "result": "received", "textBytes": 0})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_waits_for_nobody_says_so_and_sends_nothing() {
    let fake = Fake::start().await;
    let h = home(&fake, "bin-none");
    let run = new_run(&fake, "bin-none").await;
    fake.mem.add_agent(&run, "direct", "root", "running");
    let o = ham(&h, &["say", &run, "hello"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        text(&o.stderr).trim(),
        format!("ham: run {run} is not waiting for you. Cline has not asked it a question.")
    );
    assert!(fake.mem.controls_of(&run).is_empty());
    assert!(fake.mem.posts(&format!("/v1/runs/{run}/input")).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answered_question_cannot_be_answered_again_and_the_servers_reason_is_shown() {
    let (fake, h, run, q) = asked("bin-twice").await;
    let o = ham(&h, &["finish", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    // The question is closed in the event log: the command says so without calling the server.
    let o = ham(&h, &["say", &run, "again"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr)
            .contains("is not waiting for you. Its last question was answered with finish."),
        "{}",
        text(&o.stderr)
    );
    // Forcing the same question id reaches the server, which names the answer that won.
    let o = ham(&h, &["say", &run, "again", "--request", &q]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("this question was already answered with finish"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(instruct(&fake, &run).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_id_that_is_not_a_question_of_the_run_is_refused_by_the_server() {
    let (_fake, h, run, _q) = asked("bin-nosuch").await;
    let o = ham_env(
        &h,
        &["finish", &run, "--request", "01K5ZQ4T8M3N6P7R9S0V1W2X3Y"],
        &[],
    )
    .await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("no such question"),
        "{}",
        text(&o.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_wait_returns_when_the_answer_is_queued() {
    let fake = Fake::start().await;
    let h = home(&fake, "bin-nowait");
    let run = new_run(&fake, "bin-nowait").await;
    fake.mem.add_agent(&run, "direct", "root", "running");
    // The simulated agentd does not acknowledge, and the command does not wait for it.
    fake.mem.needs_input(&run, "engine_stop");
    let o = ham(&h, &["finish", &run, "--no-wait"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(
        text(&o.stdout).starts_with("Queued finish for Cline;"),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(instruct(&fake, &run).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_message_is_refused_before_anything_is_sent() {
    let (fake, h, run, _q) = asked("bin-bad").await;
    // The limit of 262,144 bytes cannot be reached on a command line (Linux takes 131,072 bytes in one argument), and `tests/input_cli.rs` sends
    // the oversize text through the same check; a blank message is refused as a usage error.
    let o = ham(&h, &["say", &run, "   "]).await;
    assert_eq!(o.status.code(), Some(2));
    assert!(!text(&o.stderr).is_empty());
    let o = ham(&h, &["try-model", &run, "gpt-9"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("opus"), "{}", text(&o.stderr));
    assert!(fake.mem.controls_of(&run).is_empty());
}
