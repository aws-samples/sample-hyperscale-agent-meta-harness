//! A run whose target is a zip delivers its zip.
//!
//! - `plan-delivery` tells the bundle validator the target: `"target": "zip"` for a run with the zip target, no member for a repository
//!   run (the validator reads no member as a pull request target, which is also what an earlier release sends).
//! - `finalize` puts one sentence in the detail of `run.completed` when the validator reports that the zip holds a workflow file or a
//!   build specification; the label does not move.
//! - `plan-delivery` picks the base the validator judges from: a follow-up of a run whose head never passed the check (it ended blocked
//!   or was cleared) is judged from the base that run started from, and a repository follow-up of a zip run from the zip run's base, so
//!   no run delivers under one target what an earlier run was refused for, or was only allowed as a zip.
//! - `plan-delivery` names the runs whose commits are judged with this run's (`sourceRunIds`), so the validator reads the items they
//!   stored for the copy rule.
//! - `mark-blocked` ends the reason of a run blocked for a protected path or a secret with the way to get the work back that exists: the
//!   follow-up that delivers a workflow file or a build specification in a zip, and the snapshot command only when the run saved a
//!   snapshot, with the time it was saved.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::validator::ValidateRequest;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{ContinueFrom, DirectOutcome, HARNESS_STOP_PREFIX, s3};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::RunItem;
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

const WORKFLOW: &str = ".github/workflows/ci.yml";

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn plan_staged(mem: &Mem, staged: Value) -> Value {
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged,
            "session": {"turn": turn_output(true), "failure": {}}}),
    )
    .await
    .unwrap()
}

async fn plan(mem: &Mem) -> Value {
    plan_staged(mem, staged()).await
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

fn validate(p: &Value) -> ValidateRequest {
    serde_json::from_value(p["validate"].clone()).unwrap()
}

// ---- the target reaches the validator ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn b821_plan_delivery_tells_the_validator_that_the_target_is_a_zip() {
    let mem = seeded(RunTarget::S3Only);
    let p = plan(&mem).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["validate"]["target"], "zip", "{p}");
    // The validator of this build reads exactly this payload, and the member is the one it writes for a zip.
    let r = validate(&p);
    assert_eq!(serde_json::to_value(&r).unwrap()["target"], "zip");
    // Nothing else of the payload moved.
    assert_eq!(p["validate"]["engineConfig"], "executable-only");
    assert_eq!(p["validate"]["allowedGlobs"], json!(["**", "!.git/**"]));
    assert_eq!(p["validate"]["baseSha"], BASE);
    assert!(p.get("pushPr").is_none(), "{p}");
}

#[tokio::test]
async fn b821_a_repository_run_sends_no_target_member() {
    let new_repo: RunTarget =
        serde_json::from_value(json!({"mode": "new-repo", "repo": "cc:hamh-e2e-new-20261111"}))
            .unwrap();
    for target in [repo_target(), new_repo] {
        let mem = seeded(target.clone());
        let p = plan(&mem).await;
        assert_eq!(p["deliver"], true, "{p}");
        assert!(
            p["validate"].get("target").is_none(),
            "{target:?}: a repository run keeps the payload it had, and the validator reads no member as a pull request: {p}"
        );
        let r = validate(&p);
        assert!(
            serde_json::to_value(&r).unwrap().get("target").is_none(),
            "{target:?}"
        );
    }
}

// ---- finalize: the notice ---------------------------------------------------------------------------------------------------------

