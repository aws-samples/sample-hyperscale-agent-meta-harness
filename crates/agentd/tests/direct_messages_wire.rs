//! Message control over the wire, offline: the same agentd, the same fake Cline engine and the same server model as `direct_messages_e2e.rs`,
//! but the module talks to the control plane through its production store, the four signed routes of the internal API (`HttpMessageStore`), and
//! the hand-out arrives as `PendingControl::Message` notices that carry no text. The routes are answered by a `Plane` from the model, in the
//! words of the golden files, so a mismatch between the module's wire and the protocol shows here.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_messages_world;
mod direct_support;

use agentd::direct::messages::{Mode, State};
use direct_messages_world::*;
use harness_protocol::direct::DirectOutcome;

#[tokio::test(flavor = "multi_thread")]
async fn a_message_over_the_four_routes_is_claimed_sent_and_acknowledged_in_its_turn() {
    let w =
        World::start_over_the_wire(&directive(r#"{"work_ms": 1500}"#), &[], quick(), &pid_dir())
            .await;
    let run = w.run();
    w.pump();
    w.until("the first prompt", 20, |w| w.count("prompt-open") == 1)
        .await;
    w.send(
        "m1",
        r#"also add a note @fake{"say": "M1-RAN"}"#,
        Mode::Next,
    )
    .await;
    assert_eq!(
        w.count("prompt-end"),
        0,
        "TRIGGER PRESENT: accepted while the first prompt was open"
    );
    run.await.unwrap();
    w.assert_serial();
    assert_eq!(w.d.prompts_received().len(), 2);
    assert_eq!(w.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert_eq!(w.d.summary().outcome, DirectOutcome::ReportedDone);
    // the order of the calls the control plane saw: the session first, then claim, outcome, and the closing seal
    let calls = w.store.calls();
    let kinds: Vec<&str> = calls.iter().map(|c| c.split(' ').next().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"epoch"), "{calls:?}");
    let claim = kinds.iter().position(|k| *k == "claim").unwrap();
    let ack = kinds.iter().position(|k| *k == "submitted").unwrap();
    let seal = kinds.iter().rposition(|k| *k == "seal").unwrap();
    assert!(claim < ack && ack < seal, "{calls:?}");
    assert!(!w.store.admission_open());
}

#[tokio::test(flavor = "multi_thread")]
async fn now_over_the_wire_interrupts_the_tool_and_delivers_the_redirect() {
    let pids = pid_dir();
    let task = directive(&format!(
        r#"{{"tool": {{"secs": 60, "pidfile": "{}"}}}}"#,
        pidfile(&pids, "wire-tool.pids")
    ));
    let w = World::start_over_the_wire(&task, &[], quick(), &pids).await;
    let run = w.run();
    w.pump();
    w.until("the tool", 20, |w| !w.pids_of("wire-tool.pids").is_empty())
        .await;
    let tool = w.pids_of("wire-tool.pids")[0];
    assert!(alive(tool), "TRIGGER PRESENT: a shell tool is running");
    w.send(
        "m1",
        r#"stop and say hi @fake{"say": "REDIRECTED"}"#,
        Mode::Now,
    )
    .await;
    run.await.unwrap();
    w.assert_serial();
    assert!(!alive(tool));
    assert_eq!(w.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert_eq!(w.d.summary().outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plane_without_the_routes_leaves_the_run_exactly_as_it_was() {
    // The production registration (`Hooks::standard`), against a fake plane that serves no message route: every call answers 404, the
    // module marks the plane as one without message control and stays out of the run. The run ends as it did before the module.
    let d = direct_support::Direct::start(direct_support::Opts::default()).await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert_eq!(
        resp.outcome,
        harness_protocol::runtime::TurnOutcome::Completed,
        "{resp:?}"
    );
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    let lines: Vec<String> = d
        .log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct messages"))
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("no-route")),
        "TRIGGER PRESENT: the module reached the plane and learned it has no routes: {lines:?}"
    );
    assert!(
        lines.len() <= 2,
        "one line says it, none repeats it: {lines:?}"
    );
    // The engine carried the tag that marks its processes.
    assert!(
        d.engine_probe().contains("AGENTD_ENGINE_TAG="),
        "{}",
        d.engine_probe()
    );
    // and the module sent nothing else to the plane: no claim, no seal, no outcome
    assert!(d.prompts_received().len() == 1);
}
