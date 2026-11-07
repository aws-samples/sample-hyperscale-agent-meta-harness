//! A run that outlives one microVM session. The real `statemachines/direct.asl.json` is walked over in-process fakes of its four Lambdas
//! while a scripted agentd ends each slice: at its deadline with time left (a handoff), at its deadline with none left, with a result, or not at
//! all (a lost session). Every slice runs 600 s, the short limit a live proof sets with `HAMH_SLICE_DEADLINE_S`, and the clock moves by what
//! each slice takes, so the total time, the reserve and the loop's counters are the real ones.
//!
//! The things this holds: the run crosses the slice limit by itself and delivers once; each slice has its own session id, Turn item and keys;
//! the next slice restores what the one before it left and is told the run's original base; a lost session restarts from its snapshot; sessions
//! in a row that lose or change nothing stall the run; the total time ends the run paused; and a repeated `next-slice` decides nothing twice.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::gateway::GatewayRequest;
use delivery::packager::PackageRequest;
use delivery::validator::ValidateRequest;
use harness_protocol::direct::{DirectOutcome, s3};
use harness_protocol::goal::{GoalStatus, ResumeMode};
use harness_protocol::target::RunTarget;
use run_session::ops::{Op, handle};
use run_session::slices::LoopState;
use serde_json::{Value, json};
use std::collections::VecDeque;
use support::asl::{Lambdas, Wait, direct_definition, walk};
use support::fakes::*;

#[derive(Debug)]
enum EndKind {
    Handoff,
    Other,
}

const SLICE_S: u32 = 600;
/// Every slice takes its engine time and two minutes for the end sequence.
const END_S: u64 = 120;

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

/// How the scripted agentd ends the next slice.
#[derive(Clone, Debug)]
enum Ends {
    /// The slice's deadline with time left: committed, bundled, uploaded; turn-complete says `handoff`. `head` is the commit it ends on.
    Handoff(String),
    /// Reported done.
    Done(String),
    /// The last slice's deadline: the same end as a handoff, without the flag.
    Deadline(String),
    /// No turn-complete; the heartbeat stops. A snapshot at this head exists when `Some`.
    Lost(Option<String>),
    /// The new session could not restore the work: `stopped_setup_failed`.
    SetupFailed,
}

struct SliceWorld {
    mem: Mem,
    script: VecDeque<Ends>,
    calls: Vec<(String, Value)>,
    /// `(slice, session id)` as each slice was invoked.
    started: Vec<(u32, String)>,
    /// The Agent item's nonce hash as each slice saw it.
    nonces: Vec<String>,
    builder: tokio::runtime::Handle,
}

impl SliceWorld {
    fn new(mem: &Mem, script: Vec<Ends>) -> SliceWorld {
        SliceWorld {
            mem: mem.clone(),
            script: script.into(),
            calls: vec![],
            started: vec![],
            nonces: vec![],
            builder: tokio::runtime::Handle::current(),
        }
    }

    fn called(&self, function: &str) -> Vec<&Value> {
        self.calls
            .iter()
            .filter(|(f, _)| f == function)
            .map(|(_, v)| v)
            .collect()
    }
}

/// A summary for slice `n`, as agentd writes it: the slice's turn, the slice's bundle.
fn summary_n(
    n: u32,
    outcome: DirectOutcome,
    head: &str,
) -> harness_protocol::direct::DirectSummary {
    let mut v = serde_json::to_value(summary(outcome, Some("Worked on pct(a, b)."), true)).unwrap();
    v["turn"] = json!(n);
    v["git"]["headSha"] = json!(head);
    v["git"]["bundleUri"] = json!(format!("s3://{BUCKET}/{}", s3::bundle_key_t(RUN, n)));
    serde_json::from_value(v).unwrap()
}

fn turn_complete(n: u32, finish: &str, head: &str, handoff: bool) -> Value {
    let mut v = json!({
        "summary": "Worked on pct(a, b).", "runId": RUN, "agentId": "direct", "turn": n,
        "finishReason": finish, "outcome": "completed",
        "bundleUri": format!("s3://{BUCKET}/{}", s3::bundle_key_t(RUN, n)), "lastCommitSha": head,
    });
    if handoff {
        v["handoff"] = json!(true);
    }
    v
}

