//! `plan-delivery` and the clean-checkout test report: the worker's `test-report.json` is read, checked against the run and
//! the delivered head, copied into the evidence and summarised for EVIDENCE.md and the pull request. Whatever it holds, the label, the
//! outcome and the decision to deliver are what they were without it.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::gateway::GatewayRequest;
use delivery::packager::PackageRequest;
use harness_protocol::direct::{
    DirectOutcome, TestCommandSource, TestCounts, TestReport, TestReportStatus, TestStep,
    TestStepRole, banned_claim_word, s3,
};
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

async fn plan(mem: &Mem) -> Value {
    let op: Op = serde_json::from_value(json!({
        "op": "plan-delivery", "runId": RUN, "staged": staged(),
        "session": {"turn": turn_output(true), "failure": {}}
    }))
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap()
}

fn seeded(target: RunTarget) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct(a, b)."),
        true,
    ));
    mem
}

fn step(exit: i32, counts: (u32, u32, u32)) -> TestStep {
    TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::TestCommandFile,
        command: "python3 -m pytest -q".into(),
        exit_code: Some(exit),
        timed_out: false,
        duration_ms: 900,
        limit_s: None,
        counts: Some(TestCounts {
            passed: counts.0,
            failed: counts.1,
            skipped: counts.2,
        }),
        counts_from: Some("pytest".into()),
        output_tail: "12 passed in 0.31s\n".into(),
        output_truncated: false,
        logged: None,
    }
}

fn report(head: &str, steps: Vec<TestStep>) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: RUN.into(),
        head_sha: head.into(),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: Some("2026-10-16T21:00:00Z".into()),
        duration_ms: 1_200,
        steps,
        summary: None,
    }
}

fn put_report(mem: &Mem, r: &TestReport) {
    mem.put_blob(&s3::test_report_key(RUN), serde_json::to_vec(r).unwrap());
}

fn package(p: &Value) -> PackageRequest {
    serde_json::from_value(p["package"].clone()).unwrap()
}

fn evidence_copy(mem: &Mem) -> Option<TestReport> {
    mem.blob(&s3::evidence_test_report_key(RUN))
        .map(|b| serde_json::from_slice(&b).unwrap())
}

#[tokio::test]
async fn a_valid_report_is_summarised_for_the_package_and_copied_into_the_evidence() {
    let mem = seeded(repo_target());
    put_report(&mem, &report(HEAD, vec![step(0, (12, 0, 0))]));
    let p = plan(&mem).await;
    assert_eq!(p["deliver"], true);
    let d = package(&p).direct;
    let s = d
        .test_report
        .expect("the summary rides in the package request");
    assert_eq!(s.status, TestReportStatus::Ran);
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 12 of 12")
    );
    let copy = evidence_copy(&mem).expect("the zip's evidence/ gets test-report.json");
    assert_eq!(copy.head_sha, HEAD);
    assert_eq!(
        copy.summary,
        Some(s),
        "the copy carries the summary run-session computed"
    );
    assert_eq!(copy.steps[0].exit_code, Some(0));
}

#[tokio::test]
async fn the_report_never_changes_the_label_the_outcome_or_the_decision_to_deliver() {
    let with = seeded(repo_target());
    put_report(&with, &report(HEAD, vec![step(1, (3, 9, 0))]));
    let without = seeded(repo_target());
    let (a, b) = (plan(&with).await, plan(&without).await);
    for key in [
        "deliver",
        "outcome",
        "label",
        "headSha",
        "headSource",
        "validate",
    ] {
        assert_eq!(a[key], b[key], "{key}");
    }
    let (da, db) = (package(&a).direct, package(&b).direct);
    assert_eq!(da.label, db.label);
    assert_eq!(da.outcome, db.outcome);
    assert_eq!(da.cline_message, db.cline_message);
    assert!(da.test_report.is_some() && db.test_report.is_none());
    assert_eq!(
        da.test_report.unwrap().claim(),
        None,
        "a red report makes no claim"
    );
}

#[tokio::test]
async fn without_a_report_the_package_and_the_pull_request_are_what_they_were() {
    let mem = seeded(repo_target());
    let p = plan(&mem).await;
    assert!(package(&p).direct.test_report.is_none());
    let GatewayRequest::PushPr(pr) = serde_json::from_value(p["pushPr"].clone()).unwrap() else {
        panic!("push-pr expected");
    };
    assert!(
        pr.description
            .ends_with("The zip carries what Cline reported (EVIDENCE.md)."),
        "{}",
        pr.description
    );
    assert!(pr.comments.is_empty());
    assert!(evidence_copy(&mem).is_none());
}

