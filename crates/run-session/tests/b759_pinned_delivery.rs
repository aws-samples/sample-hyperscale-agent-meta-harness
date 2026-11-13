//! `plan-delivery` reads the end-of-run objects of a run by the version records agentd sent, never the object at the key.
//!
//! The run's session credentials are the engine's too, so a process it leaves behind can put a newer version of `bundle`, `summary.json` or
//! `test-report.json` after agentd wrote them. The fake store here keeps every version of a key like a versioned bucket. A run whose Agent item
//! requires the records reads agentd's versions and checks their digests; a run that does not is read as it always was.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::packager::PackageRequest;
use delivery::validator::ValidateRequest;
use harness_protocol::direct::{
    DirectOutcome, TestCommandSource, TestCounts, TestReport, TestReportStatus, TestStep,
    TestStepRole, s3,
};
use harness_protocol::goal::GoalPhase;
use harness_protocol::output_pins::{OutputPin, OutputPins};
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

async fn try_plan(mem: &Mem, session: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(json!({
        "op": "plan-delivery", "runId": RUN, "staged": staged(), "session": session
    }))
    .unwrap();
    handle(&mem.deps(), &op).await
}

async fn plan(mem: &Mem, session: Value) -> Value {
    try_plan(mem, session).await.unwrap()
}

fn seeded(target: RunTarget, required: bool) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    if required {
        mem.put_pinned_agent();
    }
    mem
}

/// A summary that says what agentd saw: the session ran out of time.
fn agentd_summary() -> Vec<u8> {
    serde_json::to_vec(&summary(
        DirectOutcome::StoppedTimeout,
        Some("AGENTD'S OWN MESSAGE"),
        true,
    ))
    .unwrap()
}

/// What a process with the engine's credentials writes afterwards: a summary that claims the work is done.
fn forged_summary() -> Vec<u8> {
    serde_json::to_vec(&summary(
        DirectOutcome::ReportedDone,
        Some("FORGED MESSAGE"),
        true,
    ))
    .unwrap()
}

/// agentd's end of run: the bundle and the summary stored as versions, and the records it sends in `turn-complete`.
fn agentd_ends(mem: &Mem) -> OutputPins {
    OutputPins {
        bundle: Some(mem.agentd_writes(&s3::bundle_key(RUN), b"BUNDLE".to_vec())),
        summary: Some(mem.agentd_writes(&s3::summary_key(RUN), agentd_summary())),
        test_report: None,
    }
}

fn session(pins: Option<&OutputPins>) -> Value {
    let mut turn = turn_output(true);
    if let Some(p) = pins {
        turn["outputPins"] = serde_json::to_value(p).unwrap();
    }
    json!({"turn": turn, "failure": {}})
}

fn validate(p: &Value) -> ValidateRequest {
    serde_json::from_value(p["validate"].clone()).unwrap()
}

fn package(p: &Value) -> PackageRequest {
    serde_json::from_value(p["package"].clone()).unwrap()
}

fn evidence(mem: &Mem) -> Value {
    serde_json::from_slice(&mem.blob(&s3::evidence_key(RUN)).unwrap()).unwrap()
}

// ---- the gap, and its close ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b759_a_run_that_does_not_require_records_reads_the_object_at_the_key() {
    // How every run was read before the records existed: the newest version is the object. A forged summary put after agentd's write is the
    // one delivery reads. This is the gap; the run below closes it.
    let mem = seeded(repo_target(), false);
    let pins = agentd_ends(&mem);
    mem.put_version(&s3::summary_key(RUN), forged_summary());
    let p = plan(&mem, session(Some(&pins))).await;
    assert_eq!(p["outcome"], "reported_done", "{p}");
    assert_eq!(
        package(&p).direct.cline_message.as_deref(),
        Some("FORGED MESSAGE")
    );
    assert!(validate(&p).source_bundle_version_id.is_none());
    assert!(evidence(&mem).get("outputPins").is_none());
}

#[tokio::test]
async fn b759_a_summary_put_after_agentd_wrote_it_does_not_change_what_delivery_reads() {
    let mem = seeded(repo_target(), true);
    let pins = agentd_ends(&mem);
    // The engine's credentials put a newer summary and a newer bundle.
    mem.put_version(&s3::summary_key(RUN), forged_summary());
    mem.put_version(&s3::bundle_key(RUN), b"PLANTED BUNDLE".to_vec());
    let p = plan(&mem, session(Some(&pins))).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["outcome"], "stopped_timeout", "{p}");
    assert_eq!(p["headSource"], "final");
    let d = package(&p).direct;
    assert_eq!(d.cline_message.as_deref(), Some("AGENTD'S OWN MESSAGE"));
    assert_ne!(d.cline_message.as_deref(), Some("FORGED MESSAGE"));
    // Only agentd's versions were read; the current version of no object was.
    let reads = mem.0.version_reads.lock().unwrap().clone();
    let bundle = pins.bundle.as_ref().unwrap().version_id.clone();
    let summary_v = pins.summary.as_ref().unwrap().version_id.clone();
    assert!(reads.contains(&(s3::bundle_key(RUN), bundle)), "{reads:?}");
    assert!(
        reads.contains(&(s3::summary_key(RUN), summary_v)),
        "{reads:?}"
    );
    assert!(reads.iter().all(|(_, v)| v.is_some()), "{reads:?}");
}

