//! The words run-session writes name the engine that ran. The pull request, the evidence, `run.completed`, the run index and the stop reasons of a
//! Cline run are the words they were, byte for byte; for an OpenCode run each one says OpenCode and none says Cline.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::common::Engine;
use harness_protocol::direct::{DirectOutcome, DirectSummary, banned_claim_word};
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::events;
use run_session::ops::{Op, handle};
use run_session::text::{self, PrFacts};
use serde_json::{Value, json};
use support::fakes::*;

fn facts(engine: Engine, message: Option<&str>) -> PrFacts<'_> {
    PrFacts {
        run_id: RUN,
        engine,
        outcome: DirectOutcome::ReportedDone,
        model_id: MODEL,
        wall_s: Some(3_731),
        message,
        detail: None,
        from_snapshot: false,
        snapshot_at: None,
        continues: None,
    }
}

fn long_message() -> String {
    "word ".repeat(4_000)
}

#[test]
fn b788_a_description_differs_between_two_engines_only_in_the_name() {
    let cline = text::description(&facts(Engine::CLINE, Some("Added pct.\nTwo tests.")));
    let open = text::description(&facts(Engine::OPENCODE, Some("Added pct.\nTwo tests.")));
    assert_ne!(cline, open);
    assert_eq!(open.replace("OpenCode", "Cline"), cline);
    assert!(open.starts_with("OpenCode reported done\n"), "{open}");
    assert!(open.contains("OpenCode's last message:\n"), "{open}");
    assert!(
        open.contains("The zip carries what OpenCode reported (EVIDENCE.md)."),
        "{open}"
    );
    assert!(!open.contains("Cline"), "{open}");
}

#[test]
fn b788_the_lines_without_a_message_and_for_a_snapshot_name_the_engine() {
    let mut f = facts(Engine::OPENCODE, None);
    f.from_snapshot = true;
    f.snapshot_at = Some("2026-11-08 06:00 UTC");
    let d = text::description(&f);
    assert!(d.contains("OpenCode left no final message."), "{d}");
    assert!(d.contains(", not OpenCode's final result."), "{d}");
    assert!(!d.contains("Cline"), "{d}");
    let with_report = text::description_with(&facts(Engine::OPENCODE, Some("m")), true);
    assert!(
        with_report
            .contains("The zip carries what OpenCode reported and the clean-checkout test report"),
        "{with_report}"
    );
    assert!(!with_report.contains("Cline"), "{with_report}");
}

#[test]
fn b788_the_comments_that_hold_a_long_message_are_headed_with_the_engine() {
    let m = long_message();
    let open = text::comments_for(Engine::OPENCODE, Some(&m), Some("it passed"));
    assert!(
        open[0].starts_with("OpenCode's last message, whole"),
        "{}",
        &open[0][..60]
    );
    assert!(open.iter().all(|c| !c.contains("Cline")));
    // For Cline the same call is what `comments_with_report` always gave.
    assert_eq!(
        text::comments_for(Engine::CLINE, Some(&m), Some("it passed")),
        text::comments_with_report(Some(&m), Some("it passed"))
    );
    assert!(
        text::comments_with_report(Some(&m), None)[0].starts_with("Cline's last message, whole")
    );
}

#[test]
fn b788_run_completed_and_run_blocked_carry_the_label_of_the_engine() {
    use harness_protocol::goal::GoalStatus;
    let run = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
    let label_of = |e: harness_protocol::envelope::Envelope| match e.payload {
        harness_protocol::envelope::Payload::Inline { value } => value["label"].clone(),
        _ => panic!("an inline payload"),
    };
    for (engine, want) in [
        (Engine::CLINE, "Cline stopped: the session was lost"),
        (Engine::OPENCODE, "OpenCode stopped: the session was lost"),
    ] {
        let done = events::run_completed_as(
            run,
            1,
            GoalStatus::Stalled,
            engine,
            DirectOutcome::StoppedSessionLost,
            None,
            vec![],
        );
        assert_eq!(label_of(done), json!(want));
        let blocked = events::run_blocked(
            run,
            1,
            engine,
            Some(DirectOutcome::StoppedSessionLost),
            None,
        );
        assert_eq!(label_of(blocked), json!(want));
    }
}

