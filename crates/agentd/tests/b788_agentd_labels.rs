//! Agentd writes the outcome sentence of the engine that ran. A Cline run keeps the frozen sentence in `summary.json`, in the `turn.ended` event and in
//! the log line; a run of another engine reads its own name in all three, and the summary still validates. The engine here is `fake-acp`, a profile
//! that is not Cline: its sentence is the profile name (the display table has no entry for it), which shows that the label follows the run's engine and
//! not a constant.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::envelope::EventType;

async fn run(d: &Direct) {
    d.invoke().await;
    d.wait_healthy().await;
}

fn ended_label(d: &Direct) -> String {
    d.aws
        .events()
        .into_iter()
        .find(|(t, _)| *t == EventType::TurnEnded)
        .and_then(|(_, v)| v["label"].as_str().map(str::to_string))
        .expect("a turn.ended event with a label")
}

fn ended_log_line(d: &Direct) -> String {
    d.log
        .lines()
        .iter()
        .find(|l| l.contains("direct turn ended"))
        .cloned()
        .expect("the log line of the end")
}

#[tokio::test(flavor = "multi_thread")]
async fn b788_a_cline_run_writes_the_frozen_cline_sentence_in_summary_event_and_log() {
    let d = Direct::start(Opts::default()).await;
    run(&d).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(s.validate(), Ok(()));
    assert_eq!(ended_label(&d), "Cline reported done");
    assert!(
        ended_log_line(&d).contains("label=\"Cline reported done\""),
        "{}",
        ended_log_line(&d)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b788_a_run_of_another_engine_names_that_engine_in_summary_event_and_log() {
    let d = Direct::start(Opts {
        engine: "fake-acp",
        ..Opts::default()
    })
    .await;
    run(&d).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{s:?}");
    assert_eq!(s.engine.name, "fake-acp");
    assert_eq!(s.label, "fake-acp reported done");
    assert_eq!(
        s.validate(),
        Ok(()),
        "the summary validates against its own engine"
    );
    assert_eq!(ended_label(&d), "fake-acp reported done");
    let line = ended_log_line(&d);
    assert!(line.contains("label=\"fake-acp reported done\""), "{line}");
    assert!(!line.contains("Cline"), "{line}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b788_a_setup_failure_of_another_engine_reads_its_own_name() {
    let d = Direct::start(Opts {
        engine: "fake-acp",
        tamper_request: true,
        ..Opts::default()
    })
    .await;
    run(&d).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    assert_eq!(s.label, "fake-acp did not start");
    assert_eq!(s.validate(), Ok(()));
}