#[tokio::test]
async fn b759_the_validator_is_told_the_bundle_version_and_digest_agentd_recorded() {
    let mem = seeded(repo_target(), true);
    let pins = agentd_ends(&mem);
    mem.put_version(&s3::bundle_key(RUN), b"PLANTED BUNDLE".to_vec());
    let p = plan(&mem, session(Some(&pins))).await;
    let v = validate(&p);
    let bundle = pins.bundle.unwrap();
    assert_eq!(v.source_bundle_version_id, bundle.version_id);
    assert_eq!(
        v.source_bundle_sha256.as_deref(),
        Some(bundle.sha256.as_str())
    );
    assert_eq!(
        v.source_bundle_uri.as_deref(),
        Some(format!("s3://{BUCKET}/{}", s3::bundle_key(RUN)).as_str())
    );
}

#[tokio::test]
async fn b759_the_decision_file_records_the_versions_it_read() {
    let mem = seeded(repo_target(), true);
    let pins = agentd_ends(&mem);
    let _ = plan(&mem, session(Some(&pins))).await;
    let e = evidence(&mem);
    assert_eq!(e["outputPins"], serde_json::to_value(&pins).unwrap());
    assert!(e.get("integrityRefusal").is_none(), "{e}");
}

// ---- a record that does not hold ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn b759_a_summary_version_that_is_gone_refuses_the_final_files_and_delivers_nothing() {
    let mem = seeded(repo_target(), true);
    let mut pins = agentd_ends(&mem);
    pins.summary.as_mut().unwrap().version_id = Some("version-that-expired".into());
    let p = plan(&mem, session(Some(&pins))).await;
    assert_eq!(p["deliver"], false, "{p}");
    assert_eq!(p["outcome"], "stopped_other");
    let label = p["label"].as_str().unwrap();
    assert!(
        label.starts_with("Cline stopped: the harness ended the run (")
            && label.contains("summary.json")
            && label.contains("does not match the harness's record"),
        "{label}"
    );
    let reason = p["reason"].as_str().unwrap();
    assert!(
        reason.contains("summary.json") && reason.contains("nothing was delivered"),
        "{reason}"
    );
    assert_eq!(evidence(&mem)["integrityRefusal"], "summary.json");
    assert_ne!(
        mem.run().phase,
        GoalPhase::Delivering,
        "the run never reached the delivering phase"
    );
    // It did not fall back to the object at the key.
    assert!(
        mem.0
            .version_reads
            .lock()
            .unwrap()
            .iter()
            .all(|(_, v)| v.is_some())
    );
}

#[tokio::test]
async fn b759_a_summary_whose_digest_differs_is_not_used() {
    let mem = seeded(repo_target(), true);
    let mut pins = agentd_ends(&mem);
    pins.summary.as_mut().unwrap().sha256 = "0".repeat(64);
    let p = plan(&mem, session(Some(&pins))).await;
    assert_eq!(p["deliver"], false, "{p}");
    assert_eq!(evidence(&mem)["integrityRefusal"], "summary.json");
}

#[tokio::test]
async fn b759_a_bundle_whose_digest_differs_is_refused() {
    let mem = seeded(repo_target(), true);
    let mut pins = agentd_ends(&mem);
    pins.bundle.as_mut().unwrap().sha256 = "f".repeat(64);
    let p = plan(&mem, session(Some(&pins))).await;
    assert_eq!(p["deliver"], false, "{p}");
    assert_eq!(p["outcome"], "stopped_other");
    assert!(p["label"].as_str().unwrap().contains("bundle"), "{p}");
    assert_eq!(evidence(&mem)["integrityRefusal"], "bundle");
}

#[tokio::test]
async fn b759_a_refusal_is_the_same_decision_on_a_retry() {
    let mem = seeded(repo_target(), true);
    let mut pins = agentd_ends(&mem);
    pins.bundle.as_mut().unwrap().version_id = Some("gone".into());
    let first = plan(&mem, session(Some(&pins))).await;
    let again = plan(&mem, session(Some(&pins))).await;
    for k in ["deliver", "outcome", "label", "reason", "plannedAtMs"] {
        assert_eq!(first[k], again[k], "{k}");
    }
    assert_eq!(again["deliver"], false);
}