fn opencode_summary(outcome: DirectOutcome, message: Option<&str>) -> DirectSummary {
    let mut s = summary(outcome, message, true);
    s.engine.name = "opencode".into();
    s.label = outcome.label_for_engine(Engine::OPENCODE, s.detail.as_deref());
    s
}

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn plan(mem: &Mem, session: Value) -> Value {
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": session}),
    )
    .await
    .unwrap()
}

fn done_session() -> Value {
    json!({"turn": turn_output(true), "failure": {}})
}

fn opencode_world() -> Mem {
    let mem = Mem::default();
    mem.seed_with_members(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
        json!({"engine": "opencode"}),
    );
    mem.set_run_engine(Engine::OPENCODE);
    mem.put_bundle();
    mem
}

#[tokio::test]
async fn b788_the_plan_of_an_opencode_run_carries_its_label_its_evidence_and_its_package_text() {
    let mem = opencode_world();
    mem.put_summary(&opencode_summary(
        DirectOutcome::ReportedDone,
        Some("Added pct."),
    ));
    let p = plan(&mem, done_session()).await;
    assert_eq!(p["label"], "OpenCode reported done");
    assert_eq!(p["package"]["direct"]["label"], "OpenCode reported done");
    assert_eq!(p["package"]["direct"]["engine"], "opencode");
    let text = p.to_string();
    assert!(!text.contains("Cline"), "{text}");
    let evidence: Value = serde_json::from_slice(
        &mem.blob(&harness_protocol::direct::s3::evidence_key(RUN))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(evidence["label"], "OpenCode reported done");
}

#[tokio::test]
async fn b788_the_plan_of_a_cline_run_has_no_engine_in_its_evidence() {
    let mem = Mem::default();
    mem.seed(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
    );
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct."),
        true,
    ));
    let p = plan(&mem, done_session()).await;
    assert_eq!(p["label"], "Cline reported done");
    assert!(
        p["package"]["direct"].get("engine").is_none(),
        "{}",
        p["package"]["direct"]
    );
}

#[tokio::test]
async fn b788_a_lost_session_of_an_opencode_run_reads_its_own_name_with_no_summary() {
    let mem = opencode_world();
    let lost =
        json!({"turn": {}, "failure": {"Error": "States.HeartbeatTimeout", "Cause": "heartbeat"}});
    let p = plan(&mem, lost).await;
    assert_eq!(p["outcome"], "stopped_session_lost");
    assert_eq!(p["label"], "OpenCode stopped: the session was lost");
    assert!(!p.to_string().contains("Cline"), "{p}");
}

#[tokio::test]
async fn b788_finalize_writes_the_run_completed_label_and_the_stop_reason_of_the_engine() {
    for (outcome, want_status) in [
        (DirectOutcome::ReportedDone, "complete"),
        (DirectOutcome::StoppedSilence, "stalled"),
    ] {
        let mem = opencode_world();
        mem.put_summary(&opencode_summary(outcome, Some("Added pct.")));
        let p = plan(&mem, done_session()).await;
        let packaged = json!({"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096});
        call(
            &mem,
            json!({"op": "finalize", "runId": RUN, "plan": p, "validated": {"pass": true}, "packaged": packaged, "prLink": {}, "prFailure": {}}),
        )
        .await
        .unwrap();
        let done = mem
            .events()
            .into_iter()
            .find(|(t, _)| t == "run.completed")
            .unwrap()
            .1;
        assert_eq!(done["status"], want_status);
        let label = done["label"].as_str().unwrap();
        assert!(label.starts_with("OpenCode "), "{label}");
        assert_eq!(banned_claim_word(label), None);
        if outcome == DirectOutcome::StoppedSilence {
            let reason = mem.run().status_reason.unwrap();
            assert_eq!(
                reason,
                "silence: OpenCode sent no output for the silence limit"
            );
        }
    }
}

#[tokio::test]
async fn b788_a_blocked_opencode_run_writes_its_label() {
    let mem = opencode_world();
    let state = json!({"plan": {"result": {"outcome": "stopped_other", "detail": "x"}}, "failure": {"Error": "States.TaskFailed", "Cause": "boom"}});
    call(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": state}),
    )
    .await
    .unwrap();
    let done = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "run.completed")
        .unwrap()
        .1;
    assert_eq!(
        done["label"],
        "OpenCode stopped: the engine reported an unknown stop reason"
    );
}