async fn planned_and_started(mem: &Mem) -> Value {
    call(
        mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem.clear_events();
    plan(mem).await
}

fn packaged() -> Value {
    json!({"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096})
}

async fn finalize(mem: &Mem, plan: &Value, validated: Value) -> Value {
    call(
        mem,
        json!({"op": "finalize", "runId": RUN, "plan": plan, "validated": validated,
            "packaged": packaged(), "prLink": {}, "prFailure": {}}),
    )
    .await
    .unwrap()
}

fn completed(mem: &Mem) -> Value {
    mem.events()
        .into_iter()
        .find(|(t, _)| t == "run.completed")
        .unwrap()
        .1
}

fn finding(rule: &str, path: &str) -> Value {
    json!({"rule": rule, "path": path, "detail": format!("{path} is reported; the zip holds it")})
}

const WORKFLOW_NOTICE: &str = "The zip holds a workflow file (.github/workflows/ci.yml); it runs only if the code is pushed to a git host that runs such files.";

#[tokio::test]
async fn b821_a_zip_run_that_adds_a_workflow_file_ends_delivered_with_the_notice() {
    let mem = seeded(RunTarget::S3Only);
    let p = planned_and_started(&mem).await;
    let validated = json!({"pass": true, "runId": RUN, "headSha": HEAD, "changedFiles": 4, "violations": [],
        "findings": [finding("workflow-file", WORKFLOW)]});
    let done = finalize(&mem, &p, validated).await;
    assert_eq!(done["status"], "complete");
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert!(
        mem.event_types().contains(&"delivery.ready".to_string()),
        "the zip is delivered: {:?}",
        mem.event_types()
    );
    let c = completed(&mem);
    assert_eq!(c["label"], "Cline reported done", "{c}");
    assert_eq!(c["detail"], WORKFLOW_NOTICE, "{c}");
}

#[tokio::test]
async fn b821_the_notice_names_the_build_specification_in_its_own_words_and_counts_further_files() {
    let mem = seeded(RunTarget::S3Only);
    let p = planned_and_started(&mem).await;
    let validated = json!({"pass": true, "violations": [], "findings": [
        finding("workflow-file", WORKFLOW),
        finding("workflow-file", ".github/workflows/release.yml"),
        finding("workflow-file", ".github/workflows/nightly.yml"),
        finding("buildspec", "buildspec.yml"),
        // A finding of another rule (a third-party copy the user allowed) is not this notice's.
        {"rule": "third-party-copy", "path": "vendor/x.js", "detail": "equals a stored file"},
    ]});
    finalize(&mem, &p, validated).await;
    let c = completed(&mem);
    assert_eq!(
        c["detail"],
        "The zip holds 3 workflow files (.github/workflows/ci.yml and 2 more); they run only if the code is pushed to a git host that runs such files. The zip holds a build specification (buildspec.yml); a build service runs it only if a build project builds this code.",
        "{c}"
    );
}

#[tokio::test]
async fn b821_a_delivery_without_such_a_finding_keeps_its_detail() {
    for validated in [
        json!({"pass": true}),
        json!({"pass": true, "violations": [], "findings": []}),
        json!({"pass": true, "findings": [{"rule": "third-party-copy", "path": "vendor/x.js", "detail": "d"}]}),
        // An answer whose findings are not a list is read as none.
        json!({"pass": true, "findings": "workflow-file"}),
        Value::Null,
    ] {
        let mem = seeded(RunTarget::S3Only);
        let p = planned_and_started(&mem).await;
        finalize(&mem, &p, validated.clone()).await;
        let c = completed(&mem);
        assert!(c.get("detail").is_none(), "{validated}: {c}");
    }
}

#[tokio::test]
async fn b821_the_notice_survives_a_long_detail_and_the_label_keeps_its_reason() {
    let mem = Mem::default();
    mem.seed(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
    );
    mem.put_bundle();
    // A run the harness ended itself: the label reads the first sentence of the detail, and the detail is as long as it may be.
    let mut s = summary(DirectOutcome::StoppedOther, Some("Working on it."), true);
    let reason = format!("{HARNESS_STOP_PREFIX} three turns changed no file. ");
    s.detail = Some(format!(
        "{reason}{}",
        "x".repeat(1_000 - reason.chars().count())
    ));
    s.label = DirectOutcome::StoppedOther.label_for(s.detail.as_deref());
    mem.put_summary(&s);
    let p = planned_and_started(&mem).await;
    let validated = json!({"pass": true, "findings": [finding("workflow-file", WORKFLOW)]});
    finalize(&mem, &p, validated).await;
    let c = completed(&mem);
    let detail = c["detail"].as_str().unwrap();
    assert!(
        detail.chars().count() <= 1_000,
        "{}",
        detail.chars().count()
    );
    assert!(
        detail.ends_with(WORKFLOW_NOTICE),
        "the harness's sentence is kept whole at the end: {detail}"
    );
    assert!(detail.starts_with(&reason), "{detail}");
    assert_eq!(
        c["label"], "Cline stopped: the harness ended the run (three turns changed no file)",
        "{c}"
    );
}

#[tokio::test]
async fn b821_a_path_in_the_notice_is_one_clean_short_line() {
    // The path is the engine's text: a file name may hold a line break or an escape, and may be long.
    let mem = seeded(RunTarget::S3Only);
    let p = planned_and_started(&mem).await;
    let validated = json!({"pass": true, "findings": [
        finding("workflow-file", ".github/workflows/a\nb\u{1b}[31m.yml"),
    ]});
    finalize(&mem, &p, validated).await;
    let c = completed(&mem);
    let detail = c["detail"].as_str().unwrap();
    assert!(
        !detail.chars().any(char::is_control),
        "no control character reaches the card: {detail:?}"
    );
    assert!(
        detail.contains("(.github/workflows/a b [31m.yml)"),
        "{detail}"
    );

    let mem = seeded(RunTarget::S3Only);
    let p = planned_and_started(&mem).await;
    let long = format!(".github/workflows/{}.yml", "n".repeat(400));
    let validated = json!({"pass": true, "findings": [
        finding("workflow-file", &long),
        finding("workflow-file", ".github/workflows/b.yml"),
    ]});
    finalize(&mem, &p, validated).await;
    let c = completed(&mem);
    let detail = c["detail"].as_str().unwrap();
    assert!(
        detail.contains(&format!(
            "(.github/workflows/{}... and 1 more)",
            "n".repeat(120 - ".github/workflows/".len())
        )),
        "a long path is cut at 120 characters: {detail}"
    );
    assert!(detail.chars().count() < 300, "{}", detail.chars().count());
}

// ---- plan-delivery: the base the validator judges from ----------------------------------------------------------------------------

const SOURCE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z";
const OLDER: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3A";
/// The head the source run left: the base this run was staged on.
const SOURCE_HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
/// The base the source run itself started from.
const SOURCE_BASE: &str = "3333333333333333333333333333333333333333";
const OLDER_BASE: &str = "4444444444444444444444444444444444444444";

fn target_json(target: &RunTarget) -> Value {
    serde_json::to_value(target).unwrap()
}

/// An earlier run in the store: its Run item with `status`, its stored request (continuing `continues` when given) with the
/// `run.created` event that names the request's digest, and the decision file its `plan-delivery` wrote, which holds the base it
/// started from.
fn add_run(
    mem: &Mem,
    id: &str,
    status: &str,
    target: &RunTarget,
    base: Option<&str>,
    head: &str,
    continues: Option<(&str, &str)>,
) {
    let run: RunItem = serde_json::from_value(json!({
        "runId": id, "ownerPrincipalArn": OWNER, "promptSha256": "cd".repeat(32),
        "target": target_json(target), "status": status, "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-10-13T17:00:00Z", "version": 3
    }))
    .unwrap();
    mem.0.runs.lock().unwrap().insert(id.into(), run);
    let mut request: StartDirectRunRequest =
        serde_json::from_slice(&mem.blob(&s3::request_key(RUN)).unwrap()).unwrap();
    request.target = target.clone();
    request.continue_from = continues.map(|(run_id, head_sha)| ContinueFrom {
        run_id: run_id.into(),
        head_sha: Some(head_sha.into()),
    });
    let body = serde_json::to_vec(&request).unwrap();
    let digest = hex(&body);
    mem.put_blob(&s3::request_key(id), body);
    let created = Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("{}0", &id[..25]),
        run_id: id.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-10-13T17:00:00Z".into(),
        event_type: EventType::RunCreated,
        authority: Authority::Harness,
        payload: Payload::Inline {
            value: json!({"target": target_json(target), "requestSha256": digest, "mode": "direct"}),
        },
    };
    mem.0
        .events
        .lock()
        .unwrap()
        .insert(created.event_id.clone(), created);
    let evidence = json!({
        "schemaVersion": 1, "runId": id, "outcome": "reported_done", "label": "Cline reported done",
        "detail": null, "headSource": "final", "headSha": head, "baseSha": base,
        "plannedAt": "2026-10-13T17:50:00Z", "plannedAtMs": 1_790_000_000_000u64, "failure": null, "summary": null,
    });
    mem.put_blob(
        &s3::evidence_key(id),
        serde_json::to_vec(&evidence).unwrap(),
    );
}

/// This run (`RUN`) with `target`, continuing `SOURCE` from the head it left.
fn follow_up(target: RunTarget) -> Mem {
    let mem = Mem::default();
    let mut request = mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    request.continue_from = Some(ContinueFrom {
        run_id: SOURCE.into(),
        head_sha: Some(SOURCE_HEAD.into()),
    });
    mem.replace_request(serde_json::to_vec(&request).unwrap());
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct(a, b)."),
        true,
    ));
    mem
}