#[tokio::test]
async fn the_pull_request_gets_a_comment_with_the_one_sentence_and_no_banned_word() {
    let mem = seeded(repo_target());
    put_report(&mem, &report(HEAD, vec![step(0, (12, 0, 0))]));
    let p = plan(&mem).await;
    let GatewayRequest::PushPr(pr) = serde_json::from_value(p["pushPr"].clone()).unwrap() else {
        panic!("push-pr expected");
    };
    assert_eq!(pr.comments.len(), 1, "{:?}", pr.comments);
    assert!(
        pr.comments[0].contains("repository tests pass in a clean checkout: 12 of 12."),
        "{}",
        pr.comments[0]
    );
    assert!(
        pr.comments[0].contains("does not say the tests are meaningful"),
        "{}",
        pr.comments[0]
    );
    assert_eq!(banned_claim_word(&pr.comments[0]), None);
    assert_eq!(banned_claim_word(&pr.description), None);
    assert!(
        !pr.description
            .contains("Nothing in this pull request was checked by the harness."),
        "the description must not say nothing was run when the report ran the repository's commands: {}",
        pr.description
    );
}

#[tokio::test]
async fn a_run_with_no_pull_request_still_gets_the_summary() {
    let mem = seeded(RunTarget::S3Only);
    put_report(&mem, &report(HEAD, vec![step(0, (5, 0, 0))]));
    let p = plan(&mem).await;
    assert!(p.get("pushPr").is_none(), "{p}");
    assert!(package(&p).direct.test_report.is_some());
}

#[tokio::test]
async fn a_report_about_another_head_or_run_or_one_that_breaks_a_bound_is_ignored() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "another head",
            serde_json::to_vec(&report(&"3".repeat(40), vec![step(0, (1, 0, 0))])).unwrap(),
        ),
        ("another run", {
            let mut r = report(HEAD, vec![step(0, (1, 0, 0))]);
            r.run_id = "01M4XOTHERRUN000000000001".into();
            serde_json::to_vec(&r).unwrap()
        }),
        ("not json", b"{not json".to_vec()),
        (
            "too many steps",
            serde_json::to_vec(&report(
                HEAD,
                (0..=harness_protocol::direct::MAX_REPORT_STEPS)
                    .map(|_| step(0, (1, 0, 0)))
                    .collect(),
            ))
            .unwrap(),
        ),
        ("the off status", {
            let mut r = report(HEAD, vec![step(0, (1, 0, 0))]);
            r.status = TestReportStatus::Off;
            serde_json::to_vec(&r).unwrap()
        }),
        (
            "too large",
            vec![b' '; harness_protocol::direct::MAX_REPORT_BYTES + 1],
        ),
    ];
    for (why, bytes) in cases {
        let mem = seeded(repo_target());
        mem.put_blob(&s3::test_report_key(RUN), bytes);
        let p = plan(&mem).await;
        assert_eq!(p["deliver"], true, "{why}");
        assert_eq!(p["label"], "Cline reported done", "{why}");
        assert!(package(&p).direct.test_report.is_none(), "{why}");
        assert!(
            evidence_copy(&mem).is_none(),
            "{why}: nothing is copied into the evidence"
        );
    }
}

#[tokio::test]
async fn a_summary_the_worker_wrote_is_not_trusted() {
    let mem = seeded(repo_target());
    let mut r = report(HEAD, vec![step(1, (3, 9, 0))]);
    r.summary = Some(report(HEAD, vec![step(0, (99, 0, 0))]).summary());
    put_report(&mem, &r);
    let p = plan(&mem).await;
    let s = package(&p).direct.test_report.unwrap();
    assert_eq!(s.claim(), None);
    assert_eq!(s.counts.unwrap().passed, 3);
    assert_eq!(evidence_copy(&mem).unwrap().summary, Some(s));
}

#[tokio::test]
async fn an_oversized_tail_is_cut_before_the_copy_is_made() {
    // The worker's own bounds hold; this pins that the copy is the bounded form (the control characters are gone).
    let mem = seeded(repo_target());
    let mut r = report(HEAD, vec![step(0, (12, 0, 0))]);
    r.steps[0].output_tail = "ok\n".repeat(100);
    put_report(&mem, &r);
    plan(&mem).await;
    let copy = evidence_copy(&mem).unwrap();
    assert!(copy.steps[0].output_tail.len() <= 4096);
}

