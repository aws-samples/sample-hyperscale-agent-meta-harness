//! With: the user's cancel beside the read of a stopped session's storage. The two changes meet in `next-slice`, in
//! `plan-delivery` and in the service's word about a lost session.
//!
//! After a cancel the storage of the gone session is still read, and no further engine session starts. The run delivers the files the
//! service read back when its checks chose them, otherwise the last saved copy, and it ends `blocked` with the cancel as its reason
//! when there is neither. A cancel that arrives while the service reads, or while it waits for the stopped machine, is seen before
//! the decision. A session on Instances compute gets no read, and its cancelled run delivers the last saved copy. A discard wins over
//! a read in flight and delivers nothing. Nothing here removes or hides a copy (root invariant 9): files that were read back stay in
//! the bucket, used or not.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::lost_session::SessionLost;
use harness_protocol::direct::s3;
use harness_protocol::goal::GoalStatus;
use harness_protocol::target::RunTarget;
use run_session::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

/// The fake store records every invoke at 2026-10-13T18:00:01Z. This cancel comes after it.
const CANCEL_AT: &str = "2026-10-13T18:55:22Z";
const CANCEL_WORDS: &str = "2026-10-13 18:55 UTC";
const READ_BACK_SENTENCE: &str =
    "The work is what the service read back from the session's storage after the session ended.";

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

fn cancel_detail() -> String {
    format!(
        "The harness ended the run: you stopped it at {CANCEL_WORDS} while its session was not answering. No further session was started."
    )
}

fn cancel_reason() -> String {
    format!(
        "you stopped the run at {CANCEL_WORDS} while its session was not answering; no copy of its work had been saved, so there is nothing to deliver"
    )
}

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

/// A run whose first session started at `T0` and saved no copy.
async fn started() -> Mem {
    let mem = Mem::default();
    mem.set_now_ms(T0);
    clear_pauses();
    mem.seed_with_members(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
        json!({"maxRunS": 8_000}),
    );
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem
}

/// A run whose first session started at `T0` and saved a copy at commit `s`.
async fn started_with_copy(s: &str) -> Mem {
    let mem = started().await;
    mem.put_snapshot_t(1, s);
    mem
}

fn silence() -> Value {
    json!({"turn": {}, "failure": {"Error": "States.HeartbeatTimeout", "Cause": "heartbeat"}})
}

async fn recover(mem: &Mem) -> Value {
    call(
        mem,
        json!({"op": "recover", "runId": RUN, "session": silence(), "loop": {"slice": 1}}),
    )
    .await
    .unwrap()
}

/// What a worker answers after it stored a copy at commit `head` on top of `parent`: the storage's marker names `marker_head`.
fn stored(head: &str, parent: &str, marker_head: &str, sum: &str) -> Value {
    json!({
        "attemptId": "<attempt>", "runId": RUN, "session": 1, "recovered": true,
        "copy": {"headSha": head, "treeSha": sha('7'), "parentSha": parent,
                 "marker": {"schemaVersion": 1, "session": 1, "count": 5, "headSha": marker_head},
                 "stable": true, "files": 12, "bytes": 3456, "bundleBytes": 60, "bundleSha256": sum},
        "comparison": {"state": "done", "sameAsBaseline": false, "regressed": 0, "rewritten": 0},
        "uptimeS": 41, "tookMs": 2300, "agentdVersion": "0.1.0"
    })
}

fn record(mem: &Mem, key: &str) -> Option<Value> {
    mem.blob(key).map(|b| serde_json::from_slice(&b).unwrap())
}

fn end_of(mem: &Mem, n: u32) -> Value {
    record(mem, &format!("runs/{RUN}/slices/{n}-end.json"))
        .unwrap_or_else(|| panic!("no end record of session {n}"))
}

/// The service's words about a lost session, as the client reads them.
fn lost_words(mem: &Mem) -> Vec<SessionLost> {
    mem.envelopes()
        .iter()
        .filter_map(SessionLost::parse)
        .collect()
}

async fn next_slice(mem: &Mem, loop_state: Value) -> Value {
    call(
        mem,
        json!({"op": "next-slice", "runId": RUN, "staged": staged(), "session": silence(), "loop": loop_state}),
    )
    .await
    .unwrap()
}

async fn plan_delivery(mem: &Mem, loop_state: &Value) -> Value {
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": silence(), "loop": loop_state}),
    )
    .await
    .unwrap()
}