impl Lambdas for SliceWorld {
    fn invoke(&mut self, function: &str, payload: Value) -> Result<Value, (String, String)> {
        match function {
            "run-session" => {
                let op: Op = serde_json::from_value(payload.clone())
                    .map_err(|e| ("RunSessionInvalid".into(), e.to_string()))?;
                let deps = self.mem.deps();
                let h = self.builder.clone();
                tokio::task::block_in_place(|| h.block_on(handle(&deps, &op)))
                    .map_err(|e| (e.error_type().to_string(), e.to_string()))
            }
            "repo-gateway" => {
                let req: GatewayRequest = serde_json::from_value(payload.clone()).unwrap();
                self.calls.push((function.into(), payload));
                match req {
                    GatewayRequest::Stage(_) => Ok(staged()),
                    GatewayRequest::NewRepo(_) => Ok(json!({"repo": "hamh-e2e-new-20261103"})),
                    GatewayRequest::PushPr(_) => Ok(json!({"url": "https://example.invalid/pr/1"})),
                }
            }
            "bundle-validator" => {
                serde_json::from_value::<ValidateRequest>(payload.clone()).unwrap();
                self.calls.push((function.into(), payload));
                Ok(json!({"pass": true, "violations": []}))
            }
            "packager" => {
                let req: PackageRequest = serde_json::from_value(payload.clone()).unwrap();
                self.calls.push((function.into(), payload));
                Ok(
                    json!({"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096, "prUrl": req.pr_url}),
                )
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

    fn wait(&mut self, _function: &str, token: &str) -> Wait {
        // The invocation this wait follows: the last body the fake AgentCore received.
        let bodies = self.mem.invocations();
        let body = bodies.last().expect("start-session invoked the runtime");
        let n = u32::try_from(body["turn"].as_u64().unwrap()).unwrap();
        let session = body["runtimeSessionId"].as_str().unwrap().to_string();
        assert_eq!(
            session,
            dispatcher::ids::runtime_session_id(RUN, "direct", n),
            "the session id of slice {n}"
        );
        assert!(
            self.started.iter().all(|(_, s)| *s != session),
            "no session id is used twice: {session}"
        );
        // The slice before it was stopped before this one was invoked: one machine at a time.
        let stopped: Vec<String> = self.mem.stops().into_iter().map(|(_, s)| s).collect();
        for (earlier, id) in &self.started {
            assert!(
                stopped.contains(id),
                "slice {earlier} ({id}) was still running when slice {n} started; stopped: {stopped:?}"
            );
        }
        self.started.push((n, session));
        let agent = self.mem.agent().unwrap();
        assert_eq!(agent.session_generation, Some(n));
        assert_eq!(agent.runtime_session_id, self.started.last().unwrap().1);
        self.nonces
            .push(agent.creds.as_ref().unwrap().nonce_sha256.clone());
        let ct = self.mem.turn_n(n).unwrap().task_token_ciphertext;
        assert!(
            ct.contains(token),
            "worker-api completes the token of Turn {n}"
        );

        let engine_s = body["timeoutS"].as_u64().unwrap();
        let end = self
            .script
            .pop_front()
            .expect("the script has an end for every slice");
        let end_kind = match &end {
            Ends::Handoff(_) => EndKind::Handoff,
            _ => EndKind::Other,
        };
        let now = self.mem.now_ms();
        match end {
            Ends::Handoff(head) | Ends::Deadline(head) => {
                // agentd hands over only when the invocation said more time was left (slice.last false); on the last slice the same end carries no flag
                let more_time = body["slice"]["last"] == json!(false);
                assert_eq!(
                    more_time,
                    matches!(&end_kind, EndKind::Handoff),
                    "the script ends slice {n} with a {end_kind:?}, but the invocation said last={:?}",
                    body["slice"]["last"]
                );
                self.mem.set_now_ms(now + (engine_s + END_S) * 1000);
                self.mem.put_bundle_t(n);
                self.mem
                    .put_summary_t(n, &summary_n(n, DirectOutcome::StoppedTimeout, &head));
                Wait::Success(turn_complete(n, "stopped_timeout", &head, more_time))
            }
            Ends::Done(head) => {
                self.mem.set_now_ms(now + 300_000);
                self.mem.put_bundle_t(n);
                self.mem
                    .put_summary_t(n, &summary_n(n, DirectOutcome::ReportedDone, &head));
                Wait::Success(turn_complete(n, "end_turn", &head, false))
            }
            Ends::Lost(snapshot) => {
                self.mem.set_now_ms(now + (engine_s + 600) * 1000);
                if let Some(head) = snapshot {
                    self.mem.put_snapshot_t(n, &head);
                }
                Wait::HeartbeatTimeout
            }
            Ends::SetupFailed => {
                self.mem.set_now_ms(now + 60_000);
                let mut s = summary_n(n, DirectOutcome::StoppedSetupFailed, &sha('0'));
                s.git.bundle_uri = None;
                s.git.head_sha = None;
                self.mem.put_summary_t(n, &s);
                let mut v = turn_complete(n, "stopped_setup_failed", &sha('0'), false);
                v.as_object_mut().unwrap().remove("bundleUri");
                v.as_object_mut().unwrap().remove("lastCommitSha");
                Wait::Success(v)
            }
        }
    }
}

fn input() -> Value {
    json!({"runId": RUN})
}

/// A run of the s3-only target with a total of `max_run_s`, on a stage whose slices last 600 s.
fn seeded(max_run_s: u32) -> Mem {
    let mem = Mem::default();
    mem.set_now_ms(T0);
    mem.set_slice_deadline(SLICE_S);
    mem.seed_with_members(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
        json!({"maxRunS": max_run_s}),
    );
    mem
}

fn count(trace: &[String], state: &str) -> usize {
    trace.iter().filter(|s| s.as_str() == state).count()
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_run_crosses_the_slice_limit_by_itself_and_delivers_once() {
    let (a, b) = (sha('a'), sha('b'));
    let mem = seeded(4_500);
    let mut w = SliceWorld::new(&mem, vec![Ends::Handoff(a.clone()), Ends::Done(b.clone())]);
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 2, "{:?}", out.trace);
    assert_eq!(count(&out.trace, "NextSlice"), 2);
    assert_eq!(count(&out.trace, "ResetSession"), 1);
    assert_eq!(count(&out.trace, "Plan"), 1);
    assert_eq!(count(&out.trace, "Finalize"), 1);
    // two sessions, two Turn items, one nonce each
    assert_eq!(w.started.len(), 2);
    assert_ne!(w.started[0].1, w.started[1].1);
    assert_ne!(
        w.nonces[0], w.nonces[1],
        "the new session has its own refresh nonce"
    );
    assert!(mem.turn_n(1).is_some() && mem.turn_n(2).is_some());
    let bodies = mem.invocations();
    assert_eq!(bodies[1]["turn"], json!(2));
    assert_eq!(
        bodies[1]["repo"]["uri"],
        json!(format!("s3://{BUCKET}/{}", s3::bundle_key_t(RUN, 1)))
    );
    assert_eq!(bodies[1]["slice"]["baseSha"], json!(BASE));
    assert_eq!(bodies[1]["slice"]["restoreHead"], json!(a));
    assert_eq!(bodies[1]["slice"]["brief"], json!("Worked on pct(a, b)."));
    assert_eq!(
        bodies[0]["slice"]["last"],
        json!(false),
        "slice 1 is told more time is left"
    );
    assert_eq!(bodies[0]["timeoutS"], json!(SLICE_S));
    assert_eq!(w.called("packager").len(), 1);
    assert_eq!(w.called("bundle-validator").len(), 1);
    let validate = w.called("bundle-validator")[0];
    assert_eq!(validate["headSha"], json!(b));
    assert_eq!(validate["baseSha"], json!(BASE));
    assert_eq!(
        validate["sourceBundleUri"],
        json!(format!("s3://{BUCKET}/{}", s3::bundle_key_t(RUN, 2)))
    );
    let types = mem.event_types();
    assert_eq!(types.iter().filter(|t| *t == "run.completed").count(), 1);
    assert_eq!(types.iter().filter(|t| *t == "delivery.ready").count(), 1);
    // the record shows the session count
    let run = mem.run();
    assert_eq!(run.status, GoalStatus::Complete);
    assert_eq!(run.slice, Some(2));
    let completed = mem.events().pop().unwrap().1;
    assert!(
        completed["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("2 sessions"),
        "{completed}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_lost_session_restarts_from_its_snapshot_and_the_run_goes_on() {
    let (a, b) = (sha('a'), sha('b'));
    let mem = seeded(4_500);
    let mut w = SliceWorld::new(
        &mem,
        vec![Ends::Lost(Some(a.clone())), Ends::Done(b.clone())],
    );
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 2);
    let bodies = mem.invocations();
    assert_eq!(
        bodies[1]["repo"]["uri"],
        json!(format!(
            "s3://{BUCKET}/{}",
            s3::snapshot_bundle_key_t(RUN, 1)
        )),
        "slice 2 restores slice 1's last snapshot"
    );
    assert_eq!(bodies[1]["slice"]["restoreHead"], json!(a));
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert_eq!(w.called("packager").len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_session_lost_before_any_snapshot_restarts_from_the_same_work() {
    let b = sha('b');
    let mem = seeded(4_500);
    let mut w = SliceWorld::new(&mem, vec![Ends::Lost(None), Ends::Done(b.clone())]);
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    let bodies = mem.invocations();
    assert_eq!(
        bodies[1]["repo"]["uri"],
        json!(format!("s3://{BUCKET}/{}", s3::repo_bundle_key(RUN))),
        "nothing was saved, so the second session starts where the first did: the staged base"
    );
    assert_eq!(mem.run().status, GoalStatus::Complete);
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_three_lost_sessions_in_a_row_deliver_the_last_snapshot_and_the_run_is_stalled() {
    let (a, b, c) = (sha('a'), sha('b'), sha('c'));
    let mem = seeded(8_000);
    let mut w = SliceWorld::new(
        &mem,
        vec![
            Ends::Lost(Some(a)),
            Ends::Lost(Some(b)),
            Ends::Lost(Some(c.clone())),
        ],
    );
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 3);
    assert_eq!(count(&out.trace, "Finalize"), 1);
    let run = mem.run();
    assert_eq!(run.status, GoalStatus::Stalled);
    assert_eq!(run.resume_mode, Some(ResumeMode::FollowUp));
    assert!(
        run.status_reason
            .as_deref()
            .unwrap_or_default()
            .starts_with("session_lost: "),
        "{:?}",
        run.status_reason
    );
    // the work of the last slice that saved any is what was delivered
    let validate = w.called("bundle-validator")[0];
    assert_eq!(validate["headSha"], json!(c));
    assert_eq!(
        validate["sourceBundleUri"],
        json!(format!(
            "s3://{BUCKET}/{}",
            s3::snapshot_bundle_key_t(RUN, 3)
        ))
    );
    let completed = mem.events().pop().unwrap().1;
    assert_eq!(completed["status"], json!("stalled"));
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_three_slices_in_a_row_that_change_nothing_stall_the_run() {
    let mem = seeded(8_000);
    let same = BASE.to_string();
    let mut w = SliceWorld::new(
        &mem,
        vec![
            Ends::Handoff(same.clone()),
            Ends::Handoff(same.clone()),
            Ends::Handoff(same.clone()),
        ],
    );
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 3, "{:?}", out.trace);
    let run = mem.run();
    assert_eq!(run.status, GoalStatus::Stalled);
    assert!(
        run.status_reason
            .as_deref()
            .unwrap_or_default()
            .starts_with("no_progress: "),
        "{:?}",
        run.status_reason
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_slice_that_made_progress_resets_the_count_of_empty_ones() {
    let (a, b) = (sha('a'), sha('b'));
    let base = BASE.to_string();
    let mem = seeded(12_000);
    let mut w = SliceWorld::new(
        &mem,
        vec![
            Ends::Handoff(base.clone()),
            Ends::Handoff(base.clone()),
            Ends::Handoff(a.clone()),
            Ends::Handoff(a.clone()),
            Ends::Done(b),
        ],
    );
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 5, "{:?}", out.trace);
    assert_eq!(mem.run().status, GoalStatus::Complete);
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_the_total_time_ends_the_run_paused_and_delivers_what_it_has() {
    let a = sha('a');
    let mem = seeded(3_000);
    let mut w = SliceWorld::new(
        &mem,
        vec![Ends::Handoff(a.clone()), Ends::Deadline(sha('b'))],
    );
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 2, "{:?}", out.trace);
    let bodies = mem.invocations();
    assert_eq!(bodies[0]["slice"]["last"], json!(false));
    assert_eq!(
        bodies[1]["slice"]["last"],
        json!(true),
        "no slice fits after the second"
    );
    let run = mem.run();
    assert_eq!(run.status, GoalStatus::Paused);
    assert_eq!(run.resume_mode, Some(ResumeMode::FollowUp));
    assert!(
        run.status_reason
            .as_deref()
            .unwrap_or_default()
            .starts_with("total_time: "),
        "{:?}",
        run.status_reason
    );
    assert_eq!(
        w.called("packager").len(),
        1,
        "the work is delivered once, at the end"
    );
    let completed = mem.events().pop().unwrap().1;
    assert_eq!(completed["status"], json!("paused"));
    assert_eq!(
        completed["label"],
        json!("Cline stopped: the run reached its time limit")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_a_session_that_cannot_restore_three_times_delivers_the_work_it_was_to_restore() {
    let a = sha('a');
    let mem = seeded(8_000);
    let mut w = SliceWorld::new(
        &mem,
        vec![
            Ends::Handoff(a.clone()),
            Ends::SetupFailed,
            Ends::SetupFailed,
            Ends::SetupFailed,
        ],
    );
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 4, "{:?}", out.trace);
    let run = mem.run();
    assert_eq!(run.status, GoalStatus::Stalled);
    assert!(
        run.status_reason
            .as_deref()
            .unwrap_or_default()
            .starts_with("restore_failed: "),
        "{:?}",
        run.status_reason
    );
    let validate = w.called("bundle-validator")[0];
    assert_eq!(validate["headSha"], json!(a));
    assert_eq!(
        validate["sourceBundleUri"],
        json!(format!("s3://{BUCKET}/{}", s3::bundle_key_t(RUN, 1)))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_next_slice_decides_once_whatever_it_is_asked_again() {
    let a = sha('a');
    let mem = seeded(4_500);
    let mut w = SliceWorld::new(&mem, vec![Ends::Handoff(a.clone()), Ends::Done(sha('b'))]);
    let _ = walk(&direct_definition(), input(), &mut w);
    mem.set_now_ms(mem.now_ms() + 3_600_000);
    let op = Op::NextSlice {
        run_id: RUN.to_string(),
        staged: staged(),
        session: run_session::ops::SessionState::default(),
        loop_state: LoopState::default(),
    };
    let again = handle(&mem.deps(), &op).await.unwrap();
    assert_eq!(again["slice"], json!(2));
    assert_eq!(again["continue"], json!(true));
    let first = mem.blob(&format!("runs/{RUN}/slices/1-end.json")).unwrap();
    let plan2 = mem.blob(&s3::slice_record_key(RUN, 2)).unwrap();
    let again2 = handle(&mem.deps(), &op).await.unwrap();
    assert_eq!(again, again2);
    assert_eq!(
        mem.blob(&format!("runs/{RUN}/slices/1-end.json")).unwrap(),
        first
    );
    assert_eq!(mem.blob(&s3::slice_record_key(RUN, 2)).unwrap(), plan2);
}

/// A turn-complete the way worker-api passes it to `SendTaskSuccess`, for the ops tests that call `next-slice` by hand.
fn session_of(turn: Value) -> run_session::ops::SessionState {
    run_session::ops::SessionState {
        turn: Some(turn),
        failure: None,
    }
}

#[tokio::test]
async fn b688_a_run_that_was_ended_by_someone_else_ends_the_loop_and_keeps_its_status() {
    let mem = seeded(4_500);
    mem.set_run_status_raw(GoalStatus::Cleared);
    let op = Op::NextSlice {
        run_id: RUN.to_string(),
        staged: staged(),
        session: session_of(turn_complete(1, "stopped_timeout", &sha('a'), true)),
        loop_state: LoopState::default(),
    };
    let out = handle(&mem.deps(), &op).await.unwrap();
    assert_eq!(out["continue"], json!(false));
    assert_eq!(out["endReason"], json!("run_ended"));
    assert!(
        mem.blob(&s3::slice_record_key(RUN, 2)).is_none(),
        "no next slice is planned for an ended run"
    );
}

#[tokio::test]
async fn b688_a_run_with_no_base_commit_delivers_what_it_has_instead_of_a_session_it_cannot_check()
{
    // a lost session, and a session that could not restore: neither can start a new session without the commit it must be at
    for (turn, slice, word) in [
        (None, 1, "session_lost"),
        (
            Some(turn_complete(2, "stopped_setup_failed", &sha('a'), false)),
            2,
            "restore_failed",
        ),
    ] {
        let mem = seeded(4_500);
        let op = Op::NextSlice {
            run_id: RUN.to_string(),
            staged: json!({}),
            session: run_session::ops::SessionState {
                turn,
                failure: None,
            },
            loop_state: LoopState {
                slice,
                ..LoopState::default()
            },
        };
        let out = handle(&mem.deps(), &op).await.unwrap();
        assert_eq!(out["continue"], json!(false), "{word}");
        assert_eq!(out["endReason"], json!(word));
        assert!(mem.blob(&s3::slice_record_key(RUN, slice + 1)).is_none());
    }
}

#[tokio::test]
async fn b688_the_head_a_slice_left_is_its_bundle_else_its_last_snapshot_else_nothing() {
    use harness_protocol::api::internal::TurnCompleteRequest;
    use run_session::slices::{RestoreKind, best_head, read_plan};
    let mem = seeded(4_500);
    let (a, b) = (sha('a'), sha('b'));
    let turn: TurnCompleteRequest =
        serde_json::from_value(turn_complete(1, "stopped_timeout", &b, true)).unwrap();
    let head = |t: Option<&TurnCompleteRequest>| {
        let mem = mem.clone();
        let t = t.cloned();
        async move { best_head(&mem, BUCKET, RUN, 1, t.as_ref()).await.unwrap() }
    };
    // nothing saved
    assert!(head(None).await.is_none());
    // a snapshot whose bundle is missing, or whose head is not a commit, or that is not JSON, is not a restore point
    mem.put_blob(
        &s3::snapshot_json_key_t(RUN, 1),
        serde_json::to_vec(&json!({"headSha": a})).unwrap(),
    );
    assert!(head(None).await.is_none(), "no bundle beside the record");
    mem.put_snapshot_t(1, "not-a-commit");
    assert!(head(None).await.is_none(), "{:?}", head(None).await);
    mem.put_blob(&s3::snapshot_json_key_t(RUN, 1), b"{ broken".to_vec());
    assert!(head(None).await.is_none(), "an unreadable record");
    // a good snapshot
    mem.put_snapshot_t(1, &a);
    let r = head(None).await.unwrap();
    assert_eq!(
        (r.kind, r.head.as_deref()),
        (RestoreKind::Snapshot, Some(a.as_str()))
    );
    // a turn-complete that names a bundle which is not there, or is not this slice's key, leaves the snapshot
    assert_eq!(head(Some(&turn)).await.unwrap().kind, RestoreKind::Snapshot);
    mem.put_bundle_t(1);
    let r = head(Some(&turn)).await.unwrap();
    assert_eq!(
        (r.kind, r.head.as_deref()),
        (RestoreKind::Bundle, Some(b.as_str()))
    );
    let mut other = turn_complete(1, "stopped_timeout", &b, true);
    other["bundleUri"] = json!(format!("s3://{BUCKET}/{}", s3::bundle_key_t(RUN, 7)));
    let other: TurnCompleteRequest = serde_json::from_value(other).unwrap();
    assert_eq!(
        head(Some(&other)).await.unwrap().kind,
        RestoreKind::Snapshot
    );
    // a record that is not a plan is an error that names its key, and a missing one is none
    assert!(read_plan(&mem, RUN, 2).await.unwrap().is_none());
    mem.put_blob(&s3::slice_record_key(RUN, 2), b"not json".to_vec());
    let e = read_plan(&mem, RUN, 2).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid");
    assert!(e.to_string().contains(&s3::slice_record_key(RUN, 2)), "{e}");
}

fn plan_for(n: u32) -> Vec<u8> {
    use run_session::slices::{Restore, RestoreKind, SlicePlan};
    serde_json::to_vec_pretty(&SlicePlan {
        slice: n,
        engine_s: SLICE_S,
        last: false,
        restore: Restore {
            kind: RestoreKind::Base,
            key: s3::repo_bundle_key(RUN),
            head: Some(BASE.to_string()),
        },
        base_sha: Some(BASE.to_string()),
        sessions_before: n - 1,
        brief: None,
        planned_at_ms: T0,
    })
    .unwrap()
}

async fn start(mem: &Mem, slice: u32) -> Result<Value, run_session::RunSessionError> {
    let op = Op::StartSession {
        run_id: RUN.to_string(),
        task_token: "task-token".to_string(),
        staged: staged(),
        slice: Some(slice),
    };
    handle(&mem.deps(), &op).await
}

#[tokio::test]
async fn b688_a_slice_starts_only_from_the_agent_item_one_slice_behind_it() {
    let mem = seeded(4_500);
    // a slice number outside 1 to the maximum is refused before anything is read
    for n in [0, harness_protocol::direct::MAX_SLICES + 1] {
        let e = start(&mem, n).await.unwrap_err();
        assert_eq!(e.error_type(), "RunSessionInvalid");
        assert!(e.to_string().contains("is not 1 to"), "{e}");
    }
    start(&mem, 1).await.unwrap();
    assert_eq!(mem.agent().unwrap().session_generation, Some(1));
    // a slice two or more ahead of the item cannot start from it
    mem.put_blob(&s3::slice_record_key(RUN, 3), plan_for(3));
    let e = start(&mem, 3).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid");
    assert!(
        e.to_string().contains("slice 3 cannot start from it"),
        "{e}"
    );
    // an item without its credential binding cannot be moved
    mem.put_blob(&s3::slice_record_key(RUN, 2), plan_for(2));
    let creds = mem
        .0
        .agents
        .lock()
        .unwrap()
        .get_mut("direct")
        .unwrap()
        .creds
        .take();
    let e = start(&mem, 2).await.unwrap_err();
    assert!(e.to_string().contains("no credential binding"), "{e}");
    mem.0
        .agents
        .lock()
        .unwrap()
        .get_mut("direct")
        .unwrap()
        .creds = creds;
    // an item already at the slice must hold the nonce the new session was given
    {
        let mut agents = mem.0.agents.lock().unwrap();
        let a = agents.get_mut("direct").unwrap();
        a.session_generation = Some(2);
        a.nonce_ciphertext = None;
    }
    let e = start(&mem, 2).await.unwrap_err();
    assert!(e.to_string().contains("no nonce ciphertext"), "{e}");
    // a run with no Agent item has nothing to move
    mem.0.agents.lock().unwrap().clear();
    let e = start(&mem, 2).await.unwrap_err();
    assert!(
        e.to_string().contains("no Agent item to move to slice 2"),
        "{e}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b688_each_slice_holds_the_completion_key_made_from_its_own_nonce() {
    use harness_protocol::store::CredsBinding;
    let mem = seeded(4_500);
    let mut w = SliceWorld::new(&mem, vec![Ends::Handoff(sha('a')), Ends::Done(sha('b'))]);
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    // agentd makes the key from the nonce it is started with, so the hash on the Agent item must be that key's hash for the slice that runs
    let bodies = mem.invocations();
    let key_hash = |n: usize| {
        CredsBinding::hash_completion_key(&CredsBinding::completion_key(
            bodies[n]["nonce"].as_str().unwrap(),
        ))
    };
    assert_ne!(key_hash(0), key_hash(1), "each slice has its own nonce");
    let writes = mem.0.agent_writes.lock().unwrap().clone();
    assert_eq!(
        writes.len(),
        2,
        "the item was written for slice 1 and rotated for slice 2"
    );
    for (i, item) in writes.iter().enumerate() {
        assert_eq!(
            item.creds
                .as_ref()
                .unwrap()
                .completion_key_sha256
                .as_deref(),
            Some(key_hash(i).as_str()),
            "slice {}",
            i + 1
        );
    }
}

#[tokio::test]
async fn b688_an_agent_item_written_before_the_completion_key_keeps_naming_none_in_the_next_slice()
{
    let mem = seeded(4_500);
    start(&mem, 1).await.unwrap();
    mem.0
        .agents
        .lock()
        .unwrap()
        .get_mut("direct")
        .unwrap()
        .creds
        .as_mut()
        .unwrap()
        .completion_key_sha256 = None;
    mem.put_blob(&s3::slice_record_key(RUN, 2), plan_for(2));
    start(&mem, 2).await.unwrap();
    let item = mem.agent().unwrap();
    assert_eq!(item.session_generation, Some(2));
    assert_eq!(item.creds.unwrap().completion_key_sha256, None);
}