// ---- no record at all --------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b759_a_required_run_whose_turn_carries_no_records_reads_none_of_the_three_objects() {
    let mem = seeded(repo_target(), true);
    // The objects are at the keys, but nothing authenticated says agentd wrote them.
    mem.put_blob(&s3::bundle_key(RUN), b"BUNDLE".to_vec());
    mem.put_blob(&s3::summary_key(RUN), forged_summary());
    let p = plan(&mem, session(None)).await;
    assert_eq!(p["deliver"], false, "{p}");
    assert_eq!(p["outcome"], "stopped_other");
    assert!(
        p["label"]
            .as_str()
            .unwrap()
            .contains("no record of the files it saved at its end"),
        "{p}"
    );
    assert_ne!(p["outcome"], "reported_done");
}

#[tokio::test]
async fn b759_a_required_run_with_a_malformed_record_is_read_as_having_none() {
    let mem = seeded(repo_target(), true);
    mem.put_blob(&s3::bundle_key(RUN), b"BUNDLE".to_vec());
    mem.put_blob(&s3::summary_key(RUN), forged_summary());
    let mut turn = turn_output(true);
    turn["outputPins"] = json!({"summary": {"sha256": "not hex"}});
    let p = plan(&mem, json!({"turn": turn, "failure": {}})).await;
    assert_eq!(p["deliver"], false, "{p}");
    assert_ne!(p["outcome"], "reported_done");
}

#[tokio::test]
async fn b759_a_session_lost_before_turn_complete_does_not_trust_the_summary_at_the_key() {
    let mem = seeded(repo_target(), true);
    mem.put_blob(&s3::bundle_key(RUN), b"BUNDLE".to_vec());
    mem.put_blob(&s3::summary_key(RUN), forged_summary());
    let lost =
        json!({"turn": {}, "failure": {"Error": "States.HeartbeatTimeout", "Cause": "heartbeat"}});
    let p = plan(&mem, lost.clone()).await;
    assert_eq!(p["deliver"], false, "{p}");
    assert_eq!(p["outcome"], "stopped_session_lost", "{p}");
    // The same session on a run that does not require records is read as before.
    let old = seeded(repo_target(), false);
    old.put_blob(&s3::bundle_key(RUN), b"BUNDLE".to_vec());
    old.put_blob(&s3::summary_key(RUN), forged_summary());
    let p = plan(&old, lost).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["outcome"], "reported_done");
}

#[tokio::test]
async fn b759_a_snapshot_is_still_delivered_when_a_required_run_ended_with_no_final_bundle() {
    // The periodic snapshot is outside the records: a session that ended with no final bundle is still delivered from its last snapshot, and
    // the label and the detail say so.
    let mem = seeded(repo_target(), true);
    mem.put_snapshot(HEAD);
    let lost =
        json!({"turn": {}, "failure": {"Error": "States.HeartbeatTimeout", "Cause": "heartbeat"}});
    let p = plan(&mem, lost).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["headSource"], "snapshot");
}

// ---- the test report --------------------------------------------------------------------------------------------------------

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
        output_tail: "ran\n".into(),
        output_truncated: false,
        logged: None,
    }
}

fn report(steps: Vec<TestStep>) -> Vec<u8> {
    serde_json::to_vec(&TestReport {
        schema_version: 1,
        run_id: RUN.into(),
        head_sha: HEAD.into(),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: Some("2026-10-16T21:00:00Z".into()),
        duration_ms: 1_200,
        steps,
        summary: None,
    })
    .unwrap()
}

#[tokio::test]
async fn b759_the_test_report_is_read_by_its_record_and_a_planted_one_is_not() {
    let mem = seeded(repo_target(), true);
    let mut pins = agentd_ends(&mem);
    pins.test_report =
        Some(mem.agentd_writes(&s3::test_report_key(RUN), report(vec![step(0, (12, 0, 0))])));
    mem.put_version(&s3::test_report_key(RUN), report(vec![step(1, (0, 9, 0))]));
    let p = plan(&mem, session(Some(&pins))).await;
    let s = package(&p)
        .direct
        .test_report
        .expect("agentd's report is summarised");
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 12 of 12")
    );
    let copy: TestReport =
        serde_json::from_slice(&mem.blob(&s3::evidence_test_report_key(RUN)).unwrap()).unwrap();
    assert_eq!(copy.steps[0].exit_code, Some(0));
}