fn staged_on_the_source_head() -> Value {
    let mut st = staged();
    st["baseSha"] = json!(SOURCE_HEAD);
    st
}

async fn judged_base(mem: &Mem) -> Value {
    let p = plan_staged(mem, staged_on_the_source_head()).await;
    assert_eq!(p["deliver"], true, "{p}");
    // The package and the record of the delivery keep the staged base whatever the validator judges from.
    assert_eq!(p["package"]["baseSha"], SOURCE_HEAD, "{p}");
    p["validate"]["baseSha"].clone()
}

#[tokio::test]
async fn b821_a_follow_up_of_a_delivered_run_is_judged_from_the_head_that_run_delivered() {
    for status in ["complete", "paused", "stalled"] {
        for target in [RunTarget::S3Only, repo_target()] {
            let mem = follow_up(target.clone());
            add_run(
                &mem,
                SOURCE,
                status,
                &target,
                Some(SOURCE_BASE),
                SOURCE_HEAD,
                None,
            );
            assert_eq!(
                judged_base(&mem).await,
                SOURCE_HEAD,
                "{status} {target:?}: that head passed the check when it was delivered"
            );
        }
    }
}

#[tokio::test]
async fn b821_a_follow_up_of_a_run_that_was_not_delivered_is_judged_from_that_runs_base() {
    // Blocked: the check refused its head (a workflow file on a repository, a secret), yet its bundle exists and a follow-up starts
    // from it. Cleared and the limits: no delivery either.
    for status in ["blocked", "cleared", "usage_limited", "budget_limited"] {
        for target in [RunTarget::S3Only, repo_target()] {
            let mem = follow_up(target.clone());
            add_run(
                &mem,
                SOURCE,
                status,
                &target,
                Some(SOURCE_BASE),
                SOURCE_HEAD,
                None,
            );
            assert_eq!(
                judged_base(&mem).await,
                SOURCE_BASE,
                "{status} {target:?}: the commits of the run that was not delivered are judged with this run's"
            );
        }
    }
}