fn evidence_of(mem: &Mem) -> Value {
    record(mem, &s3::evidence_key(RUN)).expect("evidence/direct.json")
}

fn no_second_session(mem: &Mem, out: &Value) {
    assert_eq!(out["continue"], json!(false), "no further session: {out}");
    assert!(
        record(mem, &s3::slice_record_key(RUN, 2)).is_none(),
        "no plan of a second session is written"
    );
    assert!(mem.turn_n(2).is_none());
    assert_eq!(mem.invocations().len(), 1, "no engine session was started");
}

// ---- the files read back are delivered when the service's checks chose them ------------------------------------------------------

#[tokio::test]
async fn b853_b854_a_cancelled_run_delivers_the_files_read_back_when_the_checks_chose_them() {
    let (s, r) = (sha('5'), sha('6'));
    let read_back = s3::recovery_bundle_key(RUN, 1);

    // TRIGGER PRESENT: the same read without a cancel starts session 2 from the files read back.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    assert_eq!(recover(&mem).await["recovery"], "copied");
    let out = next_slice(&mem, json!({"slice": 1})).await;
    assert_eq!(out["continue"], json!(true), "TRIGGER PRESENT: {out}");
    assert_eq!(
        record(&mem, &s3::slice_record_key(RUN, 2)).unwrap()["recovery"]["head"],
        r,
        "TRIGGER PRESENT: session 2 is planned from the files read back"
    );

    // The cancel is recorded and the session never answers. The storage is still read: the read starts no engine.
    let mem = started_with_copy(&s).await;
    mem.set_cancel(CANCEL_AT);
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    assert_eq!(
        recover(&mem).await["recovery"],
        "copied",
        "a cancel that keeps the work leaves the run active, and the read is made"
    );
    assert_eq!(mem.recover_bodies().len(), 1);
    assert_eq!(mem.recover_bodies()[0]["kind"], "recover");
    let out = next_slice(&mem, json!({"slice": 1})).await;
    no_second_session(&mem, &out);
    assert_eq!(out["endReason"], json!("cancelled"), "{out}");
    let end = end_of(&mem, 1);
    assert_eq!(end["recovery"]["used"], json!(true), "{end}");
    assert_eq!(end["cancelRequestedAt"], json!(CANCEL_AT), "{end}");
    // The Run item names the copy the run delivers: it is what `ham pull <runId> --snapshot` follows.
    assert_eq!(
        serde_json::to_value(mem.run()).unwrap()["recoveredCopy"]["session"],
        1
    );

    // The service's word: the run ends for the cancel and delivers the files read back.
    let words = lost_words(&mem);
    assert_eq!(words.len(), 1);
    let lost = &words[0];
    assert_eq!(lost.cancelled_at.as_deref(), Some(CANCEL_AT));
    assert_eq!(lost.delivers.as_deref(), Some("copy"));
    assert_eq!(lost.read_back_session, Some(1));
    assert!(lost.restart().is_none());
    assert!(
        lost.sentence().ends_with(&format!(
            "You had stopped the run at {CANCEL_WORDS}, so no new session starts: the run ends and the service delivers the files it \
             read back from session 1's storage."
        )),
        "{}",
        lost.sentence()
    );
    assert!(
        !lost.sentence().contains("last saved copy"),
        "{}",
        lost.sentence()
    );

    // The delivery: the files read back, the label of a run the harness ended, the cancel first and then what the work is.
    let p = plan_delivery(&mem, &out).await;
    assert_eq!(p["deliver"], json!(true), "{p}");
    assert_eq!(
        p["headSha"],
        json!(r),
        "the files read back, not the saved copy"
    );
    assert_eq!(p["headSource"], json!("snapshot"));
    assert_eq!(p["outcome"], json!("stopped_other"));
    assert_eq!(p["endReason"], json!("cancelled"));
    assert_eq!(
        p["detail"],
        json!(format!("{} {READ_BACK_SENTENCE}", cancel_detail()))
    );
    assert!(
        !p["detail"]
            .as_str()
            .unwrap()
            .contains("last saved snapshot"),
        "{p}"
    );
    assert_eq!(evidence_of(&mem)["sourceKey"], json!(read_back));
    assert_eq!(
        p["validate"]["sourceBundleUri"],
        json!(s3::uri(BUCKET, &read_back))
    );
    // Both copies are still in the bucket.
    assert!(mem.blob(&s3::snapshot_bundle_key_t(RUN, 1)).is_some());
    assert!(mem.blob(&read_back).is_some());
}