#[tokio::test]
async fn a_request_that_switched_the_report_off_gets_an_off_summary_and_no_copy() {
    let mem = Mem::default();
    mem.seed_with_members(
        repo_target(),
        "Add percent_change(a, b) with tests",
        MODEL,
        json!({"reportTests": false}),
    );
    mem.put_bundle();
    mem.put_summary(&summary(DirectOutcome::ReportedDone, Some("m"), true));
    put_report(&mem, &report(HEAD, vec![step(0, (12, 0, 0))]));
    let p = plan(&mem).await;
    let s = package(&p).direct.test_report.unwrap();
    assert_eq!(s.status, TestReportStatus::Off);
    assert_eq!(s.claim(), None);
    assert!(evidence_copy(&mem).is_none());
    let GatewayRequest::PushPr(pr) = serde_json::from_value(p["pushPr"].clone()).unwrap() else {
        panic!("push-pr expected");
    };
    assert!(
        pr.comments.is_empty(),
        "a report that was switched off is not announced in the pull request"
    );
}

#[tokio::test]
async fn a_failed_copy_loses_the_summary_and_nothing_else() {
    let mem = seeded(repo_target());
    put_report(&mem, &report(HEAD, vec![step(0, (12, 0, 0))]));
    // The first put_if_absent is the decision file; the second is the copy.
    mem.pass("put_if_absent");
    mem.fail(
        "put_if_absent",
        RunSessionError::Unavailable("blob store".into()),
    );
    let p = plan(&mem).await;
    assert_eq!(p["deliver"], true);
    assert!(package(&p).direct.test_report.is_none());
    assert!(evidence_copy(&mem).is_none());
}

#[tokio::test]
async fn a_second_plan_gives_the_same_summary_and_leaves_the_first_copy_alone() {
    let mem = seeded(repo_target());
    put_report(&mem, &report(HEAD, vec![step(0, (12, 0, 0))]));
    let first = plan(&mem).await;
    let copy = mem.blob(&s3::evidence_test_report_key(RUN)).unwrap();
    let second = plan(&mem).await;
    assert_eq!(
        package(&first).direct.test_report,
        package(&second).direct.test_report
    );
    assert_eq!(mem.blob(&s3::evidence_test_report_key(RUN)).unwrap(), copy);
}

// ---- a report that cannot be stored or read is said in the run's events, not only in a log --------------
//
// In a deployed stage the run-session role once held no grant on `evidence/test-report.json`: the copy was never made, the summary was
// dropped without a word, and the zip lacked the file EVIDENCE.md names. The in-memory store always accepted the write, so no test saw
// it. These tests refuse the write (and the read) as S3 does for a role without the grant.

const COPY_NOTE: &str = "the test report could not be saved to the run's evidence";
const READ_NOTE: &str = "the test report could not be read from storage";

async fn start_session(mem: &Mem) {
    let op: Op = serde_json::from_value(json!({
        "op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()
    }))
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap();
}