#[tokio::test]
async fn b821_a_repository_follow_up_of_a_zip_run_is_judged_from_the_zip_runs_base() {
    // control-api refuses this start today (a repository follow-up continues a run of the same repository). If it ever reaches
    // delivery, what the zip run was allowed as a zip is judged as a repository delivery.
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        SOURCE,
        "complete",
        &RunTarget::S3Only,
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        None,
    );
    assert_eq!(judged_base(&mem).await, SOURCE_BASE);
    // The other way round needs nothing: a zip follow-up of a delivered repository run starts from a head the stricter rule passed.
    let mem = follow_up(RunTarget::S3Only);
    add_run(
        &mem,
        SOURCE,
        "complete",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        None,
    );
    assert_eq!(judged_base(&mem).await, SOURCE_HEAD);
}

#[tokio::test]
async fn b821_a_chain_of_runs_that_were_not_delivered_is_judged_from_the_first_base_that_was() {
    // OLDER (blocked) <- SOURCE (blocked, continued OLDER) <- this run: judged from OLDER's base.
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        OLDER,
        "blocked",
        &repo_target(),
        Some(OLDER_BASE),
        SOURCE_BASE,
        None,
    );
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        Some((OLDER, SOURCE_BASE)),
    );
    assert_eq!(judged_base(&mem).await, OLDER_BASE);
    // OLDER delivered: the walk stops at the head it delivered, which is the base SOURCE started from.
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        OLDER,
        "complete",
        &repo_target(),
        Some(OLDER_BASE),
        SOURCE_BASE,
        None,
    );
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        Some((OLDER, SOURCE_BASE)),
    );
    assert_eq!(judged_base(&mem).await, SOURCE_BASE);
}

#[tokio::test]
async fn b821_a_source_whose_records_cannot_be_read_is_judged_from_the_empty_tree() {
    // No Run item for the source.
    let mem = follow_up(repo_target());
    let p = plan_staged(&mem, staged_on_the_source_head()).await;
    assert!(
        p["validate"].get("baseSha").is_none(),
        "no base is the empty tree: every file of the head is judged: {p}"
    );
    // A blocked source whose decision file is gone, is not this run's, or holds no base.
    for evidence in [
        None,
        Some(json!({"schemaVersion": 1, "runId": OLDER, "baseSha": SOURCE_BASE})),
        Some(json!({"schemaVersion": 1, "runId": SOURCE, "blocked": true, "plannedAtMs": 1})),
        Some(json!({"schemaVersion": 1, "runId": SOURCE, "baseSha": "not-a-commit"})),
    ] {
        let mem = follow_up(repo_target());
        add_run(
            &mem,
            SOURCE,
            "blocked",
            &repo_target(),
            Some(SOURCE_BASE),
            SOURCE_HEAD,
            None,
        );
        match &evidence {
            Some(v) => mem.put_blob(&s3::evidence_key(SOURCE), serde_json::to_vec(v).unwrap()),
            None => {
                mem.0
                    .blobs
                    .lock()
                    .unwrap()
                    .remove(&s3::evidence_key(SOURCE));
            }
        }
        let p = plan_staged(&mem, staged_on_the_source_head()).await;
        assert!(p["validate"].get("baseSha").is_none(), "{evidence:?}: {p}");
    }
    // A blocked source whose stored request is gone: whether it continued another run is not known.
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        None,
    );
    mem.0.blobs.lock().unwrap().remove(&s3::request_key(SOURCE));
    let p = plan_staged(&mem, staged_on_the_source_head()).await;
    assert!(p["validate"].get("baseSha").is_none(), "{p}");
}

#[tokio::test]
async fn b821_a_run_that_continues_nothing_is_judged_from_its_staged_base() {
    for target in [RunTarget::S3Only, repo_target()] {
        let mem = seeded(target);
        let p = plan(&mem).await;
        assert_eq!(p["validate"]["baseSha"], BASE, "{p}");
    }
}

#[tokio::test]
async fn b821_a_store_that_cannot_answer_fails_the_plan_so_the_machine_retries_it() {
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        None,
    );
    mem.deny_read(&s3::evidence_key(SOURCE));
    let e = call(
        &mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged_on_the_source_head(),
            "session": {"turn": turn_output(true), "failure": {}}}),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(e, RunSessionError::Unavailable(_)),
        "a base is never guessed from a read that failed: {e:?}"
    );
}

/// The ends of the walk that the full CI round of the 5.15 integration head found no test for: a stored request that cannot be read now, a
/// decision file that is not JSON, and a chain of exactly as many runs as the walk reads, and of one more.
#[tokio::test]
async fn fix515_b821_a_source_whose_stored_request_cannot_be_read_now_fails_the_plan() {
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        None,
    );
    // The decision file is read, then the request: the bucket refuses the request, which is no sign that the request is gone.
    mem.deny_read(&s3::request_key(SOURCE));
    let e = call(
        &mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged_on_the_source_head(),
            "session": {"turn": turn_output(true), "failure": {}}}),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(e, RunSessionError::Unavailable(_)),
        "whether the source continued a run is not guessed from a read that failed: {e:?}"
    );
}

#[tokio::test]
async fn fix515_b821_a_source_whose_decision_file_is_not_json_is_judged_from_the_empty_tree() {
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        None,
    );
    mem.put_blob(&s3::evidence_key(SOURCE), b"{\"runId\": ".to_vec());
    let p = plan_staged(&mem, staged_on_the_source_head()).await;
    assert!(p["validate"].get("baseSha").is_none(), "{p}");
    assert_eq!(p["validate"]["sourceRunIds"], json!([SOURCE]), "{p}");
}