// ---- otherwise the last saved copy ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b853_b854_a_cancelled_run_delivers_the_last_saved_copy_when_the_files_read_back_are_not_used()
 {
    let (s, r) = (sha('5'), sha('6'));
    let read_back = s3::recovery_bundle_key(RUN, 1);
    let mem = started_with_copy(&s).await;
    mem.set_cancel(CANCEL_AT);
    // The storage's marker names another commit than the saved copy: the storage does not prove to be newer, and it is not used.
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &sha('4'), &sum));
    assert_eq!(recover(&mem).await["recovery"], "copied");
    let out = next_slice(&mem, json!({"slice": 1})).await;
    no_second_session(&mem, &out);
    assert_eq!(out["endReason"], json!("cancelled"));
    let end = end_of(&mem, 1);
    assert_eq!(
        end["recovery"]["used"],
        json!(false),
        "TRIGGER PRESENT: {end}"
    );
    assert!(
        serde_json::to_value(mem.run()).unwrap()["recoveredCopy"].is_null(),
        "a copy that is not used is not named on the Run item"
    );

    let words = lost_words(&mem);
    assert_eq!(words.len(), 1);
    let said = words[0].sentence();
    assert!(
        said.contains(&format!(
            "You had stopped the run at {CANCEL_WORDS}, so no new session starts: the run ends and the service delivers the last \
             saved copy."
        )),
        "{said}"
    );
    assert!(
        said.ends_with(&format!(
            "Download the files read back with `ham pull {RUN} --read-back 1`."
        )),
        "files that were read back and not used are named with their command: {said}"
    );

    let p = plan_delivery(&mem, &out).await;
    assert_eq!(p["deliver"], json!(true), "{p}");
    assert_eq!(p["headSha"], json!(s), "the last saved copy");
    assert_eq!(p["endReason"], json!("cancelled"));
    assert_eq!(
        p["detail"],
        json!(format!(
            "{} The work is the last saved snapshot, taken at 2026-10-13 18:50 UTC.",
            cancel_detail()
        ))
    );
    assert!(evidence_of(&mem)["sourceKey"].is_null());
    assert!(
        mem.blob(&read_back).is_some(),
        "the files read back stay in the bucket"
    );
}

// ---- neither: blocked, with the cancel as the reason ---------------------------------------------------------------------------------

#[tokio::test]
async fn b853_b854_a_cancelled_run_with_neither_copy_ends_blocked_with_the_cancel_as_its_reason() {
    let blocked = |mem: Mem, out: Value, p: Value| async move {
        let state = json!({"runId": RUN, "loop": out, "plan": {"result": p}, "session": silence()});
        let b = call(
            &mem,
            json!({"op": "mark-blocked", "runId": RUN, "state": state}),
        )
        .await
        .unwrap();
        assert_eq!(mem.run().status, GoalStatus::Blocked);
        b["reason"].as_str().unwrap().to_string()
    };

    // No copy was saved, and the read of the storage missed.
    let mem = started().await;
    mem.set_cancel(CANCEL_AT);
    assert_eq!(recover(&mem).await["reason"], "worker-refused");
    let out = next_slice(&mem, json!({"slice": 1})).await;
    no_second_session(&mem, &out);
    assert_eq!(out["endReason"], json!("cancelled"));
    let words = lost_words(&mem);
    assert_eq!(words[0].delivers.as_deref(), Some("nothing"));
    assert_eq!(words[0].cancelled_at.as_deref(), Some(CANCEL_AT));
    let p = plan_delivery(&mem, &out).await;
    assert_eq!(p["deliver"], json!(false), "{p}");
    assert_eq!(p["reason"], json!(cancel_reason()));
    assert_eq!(p["endReason"], json!("cancelled"));
    assert_eq!(p["detail"], json!(cancel_detail()));
    assert_eq!(blocked(mem, out, p).await, cancel_reason());

    // No copy was saved, and the files read back are stored and not used: the run ends the same way, and the service's word names
    // the command that downloads the files read back.
    let r = sha('6');
    let read_back = s3::recovery_bundle_key(RUN, 1);
    let mem = started().await;
    mem.set_cancel(CANCEL_AT);
    let base = staged()["baseSha"].as_str().unwrap().to_string();
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &base, &sha('4'), &sum));
    assert_eq!(recover(&mem).await["recovery"], "copied");
    let out = next_slice(&mem, json!({"slice": 1})).await;
    no_second_session(&mem, &out);
    assert_eq!(end_of(&mem, 1)["recovery"]["used"], json!(false));
    let said = lost_words(&mem)[0].sentence();
    assert!(
        said.ends_with(&format!(
            "Download the files read back with `ham pull {RUN} --read-back 1`."
        )),
        "{said}"
    );
    let p = plan_delivery(&mem, &out).await;
    assert_eq!(p["deliver"], json!(false), "{p}");
    assert_eq!(p["endReason"], json!("cancelled"));
    assert!(mem.blob(&read_back).is_some());
}

