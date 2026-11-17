//! A cancel that sticks. The user's keep-the-work cancel is recorded on the Run item (`cancelRequestedAt`, written by control-api).
//! While it is set the slice loop starts no further session: `next-slice` reads the Run item again right before its decision and ends the
//! loop with the reason `cancelled`, `start-session` refuses a session that was not invoked yet, and `plan-delivery` reads the record
//! itself, so a run whose `next-slice` failed still ends as a cancelled run. A cancel that no session answered ends the run with the
//! harness-ended label, delivers the last saved copy, and ends `blocked` with the cancel as the reason when no copy had been saved.
//! Nothing here removes or hides a copy (root invariant 9): the newest copy is what is delivered.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::gateway::GatewayRequest;
use delivery::packager::PackageRequest;
use delivery::validator::ValidateRequest;
use harness_protocol::direct::{DirectOutcome, MAX_LOST_SLICES, s3};
use harness_protocol::goal::GoalStatus;
use harness_protocol::output_pins::{OutputPin, OutputPins};
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::finalize::run_stop;
use run_session::ops::{Op, SessionState, StatesFailure, handle};
use run_session::plan_delivery::PlannedDelivery;
use run_session::slices::LoopState;
use serde_json::{Value, json};
use support::asl::{Lambdas, Wait, direct_definition, walk};
use support::fakes::*;

const COPY_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COPY_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// The fake store records every invoke at 2026-10-13T18:00:01Z. This cancel comes after it.
const CANCEL_AT: &str = "2026-10-13T18:55:22Z";
const CANCEL_WORDS: &str = "2026-10-13 18:55 UTC";
/// A cancel that was recorded before the session was invoked.
const CANCEL_BEFORE_INVOKE: &str = "2026-10-13T17:59:30Z";

fn cancel_label() -> String {
    format!(
        "Cline stopped: the harness ended the run (you stopped it at {CANCEL_WORDS} while its session was not answering)"
    )
}

fn cancel_reason() -> String {
    format!(
        "you stopped the run at {CANCEL_WORDS} while its session was not answering; no copy of its work had been saved, so there is nothing to deliver"
    )
}