/// The id and the base of link `i` of a chain of runs that were not delivered. The ids differ before their last character, which the
/// stand-in's event ids replace.
fn link(i: usize) -> (String, String) {
    (
        format!("01K5ZQ4T8M3N6P7R9S0V1W{i:03}0"),
        format!("{:040x}", 0xb000 + i),
    )
}

/// This run continues link 0, link 0 continues link 1, and so on for `runs` runs, none of them delivered.
fn chain(runs: usize) -> Mem {
    let mem = Mem::default();
    let mut request = mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    request.continue_from = Some(ContinueFrom {
        run_id: link(0).0,
        head_sha: Some(SOURCE_HEAD.into()),
    });
    mem.replace_request(serde_json::to_vec(&request).unwrap());
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct(a, b)."),
        true,
    ));
    for i in 0..runs {
        let (id, base) = link(i);
        let next = link(i + 1);
        // Each run started from the head the next one left: that head is the next one's own, and only its base matters here.
        let continues = (i + 1 < runs).then_some((next.0.as_str(), base.as_str()));
        add_run(
            &mem,
            &id,
            "blocked",
            &repo_target(),
            Some(&base),
            SOURCE_HEAD,
            continues,
        );
    }
    mem
}

#[tokio::test]
async fn fix515_b821_a_chain_as_long_as_the_walk_is_judged_from_the_base_of_its_last_run() {
    let runs = run_session::plan_delivery::MAX_SOURCE_RUNS;
    let p = plan_staged(&chain(runs), staged_on_the_source_head()).await;
    assert_eq!(p["validate"]["baseSha"], link(runs - 1).1, "{p}");
    let named = p["validate"]["sourceRunIds"].as_array().unwrap();
    assert_eq!(named.len(), runs, "{p}");
    assert_eq!(named[0], link(0).0);
    assert_eq!(named[runs - 1], link(runs - 1).0);
    // The validator of this build takes that many.
    assert_eq!(validate(&p).source_run_ids.len(), runs);
}

#[tokio::test]
async fn fix515_b821_a_chain_longer_than_the_walk_is_judged_from_the_empty_tree() {
    let runs = run_session::plan_delivery::MAX_SOURCE_RUNS;
    let p = plan_staged(&chain(runs + 1), staged_on_the_source_head()).await;
    assert!(
        p["validate"].get("baseSha").is_none(),
        "the run the walk did not reach may hold commits nobody judged, so every file of the head is judged: {p}"
    );
    assert_eq!(
        p["validate"]["sourceRunIds"].as_array().unwrap().len(),
        runs,
        "{p}"
    );
}

// ---- plan-delivery: the runs whose commits are judged with this run's --------------------------------------------------------------

#[tokio::test]
async fn b821_plan_delivery_names_the_runs_whose_commits_are_judged_with_this_runs() {
    // A blocked source: its commits are in the judged range, so the validator reads the items it stored for the copy rule.
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        None,
    );
    let p = plan_staged(&mem, staged_on_the_source_head()).await;
    assert_eq!(p["validate"]["sourceRunIds"], json!([SOURCE]), "{p}");
    // The validator reads the member the plan wrote.
    assert_eq!(
        serde_json::to_value(validate(&p)).unwrap()["sourceRunIds"],
        json!([SOURCE])
    );

    // OLDER (blocked) <- SOURCE (blocked, continued OLDER) <- this run: both, the nearest first.
    let mem = follow_up(RunTarget::S3Only);
    add_run(
        &mem,
        OLDER,
        "blocked",
        &RunTarget::S3Only,
        Some(OLDER_BASE),
        SOURCE_BASE,
        None,
    );
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &RunTarget::S3Only,
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        Some((OLDER, SOURCE_BASE)),
    );
    let p = plan_staged(&mem, staged_on_the_source_head()).await;
    assert_eq!(p["validate"]["sourceRunIds"], json!([SOURCE, OLDER]), "{p}");

    // OLDER delivered: the walk stops at the head it delivered, and its commits are not judged.
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        OLDER,
        "complete",
        &repo_target(),
        Some(OLDER_BASE),
        SOURCE_BASE,
        None,
    );
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        Some((OLDER, SOURCE_BASE)),
    );
    let p = plan_staged(&mem, staged_on_the_source_head()).await;
    assert_eq!(p["validate"]["sourceRunIds"], json!([SOURCE]), "{p}");

    // A repository follow-up of a zip run that was delivered: the zip run's commits are judged under the repository rule.
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        SOURCE,
        "complete",
        &RunTarget::S3Only,
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        None,
    );
    let p = plan_staged(&mem, staged_on_the_source_head()).await;
    assert_eq!(p["validate"]["sourceRunIds"], json!([SOURCE]), "{p}");
}