#[tokio::test]
async fn b759_a_test_report_that_does_not_match_its_record_is_left_out_and_changes_nothing_else() {
    let mem = seeded(repo_target(), true);
    let mut pins = agentd_ends(&mem);
    let mut bad = mem.agentd_writes(&s3::test_report_key(RUN), report(vec![step(0, (12, 0, 0))]));
    bad.sha256 = "e".repeat(64);
    pins.test_report = Some(bad);
    let p = plan(&mem, session(Some(&pins))).await;
    assert_eq!(
        p["deliver"], true,
        "the report never changes the delivery: {p}"
    );
    assert_eq!(p["outcome"], "stopped_timeout");
    assert!(package(&p).direct.test_report.is_none());
    assert!(
        p["reportNote"]
            .as_str()
            .unwrap()
            .contains("did not match the harness's record"),
        "{p}"
    );
    assert!(mem.blob(&s3::evidence_test_report_key(RUN)).is_none());
}

#[tokio::test]
async fn b759_a_required_run_with_no_report_record_reads_no_report_even_when_one_is_at_the_key() {
    let mem = seeded(repo_target(), true);
    let pins = agentd_ends(&mem);
    mem.put_version(&s3::test_report_key(RUN), report(vec![step(0, (12, 0, 0))]));
    let p = plan(&mem, session(Some(&pins))).await;
    assert!(package(&p).direct.test_report.is_none());
    assert!(p.get("reportNote").is_none(), "{p}");
}

// ---- the Agent item `start-session` writes -----------------------------------------------------------------------------------

#[tokio::test]
async fn b759_start_session_writes_an_agent_item_that_requires_the_version_records() {
    let mem = seeded(repo_target(), false);
    mem.keep_pins_flag();
    let op: Op = serde_json::from_value(json!({
        "op": "start-session", "runId": RUN, "taskToken": "token-A", "staged": staged()
    }))
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap();
    let creds = mem.agent().unwrap().creds.unwrap();
    assert!(creds.output_pins_required(), "{creds:?}");
    assert!(
        creds.completion_key_sha256.is_some(),
        "the records ride the call that proves the run's key"
    );
    // The item the production code wrote is what the delivery step reads: with the records it delivers, and a forged newer summary does not
    // change what it reads.
    let pins = agentd_ends(&mem);
    mem.put_version(&s3::summary_key(RUN), forged_summary());
    let p = plan(&mem, session(Some(&pins))).await;
    assert_eq!(p["outcome"], "stopped_timeout", "{p}");
}

// ---- a store that cannot read a version -------------------------------------------------------------------------------------

struct NoVersions;

impl run_session::ports::Blobs for NoVersions {
    async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>, RunSessionError> {
        panic!("the object at the key must not be read")
    }
    async fn exists(&self, _key: &str) -> Result<bool, RunSessionError> {
        panic!("the object at the key must not be looked for")
    }
    async fn put_if_absent(&self, _key: &str, _body: &[u8]) -> Result<bool, RunSessionError> {
        Ok(true)
    }
}

#[tokio::test]
async fn b759_a_store_that_cannot_read_a_version_is_unavailable_and_never_answers_with_the_current_object()
 {
    let pin = OutputPin::for_body(b"x", Some("v1".into()));
    let e = run_session::pinned::read(&NoVersions, "k", Some(&pin), "summary.json", 10)
        .await
        .unwrap_err();
    assert!(matches!(e, RunSessionError::Unavailable(_)), "{e:?}");
    let e = run_session::pinned::check(&NoVersions, "k", Some(&pin), "bundle")
        .await
        .unwrap_err();
    assert!(matches!(e, RunSessionError::Unavailable(_)), "{e:?}");
}

#[tokio::test]
async fn b759_a_record_with_no_version_id_holds_the_current_object_to_the_digest() {
    let mem = seeded(repo_target(), true);
    mem.put_blob(&s3::summary_key(RUN), agentd_summary());
    let pin = OutputPin::for_body(&agentd_summary(), None);
    let found = run_session::pinned::read(
        &mem,
        &s3::summary_key(RUN),
        Some(&pin),
        "summary.json",
        1 << 20,
    )
    .await
    .unwrap();
    assert!(matches!(found, run_session::pinned::Read::Found(_)));
    // Someone writes over it: the digest no longer holds.
    mem.put_blob(&s3::summary_key(RUN), forged_summary());
    let found = run_session::pinned::read(
        &mem,
        &s3::summary_key(RUN),
        Some(&pin),
        "summary.json",
        1 << 20,
    )
    .await
    .unwrap();
    assert_eq!(found, run_session::pinned::Read::Refused("summary.json"));
}