// ---- a cancel while the service reads, and while it waits ----------------------------------------------------------------------------

#[tokio::test]
async fn b853_b854_a_cancel_while_the_service_reads_or_waits_is_seen_before_the_decision() {
    let (s, r) = (sha('5'), sha('6'));

    // While it reads: the cancel is recorded after the read's first look at the Run item. The read goes on and stores its copy.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    mem.cancel_run_after_reads(1, CANCEL_AT);
    assert_eq!(
        recover(&mem).await["recovery"],
        "copied",
        "a cancel does not stop the read"
    );
    assert_eq!(
        mem.run().cancel_requested_at.as_deref(),
        Some(CANCEL_AT),
        "TRIGGER PRESENT: the cancel landed while the service read"
    );
    let out = next_slice(&mem, json!({"slice": 1})).await;
    no_second_session(&mem, &out);
    assert_eq!(out["endReason"], json!("cancelled"));
    assert_eq!(end_of(&mem, 1)["recovery"]["used"], json!(true));
    let p = plan_delivery(&mem, &out).await;
    assert_eq!(p["headSha"], json!(r), "{p}");

    // While it waits for the stopped machine: the look at the top of next-slice sees no cancel, the cancel is recorded during the
    // wait, and the look after the wait sees it.
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    assert_eq!(recover(&mem).await["recovery"], "copied");
    assert_eq!(mem.run().cancel_requested_at, None);
    mem.cancel_run_after_reads(1, CANCEL_AT);
    clear_pauses();
    let out = next_slice(&mem, json!({"slice": 1})).await;
    assert!(
        !pauses().is_empty(),
        "TRIGGER PRESENT: the service waited for the stopped machine"
    );
    assert_eq!(mem.run().cancel_requested_at.as_deref(), Some(CANCEL_AT));
    no_second_session(&mem, &out);
    assert_eq!(out["endReason"], json!("cancelled"), "{out}");
    let end = end_of(&mem, 1);
    assert_eq!(end["recovery"]["used"], json!(true));
    assert_eq!(end["cancelRequestedAt"], json!(CANCEL_AT));
    let words = lost_words(&mem);
    assert_eq!(words.len(), 1);
    assert_eq!(words[0].read_back_session, Some(1));
    let p = plan_delivery(&mem, &out).await;
    assert_eq!(p["deliver"], json!(true), "{p}");
    assert_eq!(p["headSha"], json!(r));
}

// ---- a cancel after the decision to use the files read back, before the next session starts ---------------------------------------