#[tokio::test]
async fn b821_a_run_judged_from_its_staged_base_names_no_source_run() {
    // A run that continues nothing.
    for target in [RunTarget::S3Only, repo_target()] {
        let p = plan(&seeded(target)).await;
        assert!(p["validate"].get("sourceRunIds").is_none(), "{p}");
    }
    // A follow-up of a run that was delivered under the rule this run is held to: that run's commits passed the check.
    for target in [RunTarget::S3Only, repo_target()] {
        let mem = follow_up(target.clone());
        add_run(
            &mem,
            SOURCE,
            "complete",
            &target,
            Some(SOURCE_BASE),
            SOURCE_HEAD,
            None,
        );
        let p = plan_staged(&mem, staged_on_the_source_head()).await;
        assert!(p["validate"].get("sourceRunIds").is_none(), "{p}");
    }
}

#[tokio::test]
async fn b821_a_source_whose_records_cannot_be_read_is_still_named() {
    // No Run item for the source: judged from the empty tree, and the items under its id, if any are left, are read.
    let mem = follow_up(repo_target());
    let p = plan_staged(&mem, staged_on_the_source_head()).await;
    assert!(p["validate"].get("baseSha").is_none(), "{p}");
    assert_eq!(p["validate"]["sourceRunIds"], json!([SOURCE]), "{p}");

    // A blocked source whose decision file is gone.
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        None,
    );
    mem.0
        .blobs
        .lock()
        .unwrap()
        .remove(&s3::evidence_key(SOURCE));
    let p = plan_staged(&mem, staged_on_the_source_head()).await;
    assert!(p["validate"].get("baseSha").is_none(), "{p}");
    assert_eq!(p["validate"]["sourceRunIds"], json!([SOURCE]), "{p}");

    // A blocked source whose stored request is gone: whether it continued another run is not known; the source itself is.
    let mem = follow_up(repo_target());
    add_run(
        &mem,
        SOURCE,
        "blocked",
        &repo_target(),
        Some(SOURCE_BASE),
        SOURCE_HEAD,
        None,
    );
    mem.0.blobs.lock().unwrap().remove(&s3::request_key(SOURCE));
    let p = plan_staged(&mem, staged_on_the_source_head()).await;
    assert!(p["validate"].get("baseSha").is_none(), "{p}");
    assert_eq!(p["validate"]["sourceRunIds"], json!([SOURCE]), "{p}");
}

/// run-session names at most as many source runs as the validator takes.
#[test]
fn b821_the_walk_names_no_more_source_runs_than_the_validator_takes() {
    assert_eq!(
        run_session::plan_delivery::MAX_SOURCE_RUNS,
        delivery::validator::MAX_SOURCE_RUNS
    );
}

// ---- mark-blocked: the way to get the work back -----------------------------------------------------------------------------------

async fn blocked_reason(mem: &Mem, violations: Value) -> String {
    let state =
        json!({"runId": RUN, "validated": {"result": {"pass": false, "violations": violations}}});
    let out = call(
        mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": state}),
    )
    .await
    .unwrap();
    let reason = out["reason"].as_str().unwrap().to_string();
    // The event carries the same words.
    let event = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "goal.blocked")
        .unwrap()
        .1;
    assert_eq!(event["reason"], reason);
    reason
}

fn violation(rule: &str, path: &str, detail: &str) -> Value {
    json!({"rule": rule, "path": path, "detail": detail})
}

/// The follow-up that brings the whole refused work back as a zip.
fn continue_words() -> String {
    format!(
        "To get all of this work as a zip, start a follow-up run with `ham continue {RUN} --s3-only '<prompt>'`. A zip run delivers such a file."
    )
}

/// The snapshot the fakes save (`put_snapshot`, `put_snapshot_t`): taken at 2026-10-13T18:50:00Z.
fn snapshot_words() -> String {
    format!(
        "The run's last snapshot was saved at 2026-10-13 18:50 UTC. Download it with `ham pull {RUN} --snapshot`; it does not hold work done after that time."
    )
}

const NO_SNAPSHOT: &str = "The run has no snapshot to download.";
const SECRET_WORDS: &str = "A follow-up of this run is refused for the same secret, which stays in its commits. Start a new run instead.";

/// The run of the review: it ended with its final bundle inside its first ten minutes, so the session saved no snapshot. The reason
/// named `ham pull <run> --snapshot`, a command that had nothing to download, and did not name the follow-up that delivers the work.
#[tokio::test]
async fn b821_the_blocked_reason_of_a_repository_run_names_the_follow_up_that_delivers_the_work_as_a_zip()
 {
    let mem = seeded(repo_target());
    assert!(mem.blob(&s3::snapshot_json_key_t(RUN, 1)).is_none());
    let reason = blocked_reason(
        &mem,
        json!([violation(
            "workflow-file",
            WORKFLOW,
            ".github/workflows/ci.yml is a workflow file, which a git host runs; a delivery to a repository refuses the change (A)"
        )]),
    )
    .await;
    assert_eq!(
        reason,
        format!(
            "bundle validation failed: workflow-file .github/workflows/ci.yml: .github/workflows/ci.yml is a workflow file, which a git host runs; a delivery to a repository refuses the change (A). {}",
            continue_words()
        )
    );
    assert!(
        !reason.contains("--snapshot"),
        "the run saved no snapshot: {reason}"
    );
    // The build specification reads the same way, alone and beside a workflow file.
    for violations in [
        json!([violation("buildspec", "buildspec.yml", "refused")]),
        json!([
            violation("workflow-file", WORKFLOW, "refused"),
            violation("buildspec", "buildspec.yml", "refused")
        ]),
    ] {
        let mem = seeded(repo_target());
        let reason = blocked_reason(&mem, violations).await;
        assert!(
            reason.ends_with(&format!(": refused. {}", continue_words())),
            "{reason}"
        );
    }
}

