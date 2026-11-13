//! Message control end to end, offline: the whole agentd runs in process against the fake AWS and the
//! fake engine with the measured behaviour of the pinned Cline (`FAKE_ACP_CLINE=1`: an internal queue that swallows a second prompt, a
//! cancel that kills a shell tool and leaves its orphans, an engine that can hang or die), with the module registered and the server
//! model (`MemoryStore`) standing in for the control plane. The control plane's hand-out is a pump that offers `store.handout()` to the
//! module every 100 ms, as the events response does every 2 s.
//!
//! Every scenario checks the property the decision record leads with, in the engine's own log: no prompt ever overlapped another
//! (`overlap` never appears and every `prompt-open` is followed by its `prompt-end` before the next one opens), and each scenario that
//! needs a condition to be present asserts it ("TRIGGER PRESENT").
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_messages_world;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::hooks::{DirectHook, Hooks};
use agentd::direct::messages::{MemoryStore, Message, MessagesHook, Mode, RejectReason, State};
use direct_messages_world::*;
use direct_support::{Direct, Opts};
use harness_protocol::direct::DirectOutcome;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---- a message while Cline works -----------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_message_sent_while_cline_works_is_delivered_after_the_turn_as_its_own_turn() {
    let w = World::start(&directive(r#"{"work_ms": 1500}"#), &[], quick(), &pid_dir()).await;
    let run = w.run();
    w.pump();
    w.until("the first prompt to be open", 20, |w| {
        w.count("prompt-open") == 1
    })
    .await;
    w.send(
        "m1",
        r#"also add a note @fake{"say": "M1-RAN"}"#,
        Mode::Next,
    )
    .await;
    // TRIGGER PRESENT: the message was accepted while the first prompt was open.
    assert_eq!(
        w.count("prompt-end"),
        0,
        "the first turn had already ended: the test proves nothing"
    );
    let resp = run.await.unwrap();
    w.assert_serial();
    assert_eq!(
        w.d.prompts_received(),
        [
            directive(r#"{"work_ms": 1500}"#),
            r#"also add a note @fake{"say": "M1-RAN"}"#.to_string()
        ],
        "the engine saw the task, then the message, each as its own prompt"
    );
    assert_eq!(w.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert_eq!(
        w.d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{resp:?}"
    );
    assert!(!w.store.admission_open(), "the run was sealed");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_messages_are_two_turns_in_the_order_they_were_sent() {
    let w = World::start(&directive(r#"{"work_ms": 1200}"#), &[], quick(), &pid_dir()).await;
    let run = w.run();
    w.pump();
    w.until("the first prompt to be open", 20, |w| {
        w.count("prompt-open") == 1
    })
    .await;
    w.send("m1", r#"first @fake{"say": "ONE"}"#, Mode::Next)
        .await;
    w.send("m2", r#"second @fake{"say": "TWO"}"#, Mode::Next)
        .await;
    assert_eq!(w.count("prompt-end"), 0);
    run.await.unwrap();
    w.assert_serial();
    let got = w.d.prompts_received();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(
        got[1].starts_with("first") && got[2].starts_with("second"),
        "{got:?}"
    );
    assert_eq!(w.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert_eq!(w.store.state("m2"), Some(State::Delivered { turn: 3 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_message_never_reaches_the_engine() {
    let w = World::start(&directive(r#"{"work_ms": 1200}"#), &[], quick(), &pid_dir()).await;
    let run = w.run();
    w.pump();
    w.until("the first prompt to be open", 20, |w| {
        w.count("prompt-open") == 1
    })
    .await;
    w.send("m1", "never mind this one", Mode::Next).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        w.count("prompt-end"),
        0,
        "still working: the withdrawal is early enough"
    );
    w.store.withdraw("m1").unwrap();
    run.await.unwrap();
    w.assert_serial();
    assert_eq!(
        w.d.prompts_received().len(),
        1,
        "only the task was ever sent"
    );
    assert_eq!(w.store.state("m1"), Some(State::Withdrawn));
    assert!(
        w.store.calls().iter().any(|c| c.starts_with("claim m1")),
        "TRIGGER PRESENT: the module tried the claim and lost it: {:?}",
        w.store.calls()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_accepted_at_the_last_moment_is_found_by_the_seal_and_delivered() {
    // The completion race: no hand-out reaches the module (the pump is not started), the first turn ends, and the seal finds the message.
    let w = World::start(&directive(r#"{"work_ms": 800}"#), &[], quick(), &pid_dir()).await;
    let run = w.run();
    w.until("the first prompt to be open", 20, |w| {
        w.count("prompt-open") == 1
    })
    .await;
    w.send(
        "m1",
        r#"just before the end @fake{"say": "LATE-RAN"}"#,
        Mode::Next,
    )
    .await;
    assert_eq!(w.count("prompt-end"), 0);
    run.await.unwrap();
    w.assert_serial();
    assert_eq!(w.d.prompts_received().len(), 2);
    assert_eq!(w.store.state("m1"), Some(State::Delivered { turn: 2 }));
    let seals: Vec<String> = w
        .store
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("seal") || c.starts_with("claim") || c.starts_with("submitted"))
        .collect();
    assert_eq!(
        seals
            .iter()
            .map(|s| s.split(' ').next().unwrap())
            .collect::<Vec<_>>(),
        ["seal", "claim", "submitted", "seal"],
        "{seals:?}"
    );
    assert!(!w.store.admission_open());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_after_the_seal_is_refused_with_the_reason_and_stores_nothing() {
    let w = World::start(&directive(r#"{"work_ms": 100}"#), &[], quick(), &pid_dir()).await;
    w.run().await.unwrap();
    w.assert_serial();
    assert!(
        w.store.calls().iter().any(|c| c.starts_with("seal")),
        "TRIGGER PRESENT: the module sealed the run"
    );
    let r = w.store.accept(Message {
        id: "late".into(),
        text: "too late".into(),
        mode: Mode::Next,
        epoch: "x".into(),
        seq: 0,
    });
    assert_eq!(r, Err(RejectReason::RunSealed));
    assert_eq!(w.store.state("late"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_with_no_message_ends_exactly_as_it_did_before_the_module() {
    let w = World::start(
        &directive(r#"{"work_ms": 100, "say": "DONE: all of it"}"#),
        &[],
        quick(),
        &pid_dir(),
    )
    .await;
    w.run().await.unwrap();
    w.assert_serial();
    assert_eq!(w.d.prompts_received().len(), 1);
    assert_eq!(w.d.summary().outcome, DirectOutcome::ReportedDone);
    let claims = w
        .store
        .calls()
        .iter()
        .filter(|c| c.starts_with("claim"))
        .count();
    assert_eq!(claims, 0);
}

// ---- /now -------------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn now_cancels_the_turn_waits_for_it_to_settle_and_prompts_on_the_same_session() {
    let pids = pid_dir();
    // The task runs a shell tool for a minute.
    let task = directive(&format!(
        r#"{{"tool": {{"secs": 60, "pidfile": "{}"}}}}"#,
        pidfile(&pids, "tool.pids")
    ));
    let w = World::start(&task, &[], quick(), &pids).await;
    let run = w.run();
    w.pump();
    w.until("the tool to run", 20, |w| {
        !w.pids_of("tool.pids").is_empty()
    })
    .await;
    let tool = w.pids_of("tool.pids")[0];
    // TRIGGER PRESENT: a prompt is open and its shell tool is running.
    assert!(alive(tool));
    assert_eq!(w.count("prompt-open"), 1);
    assert_eq!(w.count("prompt-end"), 0);
    let t = Instant::now();
    w.send(
        "m1",
        r#"stop that, say hi instead @fake{"say": "REDIRECTED"}"#,
        Mode::Now,
    )
    .await;
    w.until("the second prompt to open", 20, |w| {
        w.count("prompt-open") == 2
    })
    .await;
    let took = t.elapsed();
    run.await.unwrap();
    w.assert_serial();
    let log = w.log();
    assert_eq!(
        log.matches("\ncancel ").count(),
        1,
        "exactly one session/cancel:\n{log}"
    );
    assert!(log.contains("acted=true"), "{log}");
    assert!(
        log.find("prompt-end").unwrap() < log.rfind("prompt-open").unwrap(),
        "the new prompt went out after the old one ended:\n{log}"
    );
    assert!(log.contains("stop=cancelled"), "{log}");
    assert!(took < Duration::from_secs(10), "{took:?}");
    assert!(!alive(tool), "the shell tool is gone");
    assert_eq!(w.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert_eq!(w.d.summary().outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(flavor = "multi_thread")]
async fn after_a_now_the_tools_the_cancel_left_behind_are_reaped_before_the_new_prompt() {
    let pids = pid_dir();
    // survive_cancel: the engine does not kill its tool on the cancel, and the tool left an orphan
    let task = directive(&format!(
        r#"{{"tool": {{"secs": 60, "orphan": true, "survive_cancel": true, "pidfile": "{}"}}}}"#,
        pidfile(&pids, "orphan.pids")
    ));
    let w = World::start(&task, &[], quick(), &pids).await;
    let run = w.run();
    w.pump();
    w.until("the tool and its orphan", 20, |w| {
        w.pids_of("orphan.pids").len() >= 2
    })
    .await;
    let procs = w.pids_of("orphan.pids");
    assert!(
        procs.iter().all(|p| alive(*p)),
        "TRIGGER PRESENT: {procs:?}"
    );
    w.send("m1", r#"redirect @fake{"say": "REDIRECTED"}"#, Mode::Now)
        .await;
    run.await.unwrap();
    w.assert_serial();
    assert!(
        all_gone(&procs, 5).await,
        "the tool and the orphan outlived the interrupt: {procs:?}"
    );
    assert_eq!(w.store.state("m1"), Some(State::Delivered { turn: 2 }));
}

// ---- the engine does not answer the cancel -------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_the_engine_ignores_demotes_the_message_to_next_and_it_is_delivered_after_the_restart()
 {
    let pids = pid_dir();
    let task = directive(&format!(
        r#"{{"gen": 1, "work_ms": 100, "ignore_cancel": true, "tool": {{"secs": 60, "orphan": true, "survive_cancel": true, "pidfile": "{}"}}}}"#,
        pidfile(&pids, "stuck.pids")
    ));
    let w = World::start(&task, &[("FAKE_ACP_CLINE_WORK_MS", "800")], quick(), &pids).await;
    let run = w.run();
    w.pump();
    w.until("the tool and its orphan", 20, |w| {
        w.pids_of("stuck.pids").len() >= 2
    })
    .await;
    let procs = w.pids_of("stuck.pids");
    w.send("m1", r#"redirect @fake{"say": "REDIRECTED"}"#, Mode::Now)
        .await;
    let resp = run.await.unwrap();
    // TRIGGER PRESENT: the 10 s grace passed with no answer, the processes were reaped and the engine was replaced.
    assert!(all_gone(&procs, 5).await, "{procs:?}");
    assert!(
        w.d.engine_sessions().len() >= 2,
        "TRIGGER PRESENT: a second engine process: {:?}",
        w.d.engine_sessions()
    );
    // The message is demoted to `next`, not held: its claim is kept (nothing is given back), the restart does not hold it, and it
    // is delivered after the turn, in the new session.
    let h = w.store.history("m1");
    assert!(
        !w.store.calls().iter().any(|c| c == "release m1"),
        "the claim is kept: {:?}",
        w.store.calls()
    );
    assert!(!h.contains(&State::Held), "never held: {h:?}\n{resp:?}");
    assert!(
        matches!(w.store.state("m1"), Some(State::Delivered { .. })),
        "delivered after the restart: {h:?}"
    );
    let sent =
        w.d.prompts_received()
            .iter()
            .filter(|p| p.starts_with("redirect"))
            .count();
    assert_eq!(sent, 1, "the text reached the engine exactly once");
    // The user sees why, in the words of the decision record, and the run asks no question.
    let notices =
        w.d.events_of(harness_protocol::envelope::EventType::TurnProgress);
    assert!(
        notices.iter().any(|v| v["kind"] == "notify"
            && v["detail"] == "could not interrupt; delivered after this turn"),
        "{notices:?}"
    );
    assert!(
        notices.iter().all(|v| v["kind"] != "needs-input"),
        "an interrupt stop opens no needs-input question: {notices:?}"
    );
    // The killed engine never ended its prompt, so the strict open/end order of `assert_serial` cannot hold across the restart; what
    // holds is that no engine ever noted an overlap.
    assert!(!w.log().contains("overlap"), "{}", w.log());
    assert_eq!(w.d.summary().outcome, DirectOutcome::ReportedDone);
}

// ---- /now during a model stream and during a file edit (the shell tool is covered above) --------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn now_interrupts_a_model_stream_and_the_next_turn_continues_with_nothing_late_from_the_old_one()
 {
    let pids = pid_dir();
    let task = directive(r#"{"stream": {"chunks": 80, "step_ms": 50}}"#);
    let w = World::start(&task, &[], quick(), &pids).await;
    let run = w.run();
    w.pump();
    w.until("the stream to run", 20, |w| {
        w.log().matches("stream-").count() >= 3
    })
    .await;
    // TRIGGER PRESENT: a prompt is open and its answer is still streaming.
    assert_eq!(w.count("prompt-open"), 1);
    assert_eq!(w.count("prompt-end"), 0);
    w.send(
        "m1",
        r#"stop talking, say hi instead @fake{"say": "REDIRECTED"}"#,
        Mode::Now,
    )
    .await;
    run.await.unwrap();
    w.assert_serial();
    let log = w.log();
    assert!(
        log.contains("stop=cancelled"),
        "the stream was cancelled:\n{log}"
    );
    assert!(
        log.matches("stream-").count() < 80 * 2,
        "the stream did not run to its end:\n{log}"
    );
    // nothing of the cancelled turn arrives after its answer
    let cancelled_at = log.find("stop=cancelled").unwrap();
    assert!(
        !log[cancelled_at..].contains("stream-"),
        "a late chunk of the cancelled turn:\n{log}"
    );
    assert_eq!(w.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert_eq!(w.hook.stats().interrupts, 1);
    assert!(
        w.d.prompts_received()
            .last()
            .unwrap()
            .starts_with("stop talking")
    );
    assert_eq!(w.d.summary().outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(flavor = "multi_thread")]
async fn now_interrupts_a_file_edit_in_parts_and_the_next_turn_completes_it_with_no_late_write() {
    let pids = pid_dir();
    let task = directive(
        r#"{"edit": {"path": "src/edit.txt", "parts": ["part one\n", "part two\n", "part three\n", "part four\n", "part five\n", "part six\n"], "step_ms": 400}}"#,
    );
    let w = World::start(&task, &[], quick(), &pids).await;
    let run = w.run();
    w.pump();
    let edit = w.d.worktree().join("src/edit.txt");
    w.until("the first part of the edit", 20, |_| {
        std::fs::read_to_string(&edit).is_ok_and(|t| t.contains("part one"))
    })
    .await;
    // TRIGGER PRESENT: the edit is partly done and its prompt is still open.
    let partial = std::fs::read_to_string(&edit).unwrap();
    assert!(!partial.contains("part six"), "{partial:?}");
    assert_eq!(w.count("prompt-end"), 0);
    w.send(
        "m1",
        r#"finish the file in one go @fake{"write": {"path": "src/edit.txt", "text": "complete\n"}}"#,
        Mode::Now,
    )
    .await;
    run.await.unwrap();
    w.assert_serial();
    let log = w.log();
    assert!(
        log.contains("stop=cancelled"),
        "the edit was cancelled:\n{log}"
    );
    // the cancelled turn wrote no part after the new turn rewrote the file
    assert_eq!(std::fs::read_to_string(&edit).unwrap(), "complete\n");
    assert_eq!(w.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert_eq!(w.d.summary().outcome, DirectOutcome::ReportedDone);
}

// ---- the session changes ---------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_message_for_the_old_session_is_held_after_the_engine_dies_and_only_a_confirmed_copy_runs()
 {
    let w = World::start(
        &directive(r#"{"gen": 1, "work_ms": 5000, "die_ms": 700}"#),
        &[("FAKE_ACP_CLINE_WORK_MS", "2500")],
        quick(),
        &pid_dir(),
    )
    .await;
    let run = w.run();
    w.pump();
    w.until("the first prompt", 20, |w| w.count("prompt-open") == 1)
        .await;
    let old = w.epoch().await;
    w.send("m1", "for the first session", Mode::Next).await;
    // the engine dies, the driver starts a fresh session, the epoch moves on
    w.until("a new epoch", 30, |w| {
        w.store.current_epoch().is_some_and(|e| e != old)
    })
    .await;
    let new = w.store.current_epoch().unwrap();
    assert_eq!(
        w.store.state("m1"),
        Some(State::Held),
        "{:?}\ncalls: {:?}\nlog: {:?}\nengine log:\n{}",
        w.store.history("m1"),
        w.store.calls(),
        w.d.log.lines(),
        w.log()
    );
    w.store
        .confirm(
            "m1",
            &new,
            Message {
                id: "m1b".into(),
                text: "for the first session, confirmed".into(),
                mode: Mode::Next,
                epoch: new.clone(),
                seq: 0,
            },
        )
        .unwrap();
    run.await.unwrap();
    assert_eq!(w.store.state("m1"), Some(State::Superseded));
    assert!(
        matches!(w.store.state("m1b"), Some(State::Delivered { .. })),
        "{:?}",
        w.store.history("m1b")
    );
    let got = w.d.prompts_received();
    assert!(
        got.iter().all(|p| p != "for the first session"),
        "the held original never ran: {got:?}"
    );
    assert!(
        got.iter().any(|p| p == "for the first session, confirmed"),
        "{got:?}"
    );
}

// ---- the user's cancel ------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_users_cancel_stays_final_the_pending_message_is_never_sent_and_ends_rejected_for_it() {
    let w = World::start(
        &directive(r#"{"work_ms": 20000}"#),
        &[],
        quick(),
        &pid_dir(),
    )
    .await;
    let run = w.run();
    w.pump();
    w.until("the first prompt", 20, |w| w.count("prompt-open") == 1)
        .await;
    w.send("m1", "queued behind a cancel", Mode::Next).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    w.store.queue_cancel();
    w.canceller.cancel();
    run.await.unwrap();
    assert_eq!(w.d.summary().outcome, DirectOutcome::StoppedByUser);
    assert_eq!(
        w.d.prompts_received().len(),
        1,
        "the message was not sent after the cancel"
    );
    assert_eq!(
        w.store.state("m1"),
        Some(State::Rejected(RejectReason::RunCancelled))
    );
    assert!(!w.store.admission_open());
}

// ---- the tripwire -----------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn an_end_turn_with_no_usage_and_no_update_is_never_reported_as_done() {
    let w = World::start(
        &directive(r#"{"gen": 1, "queued_reply": true, "say": "DRAINED"}"#),
        &[("FAKE_ACP_CLINE_WORK_MS", "300")],
        quick(),
        &pid_dir(),
    )
    .await;
    let resp = w.run().await.unwrap();
    // TRIGGER PRESENT: the engine answered the task from its queue.
    assert!(
        w.log().contains("overlap"),
        "the engine did not queue the prompt:\n{}",
        w.log()
    );
    assert!(
        w.d.log.lines().iter().any(|l| l.contains("tripwire")),
        "the module did not trip: {:?}",
        w.d.log.lines()
    );
    // The engine was replaced (a second process, a second session) instead of the run ending at that reply.
    assert!(
        w.d.engine_sessions().len() >= 2,
        "{:?}\n{resp:?}",
        w.d.engine_sessions()
    );
}

// ---- the retry ladder with a message pending ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_retry_ladder_keeps_working_with_a_message_pending() {
    let w = World::start(
        &directive(r#"{"fail_times": 2, "work_ms": 100}"#),
        &[],
        quick(),
        &pid_dir(),
    )
    .await;
    let run = w.run();
    w.pump();
    w.until("the first prompt", 20, |w| {
        w.count("prompt-open") + w.count("prompt-end") >= 1
    })
    .await;
    w.send(
        "m1",
        r#"queued during the outage @fake{"say": "AFTER-OUTAGE", "write": {"path": "outage.txt", "text": "x"}}"#,
        Mode::Next,
    )
    .await;
    run.await.unwrap();
    w.assert_serial();
    let got = w.d.prompts_received();
    // the task (answered with the provider's 503), the ladder's resume sentence (answered), then the message
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[1], agentd::direct::retry::RESUME_PROMPT);
    assert!(got[2].starts_with("queued during the outage"));
    assert_eq!(w.store.state("m1"), Some(State::Delivered { turn: 3 }));
    assert_eq!(w.d.summary().outcome, DirectOutcome::ReportedDone);
}

// ---- the bounded continuation -----------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn messages_after_done_are_bounded_and_the_rest_are_rejected_for_the_limit() {
    let mut s = quick();
    s.max_turns = Some(3);
    let w = World::start(
        &directive(r#"{"work_ms": 800, "say": "DONE"}"#),
        &[],
        s,
        &pid_dir(),
    )
    .await;
    let run = w.run();
    w.pump();
    w.until("the first prompt", 20, |w| w.count("prompt-open") == 1)
        .await;
    for i in 1..=5 {
        w.send(
            &format!("m{i}"),
            &format!("message {i} @fake{{\"say\": \"DONE\"}}"),
            Mode::Next,
        )
        .await;
    }
    run.await.unwrap();
    w.assert_serial();
    let delivered = (1..=5)
        .filter(|i| {
            matches!(
                w.store.state(&format!("m{i}")),
                Some(State::Delivered { .. })
            )
        })
        .count();
    assert_eq!(delivered, 3, "{:?}", w.store.states());
    for i in 4..=5 {
        assert_eq!(
            w.store.state(&format!("m{i}")),
            Some(State::Rejected(RejectReason::ContinuationLimit))
        );
    }
    assert_eq!(
        w.d.prompts_received().len(),
        4,
        "the task and three messages"
    );
    assert!(!w.store.admission_open());
}

// ---- the cap of five interrupts ------------------------------------------------------------------------------------------------------

/// The text of a message whose turn is long (`work_ms` of model work, a minute in most uses), so the next interrupt finds a prompt to cut.
fn long_turn(label: &str, work_ms: u64) -> String {
    format!(r#"{label} @fake{{"work_ms": {work_ms}}}"#)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sixth_interrupt_is_not_made_the_message_goes_out_after_the_turn_and_the_user_is_told() {
    // Five interrupts per run. The task ends, a first message starts the allowance (item 13) and each of five `now`
    // messages cuts the turn before it. The sixth finds the cap spent: no sixth `session/cancel`, the message waits for the turn to end,
    // and the user is told why (the words of the decision record, as for a cancel the engine ignores).
    let w = World::start(&directive(r#"{"work_ms": 1200}"#), &[], quick(), &pid_dir()).await;
    let run = w.run();
    w.pump();
    w.until("the first prompt to be open", 20, |w| {
        w.count("prompt-open") == 1
    })
    .await;
    w.send("m0", &long_turn("long turn zero", 60_000), Mode::Next)
        .await;
    let mut seen: Vec<(u32, Duration)> = Vec::new();
    for k in 1..=5usize {
        w.until(&format!("prompt {} to be open", k + 1), 30, |w| {
            w.count("prompt-open") == k + 1
        })
        .await;
        // The allowance counts the turn that is open now: the message turns so far are m0 to m(k-1) and this prompt is one of them.
        let turns = u32::try_from(k).unwrap();
        w.until("the allowance to count this turn", 5, |w| {
            w.hook.allowance().0 == turns
        })
        .await;
        seen.push(w.hook.allowance());
        let work = if k == 5 { 5_000 } else { 60_000 };
        w.send(
            &format!("m{k}"),
            &long_turn(&format!("redirect {k}"), work),
            Mode::Now,
        )
        .await;
        w.until(&format!("interrupt {k} to take"), 30, |w| {
            w.hook.stats().interrupts == u32::try_from(k).unwrap()
        })
        .await;
    }
    // The last interrupt's message is the open prompt now (the 7th of the run); it works for five seconds.
    w.until("prompt 7 to be open", 30, |w| w.count("prompt-open") == 7)
        .await;
    // TRIGGER PRESENT: five interrupts are spent, a prompt is open and has not ended.
    assert_eq!(w.hook.stats().interrupts, 5, "{:?}", w.hook.stats());
    assert_eq!(w.count("prompt-end"), 6, "prompt 7 is still open");
    assert_eq!(
        w.log().matches("\ncancel ").count(),
        5,
        "five cancels so far:\n{}",
        w.log()
    );
    w.send("m6", r#"the sixth @fake{"say": "SIXTH"}"#, Mode::Now)
        .await;
    let resp = run.await.unwrap();
    w.assert_serial();
    // No sixth cancel: the message waited for prompt 7 to end and was delivered as prompt 8, once.
    let log = w.log();
    assert_eq!(
        log.matches("\ncancel ").count(),
        5,
        "a sixth interrupt was made:\n{log}"
    );
    assert_eq!(w.hook.stats().interrupts, 5);
    assert_eq!(w.store.state("m6"), Some(State::Delivered { turn: 8 }));
    let got = w.d.prompts_received();
    assert_eq!(got.len(), 8, "{got:?}");
    assert!(got[7].starts_with("the sixth"), "{got:?}");
    assert_eq!(
        w.d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{resp:?}"
    );
    // The user sees why the message did not interrupt, once, and the run asks no question.
    let notices =
        w.d.events_of(harness_protocol::envelope::EventType::TurnProgress);
    let told = notices
        .iter()
        .filter(|v| {
            v["kind"] == "notify" && v["detail"] == "could not interrupt; delivered after this turn"
        })
        .count();
    assert_eq!(
        told,
        1,
        "the user is told once that the sixth message could not interrupt: {notices:?}\nagentd log: {:#?}",
        w.d.log.lines()
    );
    assert!(
        notices.iter().all(|v| v["kind"] != "needs-input"),
        "an interrupt that is not made opens no question: {notices:?}"
    );
    // Item 13: each interrupt added its turn to the allowance and none reset it. The turns counted at each prompt are 1 to 5, the sixth
    // message makes the allowance 7 turns of the 8, and the time only grew.
    assert_eq!(
        seen.iter().map(|s| s.0).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    assert!(
        seen.windows(2).all(|p| p[0].1 <= p[1].1),
        "the extra time only grows: {seen:?}"
    );
    let (turns, exec) = w.hook.allowance();
    assert_eq!(turns, 7, "m0 to m6");
    assert!(exec >= seen[4].1, "{exec:?} after {:?}", seen[4].1);
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupts_count_against_the_allowance_and_the_limit_still_holds_after_them() {
    // The allowance is not reset by an interrupt. With three turns allowed, a first message and two interrupts use them,
    // and the next message ends rejected for the limit.
    let mut s = quick();
    s.max_turns = Some(3);
    let w = World::start(&directive(r#"{"work_ms": 1200}"#), &[], s, &pid_dir()).await;
    let run = w.run();
    w.pump();
    w.until("the first prompt to be open", 20, |w| {
        w.count("prompt-open") == 1
    })
    .await;
    w.send("m0", &long_turn("long turn zero", 60_000), Mode::Next)
        .await;
    w.until("prompt 2 to be open", 30, |w| w.count("prompt-open") == 2)
        .await;
    w.send("m1", &long_turn("redirect 1", 60_000), Mode::Now)
        .await;
    w.until("prompt 3 to be open", 30, |w| w.count("prompt-open") == 3)
        .await;
    w.send("m2", &long_turn("redirect 2", 3_000), Mode::Now)
        .await;
    w.until("prompt 4 to be open", 30, |w| w.count("prompt-open") == 4)
        .await;
    // TRIGGER PRESENT: two interrupts were made and the allowance of three turns is used up.
    assert_eq!(w.hook.stats().interrupts, 2);
    assert_eq!(w.hook.allowance().0, 3);
    w.send("m3", r#"one too many @fake{"say": "DONE"}"#, Mode::Next)
        .await;
    run.await.unwrap();
    w.assert_serial();
    assert_eq!(
        w.store.state("m3"),
        Some(State::Rejected(RejectReason::ContinuationLimit)),
        "{:?}",
        w.store.history("m3")
    );
    assert!(
        !w.d.prompts_received()
            .iter()
            .any(|p| p.starts_with("one too many")),
        "the message never reached the engine"
    );
    assert_eq!(w.hook.allowance().0, 3);
    assert_eq!(w.d.summary().outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_now_message_beyond_the_allowance_is_refused_and_the_running_turn_is_not_cancelled() {
    // follow-up: with one message turn allowed and in flight, a `now` message would be refused for the limit anyway. It must not
    // cancel the turn that is running (and the engine must not be resumed with the "interrupted" sentence): the turn runs to its end, the
    // message is refused at once without a claim, and the user is not told it will be delivered.
    let mut s = quick();
    s.max_turns = Some(1);
    let w = World::start(&directive(r#"{"work_ms": 1200}"#), &[], s, &pid_dir()).await;
    let run = w.run();
    w.pump();
    w.until("the first prompt to be open", 20, |w| {
        w.count("prompt-open") == 1
    })
    .await;
    w.send("m0", &long_turn("long turn zero", 4_000), Mode::Next)
        .await;
    w.until("prompt 2 to be open", 30, |w| w.count("prompt-open") == 2)
        .await;
    // TRIGGER PRESENT: the one allowed turn is in flight, so the allowance is spent and a prompt is open.
    assert_eq!(w.hook.allowance().0, 1);
    assert_eq!(w.count("prompt-end"), 1, "prompt 2 is still open");
    w.send("m1", r#"redirect @fake{"say": "REDIRECTED"}"#, Mode::Now)
        .await;
    run.await.unwrap();
    w.assert_serial();
    let log = w.log();
    assert_eq!(
        log.matches("\ncancel ").count(),
        0,
        "the running turn was cancelled for a message that is refused anyway:\n{log}"
    );
    assert!(
        !log.contains("stop=cancelled"),
        "no prompt was cancelled:\n{log}"
    );
    let got = w.d.prompts_received();
    assert_eq!(
        got.len(),
        2,
        "the task and the one message; no resume sentence, not the redirect: {got:?}"
    );
    assert_eq!(
        w.store.state("m1"),
        Some(State::Rejected(RejectReason::ContinuationLimit)),
        "{:?}",
        w.store.history("m1")
    );
    assert!(
        !w.store.history("m1").contains(&State::Dispatching),
        "refused without a claim: {:?}",
        w.store.history("m1")
    );
    let notices =
        w.d.events_of(harness_protocol::envelope::EventType::TurnProgress);
    assert!(
        notices
            .iter()
            .all(|v| v["detail"] != "could not interrupt; delivered after this turn"),
        "the message will not be delivered, so the user is not told it will: {notices:?}"
    );
    assert!(notices.iter().all(|v| v["kind"] != "needs-input"));
    assert_eq!(w.hook.allowance().0, 1);
    assert_eq!(w.d.summary().outcome, DirectOutcome::ReportedDone);
}

// ---- known gap: the harness's own nudge ----------------------------------------------------------------------------------------------

/// Never send a nudge while a user message is pending. The driver sends the nudge inside the stage, before the module is
/// asked, so today the nudge goes first. This test states the wanted order; it runs when the driver offers the module a say before the
/// nudge (the proposal is in the IM2 report: `DirectHook::before_nudge`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the driver hook before_nudge (proposal in the IM2 report)"]
async fn a_pending_message_goes_before_the_harness_nudge() {
    let d = Direct::start(Opts {
        prompt: directive(r#"{"work_ms": 800}"#),
        engine_env: env(&[]),
        edit_cfg: cfg(),
        nudge: Some(harness_protocol::api::v1::ContinueSettings {
            text: "NUDGE: reply DONE if you are finished.".into(),
            sha256: direct_support::sha(b"NUDGE: reply DONE if you are finished."),
            max_nudges: 1,
        }),
        ..Opts::default()
    })
    .await;
    let store = MemoryStore::new();
    let hook = MessagesHook::new(store.clone(), quick());
    let deps = Deps {
        hooks: Some(Hooks::new(vec![hook.clone() as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    let (app, req) = (d.app.clone(), d.req.clone());
    let run = tokio::spawn(async move { agentd::direct::run_with(&app, req, &deps).await });
    let end = Instant::now() + Duration::from_secs(20);
    while store.current_epoch().is_none() && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let epoch = store.current_epoch().unwrap();
    store
        .accept(Message {
            id: "m1".into(),
            text: "the user's correction".into(),
            mode: Mode::Next,
            epoch,
            seq: 0,
        })
        .unwrap();
    hook.offer(store.handout());
    run.await.unwrap();
    let got = d.prompts_received();
    let m = got
        .iter()
        .position(|p| p == "the user's correction")
        .unwrap();
    let n = got
        .iter()
        .position(|p| p.starts_with("NUDGE"))
        .unwrap_or(usize::MAX);
    assert!(m < n, "the message must come before the nudge: {got:?}");
}