async fn finalize_plan(mem: &Mem, plan: &Value) {
    let op: Op = serde_json::from_value(json!({
        "op": "finalize", "runId": RUN, "plan": plan, "validated": {"pass": true},
        "packaged": {"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096},
        "prLink": {}, "prFailure": {}
    }))
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap();
}

fn completed(mem: &Mem) -> Value {
    mem.events()
        .into_iter()
        .find(|(t, _)| t == "run.completed")
        .expect("run.completed")
        .1
}

/// The role may not write the evidence copy: the first create-only write of a plan (the decision file) passes, the second (the copy) is refused.
fn refuse_the_copy(mem: &Mem) {
    mem.pass("put_if_absent");
    mem.fail(
        "put_if_absent",
        RunSessionError::Unavailable("AccessDenied: the role may not write that key".into()),
    );
}

#[tokio::test]
async fn a_copy_the_role_may_not_write_still_delivers_and_the_plan_says_so() {
    let mem = seeded(repo_target());
    put_report(&mem, &report(HEAD, vec![step(0, (12, 0, 0))]));
    refuse_the_copy(&mem);
    let p = plan(&mem).await;
    // TRIGGER PRESENT: the copy was refused, so the zip has no evidence/test-report.json.
    assert!(evidence_copy(&mem).is_none());
    // Delivery and the label are the ones a working store gives; the summary stays out, as it does for any copy that is not made.
    assert_eq!(p["deliver"], true);
    assert_eq!(p["label"], "Cline reported done");
    // The plan carries the fault in plain words, without the store's own error text.
    assert_eq!(p["reportNote"], COPY_NOTE, "{p}");
    assert!(!p["reportNote"].as_str().unwrap().contains("AccessDenied"));
}

#[tokio::test]
async fn a_copy_that_could_not_be_saved_is_in_the_run_completed_event() {
    let mem = seeded(repo_target());
    put_report(&mem, &report(HEAD, vec![step(0, (12, 0, 0))]));
    start_session(&mem).await;
    mem.clear_events();
    refuse_the_copy(&mem);
    let p = plan(&mem).await;
    finalize_plan(&mem, &p).await;
    let c = completed(&mem);
    let detail = c["detail"]
        .as_str()
        .expect("run.completed carries a detail");
    assert!(detail.contains(COPY_NOTE), "{detail}");
    // The note is beside the outcome: the outcome and the label do not move.
    assert_eq!(c["outcome"], "reported_done");
    assert_eq!(c["label"], "Cline reported done");
    assert_eq!(banned_claim_word(detail), None, "{detail}");
}

#[tokio::test]
async fn a_worker_report_the_role_may_not_read_is_said_and_never_taken_for_no_report() {
    let mem = seeded(repo_target());
    put_report(&mem, &report(HEAD, vec![step(0, (12, 0, 0))]));
    start_session(&mem).await;
    mem.clear_events();
    mem.deny_read(&s3::test_report_key(RUN));
    let p = plan(&mem).await;
    assert_eq!(p["deliver"], true);
    assert_eq!(p["label"], "Cline reported done");
    assert_eq!(p["reportNote"], READ_NOTE, "{p}");
    assert!(package(&p).direct.test_report.is_none());
    finalize_plan(&mem, &p).await;
    let c = completed(&mem);
    assert!(c["detail"].as_str().unwrap().contains(READ_NOTE), "{c}");
}

#[tokio::test]
async fn a_working_store_adds_no_note_and_an_absent_report_is_not_a_fault() {
    let mem = seeded(repo_target());
    put_report(&mem, &report(HEAD, vec![step(0, (12, 0, 0))]));
    let p = plan(&mem).await;
    assert!(p.get("reportNote").is_none(), "{p}");
    // No object at all is the old behaviour (the report switched off in the image, or the worker died first): not a fault.
    let none = plan(&seeded(repo_target())).await;
    assert!(none.get("reportNote").is_none(), "{none}");
    // A report about another head is ignored, and that is not a storage fault either.
    let other = seeded(repo_target());
    put_report(&other, &report(&"9".repeat(40), vec![step(0, (12, 0, 0))]));
    let p = plan(&other).await;
    assert!(p.get("reportNote").is_none(), "{p}");
}

// ---- a report agentd skipped for lack of time before the run deadline is said in the run's detail -----------

const SKIPPED: &str = "test report skipped: not enough time before the run deadline";

#[tokio::test]
async fn a_report_skipped_for_lack_of_time_is_in_the_run_completed_detail() {
    let mem = seeded(repo_target());
    let skipped =
        TestReport::without_steps(RUN, HEAD, TestReportStatus::NotRun, Some(SKIPPED.into()));
    put_report(&mem, &skipped);
    start_session(&mem).await;
    mem.clear_events();
    let p = plan(&mem).await;
    assert_eq!(p["deliver"], true);
    assert_eq!(p["label"], "Cline reported done");
    assert_eq!(p["reportNote"], SKIPPED, "{p}");
    finalize_plan(&mem, &p).await;
    let c = completed(&mem);
    assert!(c["detail"].as_str().unwrap().contains(SKIPPED), "{c}");
    assert_eq!(c["outcome"], "reported_done");
    // Any other report that did not run keeps its own reason in the report and adds nothing to the run's detail.
    let other = seeded(repo_target());
    put_report(
        &other,
        &TestReport::without_steps(
            RUN,
            HEAD,
            TestReportStatus::NotRun,
            Some("the clean checkout failed".into()),
        ),
    );
    let q = plan(&other).await;
    assert!(q.get("reportNote").is_none(), "{q}");
}