#[tokio::test]
async fn b821_the_blocked_reason_names_the_snapshot_command_only_when_the_run_saved_a_snapshot() {
    // A repository run that saved a snapshot: the follow-up first (it brings all of the work), then the snapshot and its time.
    let mem = seeded(repo_target());
    mem.put_snapshot(HEAD);
    let reason = blocked_reason(
        &mem,
        json!([violation("workflow-file", WORKFLOW, "refused")]),
    )
    .await;
    assert!(
        reason.ends_with(&format!(
            ": refused. {} {}",
            continue_words(),
            snapshot_words()
        )),
        "{reason}"
    );

    // The engine's executable configuration is refused for every target: no zip takes it, so no follow-up is named.
    for target in [repo_target(), RunTarget::S3Only] {
        let mem = seeded(target.clone());
        let reason = blocked_reason(
            &mem,
            json!([violation(
                "engine-config",
                ".cline/hooks/PreToolUse",
                "refused"
            )]),
        )
        .await;
        assert!(
            reason.ends_with(&format!(": refused. {NO_SNAPSHOT}")),
            "{target:?}: {reason}"
        );
        assert!(
            !reason.contains("--snapshot") && !reason.contains("ham continue"),
            "{target:?}: {reason}"
        );
        let mem = seeded(target.clone());
        mem.put_snapshot(HEAD);
        let reason = blocked_reason(
            &mem,
            json!([violation(
                "engine-config",
                ".cline/hooks/PreToolUse",
                "refused"
            )]),
        )
        .await;
        assert!(
            reason.ends_with(&format!(": refused. {}", snapshot_words())),
            "{target:?}: {reason}"
        );
    }

    // A zip run that a validator of an earlier release refused for a workflow file: it is a --s3-only run already.
    let mem = seeded(RunTarget::S3Only);
    let reason = blocked_reason(
        &mem,
        json!([violation("workflow-file", WORKFLOW, "refused")]),
    )
    .await;
    assert!(
        reason.ends_with(&format!(": refused. {NO_SNAPSHOT}")),
        "{reason}"
    );
    assert!(!reason.contains("--s3-only"), "{reason}");
}

/// `ham pull <run> --snapshot` asks the service for the newest snapshot record of the run's sessions, the current one first, and
/// downloads the bundle beside it. The reason names the command only when that pair exists.
#[tokio::test]
async fn b821_the_snapshot_the_reason_names_is_the_one_the_pull_command_finds() {
    let engine_config = || {
        json!([violation(
            "engine-config",
            ".cline/hooks/PreToolUse",
            "refused"
        )])
    };
    let in_slice = |mem: &Mem, slice: u32| {
        mem.0.runs.lock().unwrap().get_mut(RUN).unwrap().slice = Some(slice);
    };

    // A run in its second session whose only snapshot is the first session's.
    let mem = seeded(repo_target());
    in_slice(&mem, 2);
    mem.put_snapshot_t(1, HEAD);
    let reason = blocked_reason(&mem, engine_config()).await;
    assert!(
        reason.ends_with(&format!(": refused. {}", snapshot_words())),
        "{reason}"
    );

    // A record whose bundle is gone is no snapshot to download.
    let mem = seeded(repo_target());
    mem.put_snapshot(HEAD);
    mem.0
        .blobs
        .lock()
        .unwrap()
        .remove(&s3::snapshot_bundle_key_t(RUN, 1));
    let reason = blocked_reason(&mem, engine_config()).await;
    assert!(
        reason.ends_with(&format!(": refused. {NO_SNAPSHOT}")),
        "{reason}"
    );

    // The newest record decides, as it does for the command: the second session's record has no bundle.
    let mem = seeded(repo_target());
    in_slice(&mem, 2);
    mem.put_snapshot_t(1, HEAD);
    mem.put_snapshot_t(2, HEAD);
    mem.0
        .blobs
        .lock()
        .unwrap()
        .remove(&s3::snapshot_bundle_key_t(RUN, 2));
    let reason = blocked_reason(&mem, engine_config()).await;
    assert!(
        reason.ends_with(&format!(": refused. {NO_SNAPSHOT}")),
        "{reason}"
    );

    // A record that names no time.
    let mem = seeded(repo_target());
    mem.put_blob(&s3::snapshot_bundle_key_t(RUN, 1), b"SNAP".to_vec());
    mem.put_blob(
        &s3::snapshot_json_key_t(RUN, 1),
        serde_json::to_vec(&json!({"schemaVersion": 1, "headSha": HEAD})).unwrap(),
    );
    let reason = blocked_reason(&mem, engine_config()).await;
    assert!(
        reason.ends_with(&format!(
            ": refused. Download the run's last snapshot with `ham pull {RUN} --snapshot`; it does not hold work done after it was saved."
        )),
        "{reason}"
    );
}