/// The cancel is recorded after `next-slice` chose the files read back for session 2 and before session 2 was invoked. The start is
/// refused for the cancel, so the copy was never tried: the refused slice did not fail on it. The run delivers the files read back, as
/// the same cancel does a moment earlier (seen by the decision of session 1) or a moment later (answered by session 2).
#[tokio::test]
async fn b853_b854_a_cancel_between_the_decision_and_the_next_start_delivers_the_files_read_back() {
    let (s, r) = (sha('5'), sha('6'));
    let read_back = s3::recovery_bundle_key(RUN, 1);
    let mem = started_with_copy(&s).await;
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    assert_eq!(recover(&mem).await["recovery"], "copied");
    let out = next_slice(&mem, json!({"slice": 1})).await;
    assert_eq!(out["continue"], json!(true), "TRIGGER PRESENT: {out}");
    assert_eq!(
        end_of(&mem, 1)["recovery"]["used"],
        json!(true),
        "TRIGGER PRESENT: the decision of session 1 uses the files read back"
    );
    let plan = record(&mem, &s3::slice_record_key(RUN, 2)).unwrap();
    assert_eq!(
        (
            plan["recovery"]["head"].clone(),
            plan["restore"]["head"].clone()
        ),
        (json!(r), json!(s)),
        "TRIGGER PRESENT: session 2 is planned from the files read back, and its plan keeps the saved copy beside them"
    );

    // The cancel lands now: after the decision, before the invoke of session 2.
    mem.set_cancel(CANCEL_AT);
    let e = call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t2", "staged": staged(), "slice": 2}),
    )
    .await
    .expect_err("TRIGGER PRESENT: the start of session 2 is refused for the cancel");
    assert!(matches!(e, RunSessionError::Invalid(_)), "{e}");
    assert!(
        mem.turn_n(2).is_none(),
        "no Turn item for the refused session"
    );
    assert_eq!(mem.invocations().len(), 1, "no second session was invoked");

    // The machine catches the refusal and asks about the slice: nothing to read, then the decision.
    let refused =
        json!({"turn": {}, "failure": {"Error": "RunSessionInvalid", "Cause": e.to_string()}});
    let loop2 = json!({"slice": 2, "lost": 1, "empty": 0, "continue": true});
    assert_eq!(
        call(
            &mem,
            json!({"op": "recover", "runId": RUN, "session": refused, "loop": loop2}),
        )
        .await
        .unwrap(),
        json!({"recovery": "not-needed"}),
        "a session that was never invoked has no storage to read"
    );
    let out = call(
        &mem,
        json!({"op": "next-slice", "runId": RUN, "staged": staged(), "session": refused, "loop": loop2}),
    )
    .await
    .unwrap();
    assert_eq!(out["continue"], json!(false), "{out}");
    assert_eq!(out["endReason"], json!("cancelled"), "{out}");
    assert!(
        record(&mem, &s3::slice_record_key(RUN, 3)).is_none(),
        "no plan of a third session is written"
    );
    let end = end_of(&mem, 2);
    assert_eq!(end["cancelRequestedAt"], json!(CANCEL_AT), "{end}");
    assert_ne!(
        end["lostCause"],
        json!("restore-failed"),
        "a start that the cancel refused did not fail on the files read back: {end}"
    );
    // The refused slice never ran and is not announced as lost: the one word is that of session 1.
    let words = lost_words(&mem);
    assert_eq!(
        words.len(),
        1,
        "{:?}",
        words.iter().map(SessionLost::sentence).collect::<Vec<_>>()
    );
    assert_eq!(words[0].session, 1);

    // The delivery: the files read back, which session 2 was to start from, and not the older saved copy.
    let p = call(
        &mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": refused, "loop": out}),
    )
    .await
    .unwrap();
    assert_eq!(p["deliver"], json!(true), "{p}");
    assert_eq!(
        p["headSha"],
        json!(r),
        "the files read back, not the saved copy: {p}"
    );
    assert_eq!(p["endReason"], json!("cancelled"));
    assert_eq!(
        p["detail"],
        json!(format!("{} {READ_BACK_SENTENCE}", cancel_detail())),
        "{p}"
    );
    assert_eq!(evidence_of(&mem)["sourceKey"], json!(read_back));
    assert_eq!(
        p["validate"]["sourceBundleUri"],
        json!(s3::uri(BUCKET, &read_back))
    );
    // Both copies are still in the bucket, and the Run item still names the files read back for `ham pull <runId> --snapshot`.
    assert!(mem.blob(&s3::snapshot_bundle_key_t(RUN, 1)).is_some());
    assert!(mem.blob(&read_back).is_some());
    assert_eq!(
        serde_json::to_value(mem.run()).unwrap()["recoveredCopy"]["session"],
        1
    );
}

// ---- Instances compute: no read ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b853_b854_a_cancelled_run_on_instances_compute_gets_no_read_and_delivers_the_last_saved_copy()
 {
    let s = sha('5');
    let mem = started_with_copy(&s).await;
    mem.0
        .agents
        .lock()
        .unwrap()
        .get_mut("direct")
        .unwrap()
        .compute = Some("instances".into());
    mem.set_cancel(CANCEL_AT);
    assert_eq!(
        recover(&mem).await,
        json!({"recovery": "skipped", "reason": "instances"})
    );
    assert!(
        mem.recovery_calls().is_empty(),
        "no fence, no stop, no call"
    );
    clear_pauses();
    let out = next_slice(&mem, json!({"slice": 1})).await;
    assert!(
        pauses().is_empty(),
        "a read that was skipped has nothing to wait for: {:?}",
        pauses()
    );
    no_second_session(&mem, &out);
    assert_eq!(out["endReason"], json!("cancelled"));
    assert_eq!(end_of(&mem, 1)["recovery"]["reason"], json!("instances"));
    let said = lost_words(&mem)[0].sentence();
    assert!(
        said.contains("the run ends and the service delivers the last saved copy."),
        "{said}"
    );
    assert!(!said.contains("--read-back"), "{said}");
    let p = plan_delivery(&mem, &out).await;
    assert_eq!(p["deliver"], json!(true), "{p}");
    assert_eq!(p["headSha"], json!(s));
    assert_eq!(p["endReason"], json!("cancelled"));
}