async fn op(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn start(mem: &Mem, slice: u32) -> Result<Value, RunSessionError> {
    op(
        mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": format!("t{slice}"), "staged": staged(), "slice": slice}),
    )
    .await
}

fn seeded() -> Mem {
    let mem = Mem::default();
    mem.set_now_ms(T0);
    mem.seed_with_members(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
        json!({"maxRunS": 8_000}),
    );
    mem
}

/// A run whose first session started at `T0`.
async fn started() -> Mem {
    let mem = seeded();
    start(&mem, 1).await.unwrap();
    mem.clear_events();
    mem
}

fn failed(error: &str) -> SessionState {
    SessionState {
        turn: None,
        failure: Some(StatesFailure {
            error: Some(error.into()),
            cause: None,
        }),
    }
}

/// The state machine's word for a session whose heartbeat stopped.
fn heartbeat_timeout() -> SessionState {
    failed("States.HeartbeatTimeout")
}

async fn next_slice(mem: &Mem, session: SessionState, state: LoopState) -> Value {
    handle(
        &mem.deps(),
        &Op::NextSlice {
            run_id: RUN.to_string(),
            staged: staged(),
            session,
            loop_state: state,
        },
    )
    .await
    .unwrap()
}

fn loop_of(v: &Value) -> LoopState {
    serde_json::from_value(v.clone()).unwrap()
}

async fn plan(mem: &Mem, session: SessionState, state: &Value) -> Value {
    op(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": session, "loop": state}),
    )
    .await
    .unwrap()
}

/// The payloads of the service's `run.waiting` events (its word about a lost session).
fn waiting(mem: &Mem) -> Vec<Value> {
    mem.events()
        .into_iter()
        .filter(|(t, _)| t == "run.waiting")
        .map(|(_, v)| v)
        .collect()
}

fn planned(v: &Value) -> PlannedDelivery {
    serde_json::from_value(v.clone()).unwrap()
}

fn packaged() -> Value {
    json!({"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096})
}

fn completed(mem: &Mem) -> Value {
    mem.events()
        .into_iter()
        .find(|(t, _)| t == "run.completed")
        .map(|(_, v)| v)
        .expect("run.completed")
}

// ---- T1: the measured defect ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b853_cancel_of_a_lost_session_starts_no_second_session() {
    // TRIGGER PRESENT: the same store without the record of a cancel plans session 2.
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_now_ms(T0 + 700_000);
    let out = next_slice(&mem, heartbeat_timeout(), LoopState::default()).await;
    assert_eq!(out["continue"], json!(true), "TRIGGER PRESENT: {out}");
    assert!(mem.blob(&s3::slice_record_key(RUN, 2)).is_some());

    // The user's cancel is recorded, the session never answers, and the state machine reports the missed heartbeat.
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_cancel(CANCEL_AT);
    mem.set_now_ms(T0 + 700_000);
    let out = next_slice(&mem, heartbeat_timeout(), LoopState::default()).await;
    assert_eq!(out["continue"], json!(false), "no session 2: {out}");
    assert_eq!(out["endReason"], json!("cancelled"), "{out}");
    assert!(
        mem.blob(&s3::slice_record_key(RUN, 2)).is_none(),
        "no plan for a second session is written"
    );
    assert_eq!(mem.invocations().len(), 1);
    // The service's word about the lost session says the run ends because of the cancel, and what is delivered.
    let words = waiting(&mem);
    assert_eq!(words.len(), 1, "{words:?}");
    assert_eq!(words[0]["cancelledAt"], json!(CANCEL_AT), "{}", words[0]);
    assert_eq!(words[0]["then"], json!("end"));
    assert_eq!(words[0]["delivers"], json!("copy"));
    assert!(words[0].get("nextSession").is_none());
    let lost = ham_core::lost_session::SessionLost::parse(
        &mem.envelopes()
            .into_iter()
            .find(|e| e.event_type.as_str() == "run.waiting")
            .unwrap(),
    )
    .expect("the client reads the payload");
    assert!(
        lost.sentence().ends_with(&format!(
            "You had stopped the run at {CANCEL_WORDS}, so no new session starts: the run ends and the service delivers the last saved copy."
        )),
        "{}",
        lost.sentence()
    );
    // The decision is stored: asked again, the same answer and no second event.
    let again = next_slice(&mem, SessionState::default(), LoopState::default()).await;
    assert_eq!(again, out);
    assert_eq!(waiting(&mem).len(), 1);

    // The delivery: the last saved copy, with the label of a run the harness ended and the cancel as its reason.
    let p = plan(&mem, heartbeat_timeout(), &out).await;
    assert_eq!(p["deliver"], json!(true), "{p}");
    assert_eq!(p["outcome"], json!("stopped_other"));
    assert_eq!(p["headSha"], json!(COPY_A));
    assert_eq!(p["headSource"], json!("snapshot"));
    assert_eq!(
        p["detail"],
        json!(format!(
            "The harness ended the run: you stopped it at {CANCEL_WORDS} while its session was not answering. No further session was \
             started. The work is the last saved snapshot, taken at 2026-10-13 18:50 UTC."
        ))
    );
    assert_eq!(p["label"], json!(cancel_label()));
    assert_eq!(p["sessions"], json!(1));
    assert_eq!(p["endReason"], json!("cancelled"));
    assert_eq!(
        run_stop(&planned(&p), "Cline"),
        None,
        "a cancelled run that is delivered ends complete"
    );
    // The copy is still where `ham pull <run> --snapshot` reads it.
    assert!(mem.blob(&s3::snapshot_bundle_key_t(RUN, 1)).is_some());

    // A third lost session in a row with the cancel recorded ends for the cancel, not for the lost sessions.
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_cancel(CANCEL_AT);
    mem.set_now_ms(T0 + 700_000);
    let prev = LoopState {
        slice: 1,
        lost: MAX_LOST_SLICES - 1,
        ..LoopState::default()
    };
    let out = next_slice(&mem, heartbeat_timeout(), prev).await;
    assert_eq!(out["continue"], json!(false));
    assert_eq!(out["endReason"], json!("cancelled"), "{out}");
}

// ---- T1b: the measured case, no copy was ever saved --------------------------------------------------------------------------

#[tokio::test]
async fn b853_a_cancelled_run_without_a_copy_ends_blocked_with_the_cancel_reason() {
    let mem = started().await;
    mem.set_cancel(CANCEL_AT);
    mem.set_now_ms(T0 + 700_000);
    let out = next_slice(&mem, heartbeat_timeout(), LoopState::default()).await;
    assert_eq!(out["continue"], json!(false), "{out}");
    assert_eq!(out["endReason"], json!("cancelled"));
    assert!(mem.blob(&s3::slice_record_key(RUN, 2)).is_none());
    let words = waiting(&mem);
    assert_eq!(words.len(), 1);
    assert_eq!(words[0]["delivers"], json!("nothing"), "{}", words[0]);
    assert_eq!(words[0]["cancelledAt"], json!(CANCEL_AT));

    let p = plan(&mem, heartbeat_timeout(), &out).await;
    assert_eq!(p["deliver"], json!(false), "{p}");
    assert_eq!(p["reason"], json!(cancel_reason()));
    assert_eq!(p["outcome"], json!("stopped_other"));
    assert_eq!(p["endReason"], json!("cancelled"));
    assert_eq!(p["label"], json!(cancel_label()));
    assert_eq!(
        p["detail"],
        json!(format!(
            "The harness ended the run: you stopped it at {CANCEL_WORDS} while its session was not answering. No further session was started."
        ))
    );

    // The machine ends the run blocked: the reason is said once, and the run list carries the same label as the card.
    let state = json!({
        "runId": RUN, "loop": out, "plan": {"result": p},
        "session": {"turn": {}, "failure": {"Error": "States.HeartbeatTimeout"}},
    });
    let blocked = op(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": state}),
    )
    .await
    .unwrap();
    assert_eq!(blocked["status"], json!("blocked"));
    assert_eq!(
        blocked["reason"],
        json!(cancel_reason()),
        "the reason alone, without the detail in brackets"
    );
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    let touch = mem
        .index_touches()
        .pop()
        .expect("the search item is touched");
    assert_eq!(touch.3.as_deref(), Some(cancel_label().as_str()));
    // The card of a blocked run is its `run.completed` event with the status `blocked`.
    let card = completed(&mem);
    assert_eq!(card["status"], json!("blocked"), "{card}");
    assert_eq!(card["label"], json!(cancel_label()), "{card}");
}

// ---- T1c: the newer of two copies is delivered (root invariant 9) -------------------------------------------------------------

#[tokio::test]
async fn b853_the_newer_of_two_copies_is_delivered_after_a_cancel() {
    let mem = started().await;
    // Session 1 saved copy A and was lost; session 2 restored A.
    mem.put_snapshot_t(1, COPY_A);
    mem.set_now_ms(T0 + 700_000);
    let first = next_slice(&mem, heartbeat_timeout(), LoopState::default()).await;
    assert_eq!(first["continue"], json!(true), "{first}");
    start(&mem, 2).await.unwrap();
    // Session 2 saved copy B, other content at another commit, and was lost; the user had stopped the run.
    mem.put_snapshot_t(2, COPY_B);
    mem.put_blob(&s3::snapshot_bundle_key_t(RUN, 2), b"SNAP-B".to_vec());
    mem.set_cancel(CANCEL_AT);
    mem.set_now_ms(T0 + 1_500_000);
    let second = next_slice(&mem, heartbeat_timeout(), loop_of(&first)).await;
    assert_eq!(second["continue"], json!(false), "{second}");
    assert_eq!(second["endReason"], json!("cancelled"));
    assert!(mem.blob(&s3::slice_record_key(RUN, 3)).is_none());

    let p = plan(&mem, heartbeat_timeout(), &second).await;
    assert_eq!(p["deliver"], json!(true), "{p}");
    assert_eq!(p["headSha"], json!(COPY_B), "the newer copy");
    assert_eq!(
        p["validate"]["sourceBundleUri"],
        json!(format!(
            "s3://{BUCKET}/{}",
            s3::snapshot_bundle_key_t(RUN, 2)
        ))
    );
    assert_eq!(p["sessions"], json!(2));
    // Both copies are still in the bucket, each with its own bytes.
    assert_eq!(
        mem.blob(&s3::snapshot_bundle_key_t(RUN, 1)).as_deref(),
        Some(b"SNAP".as_slice())
    );
    assert_eq!(
        mem.blob(&s3::snapshot_bundle_key_t(RUN, 2)).as_deref(),
        Some(b"SNAP-B".as_slice())
    );
}

// ---- T1d: a cancel that lands while `next-slice` works ------------------------------------------------------------------------

#[tokio::test]
async fn b853_a_cancel_during_the_decision_is_seen_before_the_end_record() {
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_now_ms(T0 + 700_000);
    // The read at the top of the op sees no cancel; the cancel is recorded right after it.
    mem.cancel_run_after_reads(1, CANCEL_AT);
    let out = next_slice(&mem, heartbeat_timeout(), LoopState::default()).await;
    assert_eq!(mem.run().cancel_requested_at.as_deref(), Some(CANCEL_AT));
    assert_eq!(out["continue"], json!(false), "{out}");
    assert_eq!(out["endReason"], json!("cancelled"));
    assert!(
        mem.blob(&s3::slice_record_key(RUN, 2)).is_none(),
        "no plan for session 2"
    );
    assert!(mem.turn_n(2).is_none());
    let words = waiting(&mem);
    assert_eq!(words.len(), 1);
    assert_eq!(words[0]["then"], json!("end"), "{}", words[0]);
    assert!(words[0].get("nextSession").is_none(), "{}", words[0]);
}

// ---- T1e: `plan-delivery` reads the record itself -----------------------------------------------------------------------------

#[tokio::test]
async fn b853_plan_delivery_reads_the_flag_when_next_slice_failed() {
    // The `NextSlice` state failed: the loop state is the one of a slice that was never decided.
    let undecided = json!({"slice": 1});
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_cancel(CANCEL_AT);
    let p = plan(&mem, heartbeat_timeout(), &undecided).await;
    assert_eq!(p["outcome"], json!("stopped_other"), "{p}");
    assert_eq!(p["endReason"], json!("cancelled"));
    assert_eq!(p["label"], json!(cancel_label()));
    assert_eq!(run_stop(&planned(&p), "Cline"), None);
    // The decision is stored with the time of the cancel, and a second call reads it back.
    let evidence: Value =
        serde_json::from_slice(&mem.blob(&s3::evidence_key(RUN)).unwrap()).unwrap();
    assert_eq!(
        evidence["cancelRequestedAt"],
        json!(CANCEL_AT),
        "{evidence}"
    );
    let again = plan(&mem, heartbeat_timeout(), &undecided).await;
    assert_eq!(again, p);

    // A session that left a summary keeps its own outcome and detail, whatever the record says.
    let mem = started().await;
    mem.put_bundle_t(1);
    mem.put_summary_t(
        1,
        &summary(DirectOutcome::StoppedSilence, Some("Worked."), true),
    );
    mem.set_cancel(CANCEL_AT);
    let answered = SessionState {
        turn: Some(turn_output(true)),
        failure: None,
    };
    let p = plan(&mem, answered.clone(), &undecided).await;
    assert_eq!(p["outcome"], json!("stopped_silence"), "{p}");
    assert_eq!(p["detail"], json!("no output for 900 s"));
    assert!(p.get("endReason").is_none(), "{p}");

    // A session that handed over at its limit after the cancel was recorded gets no successor: its outcome stands, and its detail
    // says why no further session was started.
    let mem = started().await;
    mem.put_bundle_t(1);
    mem.put_summary_t(
        1,
        &summary(DirectOutcome::StoppedTimeout, Some("Worked."), true),
    );
    mem.set_cancel(CANCEL_AT);
    let p = plan(
        &mem,
        answered,
        &json!({"slice": 1, "endReason": "cancelled"}),
    )
    .await;
    assert_eq!(p["outcome"], json!("stopped_timeout"), "{p}");
    assert_eq!(
        p["detail"],
        json!(format!(
            "no output for 900 s. You stopped the run at {CANCEL_WORDS}, so no further session was started."
        ))
    );
    assert_eq!(p["endReason"], json!("cancelled"));
    assert_eq!(
        run_stop(&planned(&p), "Cline"),
        None,
        "the run is delivered and ends complete"
    );
    // The decision file with the time of the cancel is read as before by the reader of the engine's last message (the head route of
    // control-api and the client both use it; they and the packager read single places of the parsed file, no typed record).
    let evidence: Value =
        serde_json::from_slice(&mem.blob(&s3::evidence_key(RUN)).unwrap()).unwrap();
    assert_eq!(
        evidence["cancelRequestedAt"],
        json!(CANCEL_AT),
        "{evidence}"
    );
    assert_eq!(
        ham_core::head::cline_message_in_evidence(&evidence),
        Some("Worked.")
    );

    // A refusal of the run's final files keeps its own outcome and detail.
    let mem = seeded();
    mem.put_pinned_agent();
    let body =
        serde_json::to_vec(&summary(DirectOutcome::ReportedDone, Some("Done."), true)).unwrap();
    let stored = mem.agentd_writes(&s3::summary_key(RUN), body);
    let pins = OutputPins {
        bundle: Some(mem.agentd_writes(&s3::bundle_key(RUN), b"BUNDLE".to_vec())),
        // The record names another digest than the object the store holds.
        summary: Some(OutputPin::for_body(b"another summary", stored.version_id)),
        test_report: None,
    };
    mem.set_cancel(CANCEL_AT);
    let mut turn = turn_output(true);
    turn["outputPins"] = serde_json::to_value(&pins).unwrap();
    let refused = SessionState {
        turn: Some(turn),
        failure: None,
    };
    let p = plan(&mem, refused, &undecided).await;
    assert_eq!(p["deliver"], json!(false), "{p}");
    assert_eq!(p["outcome"], json!("stopped_other"));
    assert!(
        p["detail"].as_str().unwrap().starts_with(
            "The harness ended the run: the summary.json saved at the end of the run does not match"
        ),
        "{p}"
    );
    assert!(
        !p["reason"].as_str().unwrap().contains("you stopped"),
        "{p}"
    );
}

/// The second sentence of the detail is worded from the records: whether the run's last session was invoked after the cancel.
#[tokio::test]
async fn b853_the_detail_says_whether_a_session_was_already_starting() {
    let undecided = json!({"slice": 1});
    let detail_with = |second: &str, at: &str| {
        format!(
            "The harness ended the run: you stopped it at {at} while its session was not answering.{second} The work is the last saved \
             snapshot, taken at 2026-10-13 18:50 UTC."
        )
    };
    // The session was invoked after the cancel was recorded (the invoke was in flight) and never answered.
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_cancel(CANCEL_BEFORE_INVOKE);
    let p = plan(&mem, heartbeat_timeout(), &undecided).await;
    assert_eq!(
        p["detail"],
        json!(detail_with(
            " Session 1 was already starting when you stopped the run, and it did not answer.",
            "2026-10-13 17:59 UTC"
        ))
    );
    // The session was invoked before the cancel.
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_cancel(CANCEL_AT);
    let p = plan(&mem, heartbeat_timeout(), &undecided).await;
    assert_eq!(
        p["detail"],
        json!(detail_with(
            " No further session was started.",
            CANCEL_WORDS
        ))
    );
    // The session's record holds no time of the invoke: the sentence is dropped.
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_cancel(CANCEL_AT);
    mem.0
        .turns
        .lock()
        .unwrap()
        .get_mut(&("direct".to_string(), 1))
        .unwrap()
        .invoked_at = Some("not a time".into());
    let p = plan(&mem, heartbeat_timeout(), &undecided).await;
    assert_eq!(p["detail"], json!(detail_with("", CANCEL_WORDS)));
    assert_eq!(p["sessions"], json!(1));
    // A cancel time that does not read: the first sentence names no time and the second is dropped. The run still ends as cancelled.
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_cancel("yesterday");
    let p = plan(&mem, heartbeat_timeout(), &undecided).await;
    assert_eq!(
        p["detail"],
        json!(
            "The harness ended the run: you stopped it while its session was not answering. The work is the last saved snapshot, taken \
             at 2026-10-13 18:50 UTC."
        )
    );
    assert_eq!(p["endReason"], json!("cancelled"));
}

// ---- T4: the real machine, a cancel after the decision to start session 2 -----------------------------------------------------

/// The four Lambdas of the machine: run-session's real ops over the in-memory ports, and stand-ins for the delivery functions. Session 1
/// saves a copy and is lost. The user's cancel is recorded after `next-slice` decided to start session 2 and before `start-session` runs.
struct CancelWorld {
    mem: Mem,
    cancelled: bool,
    builder: tokio::runtime::Handle,
}

impl Lambdas for CancelWorld {
    fn invoke(&mut self, function: &str, payload: Value) -> Result<Value, (String, String)> {
        match function {
            "run-session" => {
                let op: Op = serde_json::from_value(payload)
                    .map_err(|e| ("RunSessionInvalid".to_string(), e.to_string()))?;
                if let Op::StartSession { slice: Some(2), .. } = &op
                    && !self.cancelled
                {
                    self.mem.set_cancel(CANCEL_AT);
                    self.cancelled = true;
                }
                let deps = self.mem.deps();
                let h = self.builder.clone();
                tokio::task::block_in_place(|| h.block_on(handle(&deps, &op)))
                    .map_err(|e| (e.error_type().to_string(), e.to_string()))
            }
            "repo-gateway" => match serde_json::from_value::<GatewayRequest>(payload).unwrap() {
                GatewayRequest::Stage(_) => Ok(staged()),
                other => panic!("an s3-only run asked the gateway for {other:?}"),
            },
            "bundle-validator" => {
                serde_json::from_value::<ValidateRequest>(payload).unwrap();
                Ok(json!({"pass": true, "violations": []}))
            }
            "packager" => {
                let req: PackageRequest = serde_json::from_value(payload).unwrap();
                let mut v = packaged();
                v["prUrl"] = json!(req.pr_url);
                Ok(v)
            }
            other => panic!("unknown function {other}"),
        }
    }

    fn event(&mut self, function: &str, payload: Value) -> Result<(), (String, String)> {
        assert_eq!(function, "run-session");
        let op: Op = serde_json::from_value(payload).unwrap();
        let deps = self.mem.deps();
        let h = self.builder.clone();
        let _ = tokio::task::block_in_place(|| h.block_on(handle(&deps, &op)));
        Ok(())
    }

    fn wait(&mut self, _function: &str, _token: &str) -> Wait {
        // Only session 1 is ever invoked: it saves a copy, then its heartbeat stops.
        assert_eq!(self.mem.invocations().len(), 1, "one session was invoked");
        self.mem.put_snapshot_t(1, COPY_A);
        self.mem.set_now_ms(self.mem.now_ms() + 700_000);
        Wait::HeartbeatTimeout
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b853_cancel_after_the_decision_stops_the_next_start() {
    let mem = seeded();
    let mut w = CancelWorld {
        mem: mem.clone(),
        cancelled: false,
        builder: tokio::runtime::Handle::current(),
    };
    let out = walk(&direct_definition(), json!({"runId": RUN}), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert!(
        w.cancelled,
        "TRIGGER PRESENT: the machine asked for session 2"
    );
    let count = |state: &str| out.trace.iter().filter(|s| s.as_str() == state).count();
    assert_eq!(count("StartSession"), 2, "{:?}", out.trace);
    assert_eq!(count("NextSlice"), 2, "{:?}", out.trace);
    assert!(
        out.caught
            .iter()
            .any(|(state, error)| state == "StartSession" && error == "RunSessionInvalid"),
        "{:?}",
        out.caught
    );
    assert_eq!(out.trace.last().map(String::as_str), Some("Finalize"));

    // The refused start invoked nothing, created no Turn item, left the Agent item at session 1 and its plan as it was.
    assert_eq!(mem.invocations().len(), 1, "no second session was invoked");
    assert!(
        mem.turn_n(2).is_none(),
        "no Turn item for the refused session"
    );
    assert_eq!(mem.agent().unwrap().session_generation, Some(1));
    assert!(mem.blob(&s3::slice_record_key(RUN, 2)).is_some());
    let words = waiting(&mem);
    assert_eq!(words.len(), 1, "{words:?}");
    assert_eq!(
        (words[0]["session"].clone(), words[0]["then"].clone()),
        (json!(1), json!("restart"))
    );
    // The decision for the refused slice was made fresh and read the record.
    let end: Value = serde_json::from_slice(
        &mem.blob(&format!("runs/{RUN}/slices/2-end.json"))
            .expect("the end record of slice 2"),
    )
    .unwrap();
    assert_eq!(end["endReason"], json!("cancelled"), "{end}");
    assert_eq!(end["continue"], json!(false));
    assert_eq!(end["cancelRequestedAt"], json!(CANCEL_AT));

    // The run delivers what session 2 would have restored, counts one session, and ends complete with the cancel label.
    let finalize = out
        .entered
        .iter()
        .find(|(state, _)| state == "Finalize")
        .map(|(_, data)| data["plan"]["result"].clone())
        .expect("the machine reached Finalize");
    assert_eq!(finalize["headSha"], json!(COPY_A), "{finalize}");
    assert_eq!(finalize["sessions"], json!(1));
    assert_eq!(finalize["endReason"], json!("cancelled"));
    assert_eq!(finalize["label"], json!(cancel_label()));
    assert_eq!(mem.run().status, GoalStatus::Complete);
    let card = completed(&mem);
    assert_eq!(card["label"], json!(cancel_label()), "{card}");
    assert_eq!(
        card["detail"],
        json!(format!(
            "The harness ended the run: you stopped it at {CANCEL_WORDS} while its session was not answering. No further session was \
             started. The work is the last saved snapshot, taken at 2026-10-13 18:50 UTC."
        )),
        "the card does not say that the run used 2 sessions"
    );
}

// ---- T5: a session that was invoked is not refused; one that was not is ---------------------------------------------------------

#[tokio::test]
async fn b853_a_started_session_is_not_refused_again() {
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_now_ms(T0 + 700_000);
    let first = next_slice(&mem, heartbeat_timeout(), LoopState::default()).await;
    assert_eq!(first["continue"], json!(true));
    start(&mem, 2).await.unwrap();
    assert_eq!(mem.invocations().len(), 2);
    // The cancel is recorded while session 2 runs; the state machine retries the start (its answer was lost).
    mem.set_cancel(CANCEL_AT);
    let again = start(&mem, 2)
        .await
        .expect("the running session is not refused");
    assert_eq!(again["alreadyInvoked"], json!(true), "{again}");
    assert_eq!(mem.invocations().len(), 2);
}

#[tokio::test]
async fn b853_a_session_whose_invoke_was_never_recorded_is_refused_and_stopped() {
    let mem = started().await;
    mem.put_snapshot_t(1, COPY_A);
    mem.set_now_ms(T0 + 700_000);
    let first = next_slice(&mem, heartbeat_timeout(), LoopState::default()).await;
    assert_eq!(first["continue"], json!(true));
    mem.clear_events();
    // The invoke of session 2 was accepted, and the function died before it recorded that (both tries of the write fail).
    for _ in 0..2 {
        mem.fail(
            "mark_turn_invoked",
            RunSessionError::Unavailable("the function ended".into()),
        );
    }
    start(&mem, 2)
        .await
        .expect_err("the start ends in an error");
    assert_eq!(
        mem.invocations().len(),
        2,
        "TRIGGER PRESENT: session 2 runs"
    );
    assert!(mem.turn_n(2).unwrap().invoked_at.is_none());
    let session_2 = mem.agent().unwrap().runtime_session_id;
    // The cancel is recorded before the retry: the retry starts nothing.
    mem.set_cancel(CANCEL_AT);
    let e = start(&mem, 2).await.expect_err("the retry is refused");
    assert!(
        matches!(&e, RunSessionError::Invalid(m) if m.contains("was cancelled")),
        "{e:?}"
    );
    assert_eq!(mem.invocations().len(), 2);
    // `next-slice` stops the session the Agent item names and ends the loop; no record says the slice was invoked, so it is not
    // announced as lost.
    mem.set_now_ms(T0 + 800_000);
    let out = next_slice(&mem, failed("RunSessionInvalid"), loop_of(&first)).await;
    assert_eq!(out["continue"], json!(false), "{out}");
    assert_eq!(out["endReason"], json!("cancelled"));
    assert!(
        mem.stops().iter().any(|(_, id)| *id == session_2),
        "the running session is stopped: {:?}",
        mem.stops()
    );
    assert!(waiting(&mem).is_empty(), "{:?}", waiting(&mem));
}

/// A start that is refused before the run's first session was ever invoked: nothing was saved, and the run ends blocked with the cancel
/// as the reason.
#[tokio::test]
async fn b853_a_cancel_before_the_first_invoke_starts_nothing() {
    let mem = seeded();
    mem.set_cancel(CANCEL_AT);
    let e = start(&mem, 1).await.expect_err("the start is refused");
    assert!(
        matches!(&e, RunSessionError::Invalid(m) if m.contains("was cancelled")),
        "{e:?}"
    );
    assert!(mem.invocations().is_empty());
    assert!(
        mem.turn_n(1).is_none(),
        "a refused slice leaves no Turn item"
    );
    let out = next_slice(&mem, failed("RunSessionInvalid"), LoopState::default()).await;
    assert_eq!(out["endReason"], json!("cancelled"), "{out}");
    assert!(
        waiting(&mem).is_empty(),
        "a slice that was never invoked was not lost"
    );
    let p = plan(&mem, failed("RunSessionInvalid"), &out).await;
    assert_eq!(p["deliver"], json!(false), "{p}");
    assert_eq!(p["reason"], json!(cancel_reason()));
    assert_eq!(p["sessions"], json!(0));
}

// ---- T8: one label for a run the harness ended --------------------------------------------------------------------------------

#[tokio::test]
async fn b853_the_label_of_a_harness_ended_run_is_the_same_everywhere() {
    // A delivered run whose detail begins with the harness's own words, and a note about the test report in front of it in the card.
    for (detail, label) in [
        (
            format!(
                "The harness ended the run: you stopped it at {CANCEL_WORDS} while its session was not answering. No further session \
                 was started. The work is the last saved snapshot."
            ),
            cancel_label(),
        ),
        (
            "The harness ended the run: the session sent no record of the files it saved at its end. They were not used. The work is \
             the last saved snapshot."
                .to_string(),
            "Cline stopped: the harness ended the run (the session sent no record of the files it saved at its end)".to_string(),
        ),
    ] {
        let mem = started().await;
        let p = json!({
            "deliver": true, "outcome": "stopped_other", "label": label, "detail": detail,
            "headSha": COPY_A, "headSource": "snapshot", "plannedAtMs": T0 + 1_000, "slice": 1,
            "reportNote": "the test report could not be saved to the run's evidence",
        });
        op(
            &mem,
            json!({"op": "finalize", "runId": RUN, "plan": p, "validated": {"pass": true, "violations": []},
                "packaged": packaged(), "prLink": {}}),
        )
        .await
        .unwrap();
        let card = completed(&mem);
        assert_eq!(card["label"], json!(label), "{card}");
        assert!(
            card["detail"]
                .as_str()
                .unwrap()
                .starts_with("the test report could not be saved"),
            "TRIGGER PRESENT: the note stands in front of the plan's detail: {card}"
        );
    }

    // A run that ends blocked: the run list carries the label of the card.
    for (detail, label) in [
        (
            format!(
                "The harness ended the run: you stopped it at {CANCEL_WORDS} while its session was not answering. No further session \
                 was started."
            ),
            cancel_label(),
        ),
        (
            "The harness ended the run: the bundle saved at the end of the run does not match the harness's record of it. It was not used"
                .to_string(),
            "Cline stopped: the harness ended the run (the bundle saved at the end of the run does not match the harness's record of it)"
                .to_string(),
        ),
    ] {
        let mem = started().await;
        let state = json!({
            "runId": RUN,
            "plan": {"result": {
                "deliver": false, "reason": "nothing was delivered", "outcome": "stopped_other", "label": label,
                "detail": detail, "plannedAtMs": T0 + 1_000, "slice": 1,
            }},
            "session": {"turn": {}, "failure": {}},
        });
        op(
            &mem,
            json!({"op": "mark-blocked", "runId": RUN, "state": state}),
        )
        .await
        .unwrap();
        let touch = mem.index_touches().pop().expect("the search item is touched");
        assert_eq!(touch.3.as_deref(), Some(label.as_str()));
    }
}