/// A zip follow-up delivers the work only when a workflow file or a build specification is all the run was refused for.
#[tokio::test]
async fn b821_a_refusal_that_a_zip_would_not_take_names_no_follow_up() {
    for other in [
        violation("engine-config", "opencode.json", "refused"),
        violation(
            "third-party-copy",
            "src/vendor/util.js",
            "equals a stored file",
        ),
        json!({"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"}),
    ] {
        let mem = seeded(repo_target());
        let reason = blocked_reason(
            &mem,
            json!([violation("workflow-file", WORKFLOW, "refused"), other]),
        )
        .await;
        assert!(!reason.contains("ham continue"), "{other}: {reason}");
        assert!(reason.ends_with(NO_SNAPSHOT), "{other}: {reason}");
    }
}

/// A follow-up of a run refused for a secret is judged from that run's base, and the scan reads every commit of the range: the
/// follow-up is refused again. The reason says so.
#[tokio::test]
async fn b821_the_blocked_reason_for_a_secret_says_that_a_follow_up_is_refused_too() {
    let secret = || {
        violation(
            "secret",
            "src/a.py",
            "gitleaks rule aws-access-token at src/a.py line 3 in commit 0123456789ab",
        )
    };
    let mem = seeded(repo_target());
    let reason = blocked_reason(
        &mem,
        json!([
            secret(),
            {"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"}
        ]),
    )
    .await;
    assert_eq!(
        reason,
        format!(
            "bundle validation failed: secret src/a.py: gitleaks rule aws-access-token at src/a.py line 3 in commit 0123456789ab; bundle-size: the bundle is 3 bytes; the limit is 2 bytes. {SECRET_WORDS} {NO_SNAPSHOT}"
        )
    );
    // Beside a workflow file on a repository run: the zip follow-up would be refused for the secret, so it is not named.
    let mem = seeded(repo_target());
    mem.put_snapshot(HEAD);
    let reason = blocked_reason(
        &mem,
        json!([violation("workflow-file", WORKFLOW, "refused"), secret()]),
    )
    .await;
    assert!(!reason.contains("ham continue"), "{reason}");
    assert!(
        reason.ends_with(&format!(". {SECRET_WORDS} {}", snapshot_words())),
        "{reason}"
    );
}

#[tokio::test]
async fn b821_a_blocked_reason_without_a_protected_path_or_a_secret_is_the_one_it_was() {
    // A snapshot exists, and the reason of another refusal still reads as it did.
    let mem = seeded(repo_target());
    mem.put_snapshot(HEAD);
    let reason = blocked_reason(
        &mem,
        json!([
            violation("path-glob", "x/y", "x/y is outside the allowed paths"),
            {"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"}
        ]),
    )
    .await;
    assert_eq!(
        reason,
        "bundle validation failed: path-glob x/y: x/y is outside the allowed paths; bundle-size: the bundle is 3 bytes; the limit is 2 bytes"
    );
    // A run that was blocked before any validation.
    let mem = seeded(repo_target());
    mem.put_snapshot(HEAD);
    let out = call(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": {"runId": RUN,
            "plan": {"result": {"deliver": false, "reason": "the session ended before any work was saved"}}}}),
    )
    .await
    .unwrap();
    assert_eq!(out["reason"], "the session ended before any work was saved");
}

/// The sentence about the snapshot is advice. A bucket that cannot answer does not keep the run from being recorded as blocked, and
/// the reason neither names a snapshot as saved nor says that there is none.
#[tokio::test]
async fn b821_a_bucket_that_cannot_answer_about_the_snapshot_does_not_keep_the_run_from_ending_blocked()
 {
    let mem = seeded(repo_target());
    mem.put_snapshot(HEAD);
    mem.deny_read(&s3::snapshot_json_key_t(RUN, 1));
    let reason = blocked_reason(
        &mem,
        json!([violation("engine-config", "opencode.json", "refused")]),
    )
    .await;
    assert!(
        reason.ends_with(&format!(
            ": refused. Whether the run saved a snapshot could not be read; `ham pull {RUN} --snapshot` downloads it if it did."
        )),
        "{reason}"
    );
    assert!(!reason.contains(NO_SNAPSHOT), "{reason}");
    assert_eq!(mem.run().status, GoalStatus::Blocked);
}

/// A blocked run that allowed third-party copies holds its copies as findings. Its follow-up is judged with its commits and reads the
/// items it stored, so the follow-up the reason names carries the same option; without it the follow-up would be refused for the copy.
#[tokio::test]
async fn b821_the_follow_up_carries_the_third_party_option_when_the_blocked_run_allowed_copies() {
    let mem = seeded(repo_target());
    let state = json!({"runId": RUN, "validated": {"result": {"pass": false,
        "violations": [violation("workflow-file", WORKFLOW, "refused")],
        "findings": [violation("third-party-copy", "src/vendor/util.js", "equals a stored file")]}}});
    let out = call(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": state}),
    )
    .await
    .unwrap();
    let reason = out["reason"].as_str().unwrap();
    assert!(
        reason.ends_with(&format!(
            ": refused. To get all of this work as a zip, start a follow-up run with `ham continue {RUN} --s3-only --allow-third-party '<prompt>'`. A zip run delivers such a file."
        )),
        "{reason}"
    );
}