// ---- a discard wins ---------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b853_b854_a_discard_wins_over_a_read_in_flight_and_keeps_what_was_read() {
    let (s, r) = (sha('5'), sha('6'));
    let read_back = s3::recovery_bundle_key(RUN, 1);

    // The run is discarded between the stop and the call of the read: the old session id is not called.
    let mem = started_with_copy(&s).await;
    mem.set_cancel(CANCEL_AT);
    mem.flip_run_after_reads(1, GoalStatus::Cleared);
    assert_eq!(
        recover(&mem).await,
        json!({"recovery": "skipped", "reason": "run-ended"})
    );
    assert!(mem.recover_bodies().is_empty());
    let out = next_slice(&mem, json!({"slice": 1})).await;
    no_second_session(&mem, &out);
    assert_eq!(out["endReason"], json!("run_ended"), "{out}");
    assert!(
        lost_words(&mem).is_empty(),
        "a run that was discarded is not told about a lost session"
    );

    // The read had stored its copy when the run was discarded: no session follows, the decision is the discard's and not the
    // cancel's, and the files read back stay in the bucket, where `ham pull <runId> --read-back 1` finds them.
    let mem = started_with_copy(&s).await;
    mem.set_cancel(CANCEL_AT);
    let sum = mem.put_recovery_bundle(1, &r);
    mem.recover_answers(&stored(&r, &s, &s, &sum));
    assert_eq!(recover(&mem).await["recovery"], "copied");
    mem.0.runs.lock().unwrap().get_mut(RUN).unwrap().status = GoalStatus::Cleared;
    let out = next_slice(&mem, json!({"slice": 1})).await;
    no_second_session(&mem, &out);
    assert_eq!(
        out["endReason"],
        json!("run_ended"),
        "the discard wins over the cancel: {out}"
    );
    assert!(end_of(&mem, 1).get("cancelRequestedAt").is_none());
    assert!(lost_words(&mem).is_empty());
    assert!(mem.blob(&read_back).is_some());
    assert!(mem.blob(&s3::snapshot_bundle_key_t(RUN, 1)).is_some());
}

// ---- the service says at the start whether it reads storage back -------------------------------------------------------------------

/// The payload of the service's `session.started` event of the run.
fn session_started(mem: &Mem) -> Value {
    mem.events()
        .into_iter()
        .find(|(t, _)| t == "session.started")
        .map(|(_, v)| v)
        .expect("session.started")
}

async fn started_with(extra: Value) -> Mem {
    let mem = Mem::default();
    mem.set_now_ms(T0);
    mem.seed_with_members(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
        extra,
    );
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem
}

/// The client's notice of a silent session promises a read of the session's storage only when the service said that it makes one.
/// The service says it in the first event of the run's session, and only for a run whose stopped sessions it reads back.
#[tokio::test]
async fn b854_the_service_says_at_the_start_whether_it_reads_a_stopped_sessions_storage_back() {
    // The default: the read is on, on a machine that keeps its storage.
    let mem = started_with(json!({"maxRunS": 8_000})).await;
    assert_eq!(
        session_started(&mem)["storageReadBack"],
        json!(true),
        "{}",
        session_started(&mem)
    );
    // A retry of the start writes the same event.
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    assert_eq!(session_started(&mem)["storageReadBack"], json!(true));

    // The run switched the read off: the member is left out, as a service of an earlier release leaves it out.
    let mem = started_with(json!({"maxRunS": 8_000, "recoverAfterStop": false})).await;
    assert_eq!(
        mem.agent().unwrap().recover_after_stop,
        Some(false),
        "TRIGGER PRESENT: the read is off for the run"
    );
    assert!(
        session_started(&mem).get("storageReadBack").is_none(),
        "{}",
        session_started(&mem)
    );
}
