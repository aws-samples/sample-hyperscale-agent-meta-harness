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
    /// The commit the staging fake reports as the base (a follow-up run is staged at the head an earlier run delivered).
    base: String,
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
            base: BASE.to_string(),
        }
    }

    /// The same world with its staging fake reporting `base` as the commit the run starts from.
    fn staged_at(mut self, base: &str) -> SliceWorld {
        self.base = base.to_string();
        self
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
                    GatewayRequest::Stage(_) => {
                        let mut v = staged();
                        v["baseSha"] = json!(self.base);
                        Ok(v)
                    }
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

// ---- to: sessions of the shortest length -------------------------------------------------------------------------------
//
// These walks run the real state machine at that limit over the same fakes, on the fake clock, so what the live run will do is known before it starts.
// A session costs its 60 s and two minutes of end sequence (`END_S`), so the sessions of a total are fixed by the arithmetic of `plan_timing`: slice k
// is planned with `total - 180 * (k - 1)` seconds left, gets 60 s, and is the last when less than 960 s would be left after it and its reserve.

/// The limit of the short live stage.
const SHORT_S: u32 = 60;
/// The run a follow-up continues.
const FIRST_RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B8";

fn seeded_short(max_run_s: Option<u32>) -> Mem {
    let mem = Mem::default();
    mem.set_now_ms(T0);
    mem.set_slice_deadline(SHORT_S);
    let members = max_run_s.map_or_else(|| json!({}), |s| json!({"maxRunS": s}));
    mem.seed_with_members(
        RunTarget::S3Only,
        "Keep adding a tick line to PROGRESS.md",
        MODEL,
        members,
    );
    mem
}

/// The commit session `n` ends on.
fn head_of(n: u32) -> String {
    format!("{:040x}", 0x00c0_de00_u64 + u64::from(n))
}

fn plan_of(mem: &Mem, n: u32) -> run_session::slices::SlicePlan {
    serde_json::from_slice(&mem.blob(&s3::slice_record_key(RUN, n)).unwrap()).unwrap()
}

fn end_of(mem: &Mem, n: u32) -> run_session::slices::SliceEnd {
    serde_json::from_slice(
        &mem.blob(&format!("runs/{RUN}/slices/{n}-end.json"))
            .unwrap(),
    )
    .unwrap()
}

/// Four hand-overs and a fifth session that reports done.
fn five_sessions() -> Vec<Ends> {
    vec![
        Ends::Handoff(head_of(1)),
        Ends::Handoff(head_of(2)),
        Ends::Handoff(head_of(3)),
        Ends::Handoff(head_of(4)),
        Ends::Done(head_of(5)),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn b773_five_sixty_second_sessions_hand_over_four_times_and_each_restores_the_head_the_one_before_it_ended_on()
 {
    let mem = seeded_short(Some(2_500));
    let mut w = SliceWorld::new(&mem, five_sessions());
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 5, "{:?}", out.trace);
    assert_eq!(count(&out.trace, "NextSlice"), 5);
    assert_eq!(count(&out.trace, "ResetSession"), 4, "four hand-overs");
    assert_eq!(count(&out.trace, "Finalize"), 1);
    let bodies = mem.invocations();
    assert_eq!(bodies.len(), 5);
    for k in 2..=5u32 {
        let (plan, before) = (plan_of(&mem, k), end_of(&mem, k - 1));
        assert_eq!(
            plan.restore.head.as_deref(),
            Some(head_of(k - 1).as_str()),
            "session {k} restores the head session {} ended on",
            k - 1
        );
        assert_eq!(before.head.as_deref(), Some(head_of(k - 1).as_str()));
        assert!(before.cont, "the end record of session {} goes on", k - 1);
        let body = &bodies[usize::try_from(k - 1).unwrap()];
        assert_eq!(body["slice"]["restoreHead"], json!(head_of(k - 1)));
        assert_eq!(
            body["repo"]["uri"],
            json!(format!("s3://{BUCKET}/{}", s3::bundle_key_t(RUN, k - 1))),
            "session {k} restores the bundle of session {}",
            k - 1
        );
        assert_eq!(
            body["slice"]["baseSha"],
            json!(BASE),
            "the base never moves"
        );
    }
    assert!(!end_of(&mem, 5).cont);
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert_eq!(mem.run().slice, Some(5));
}

#[tokio::test(flavor = "multi_thread")]
async fn b773_the_five_sessions_deliver_once_and_the_delivered_head_is_the_head_the_last_one_ended_on()
 {
    let mem = seeded_short(Some(2_500));
    let mut w = SliceWorld::new(&mem, five_sessions());
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(w.called("packager").len(), 1, "one delivery for the run");
    assert_eq!(w.called("bundle-validator").len(), 1);
    let validate = w.called("bundle-validator")[0];
    assert_eq!(validate["headSha"], json!(head_of(5)));
    assert_eq!(validate["baseSha"], json!(BASE));
    assert_eq!(
        validate["sourceBundleUri"],
        json!(format!("s3://{BUCKET}/{}", s3::bundle_key_t(RUN, 5))),
        "the delivery reads the bundle of the last session"
    );
    assert_eq!(end_of(&mem, 5).head.as_deref(), Some(head_of(5).as_str()));
    let types = mem.event_types();
    assert_eq!(types.iter().filter(|t| *t == "run.completed").count(), 1);
    assert_eq!(types.iter().filter(|t| *t == "delivery.ready").count(), 1);
    let completed = mem.events().pop().unwrap().1;
    assert!(
        completed["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("5 sessions"),
        "{completed}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b773_every_session_has_its_own_machine_session_id_nonce_and_turn_and_the_one_before_it_is_stopped()
 {
    let mem = seeded_short(Some(2_500));
    let mut w = SliceWorld::new(&mem, five_sessions());
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(w.started.len(), 5);
    let mut ids: Vec<&String> = w.started.iter().map(|(_, s)| s).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 5, "no session id is used twice");
    let mut nonces = w.nonces.clone();
    nonces.sort();
    nonces.dedup();
    assert_eq!(nonces.len(), 5, "every session has its own refresh nonce");
    let stopped: Vec<String> = mem.stops().into_iter().map(|(_, s)| s).collect();
    for (n, id) in &w.started {
        assert!(stopped.contains(id), "session {n} ({id}) was stopped");
        assert!(
            mem.turn_n(*n).is_some(),
            "session {n} has its own Turn item"
        );
    }
    let tokens: Vec<String> = (1..=5)
        .map(|n| mem.turn_n(n).unwrap().task_token_ciphertext)
        .collect();
    let mut distinct = tokens.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 5, "every Turn holds its own task token");
}

#[tokio::test(flavor = "multi_thread")]
async fn b773_every_session_is_planned_with_sixty_seconds_and_only_the_fifth_is_the_last_and_each_costs_three_minutes_of_the_total()
 {
    let mem = seeded_short(Some(2_500));
    let mut w = SliceWorld::new(&mem, five_sessions());
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    let bodies = mem.invocations();
    for k in 1..=5u32 {
        let plan = plan_of(&mem, k);
        assert_eq!(plan.engine_s, SHORT_S, "plan {k}");
        assert_eq!(plan.last, k == 5, "plan {k}");
        assert_eq!(plan.sessions_before, k - 1);
        // a session takes its 60 s and the two minutes of its end sequence, and both count against the total
        assert_eq!(
            plan.planned_at_ms,
            T0 + u64::from(k - 1) * (u64::from(SHORT_S) + END_S) * 1000,
            "plan {k}"
        );
        let body = &bodies[usize::try_from(k - 1).unwrap()];
        assert_eq!(body["timeoutS"], json!(SHORT_S));
        assert_eq!(body["slice"]["last"], json!(k == 5));
    }
    assert!(
        mem.blob(&s3::slice_record_key(RUN, 6)).is_none(),
        "no sixth session is planned"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b773_each_session_asks_for_a_package_token_that_runs_from_its_own_start_not_the_runs() {
    let mem = seeded_short(Some(2_500));
    let mut w = SliceWorld::new(&mem, five_sessions());
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    let bodies = mem.invocations();
    let mut last = 0;
    for (i, body) in bodies.iter().enumerate() {
        let asked_at = plan_of(&mem, u32::try_from(i + 1).unwrap()).planned_at_ms / 1000;
        let expires = body["packagesTokenExpiresAt"].as_u64().unwrap();
        let timeout = body["timeoutS"].as_u64().unwrap();
        assert!(
            expires >= asked_at + timeout,
            "session {}: the token ends at {expires}, before the session ends at {}",
            i + 1,
            asked_at + timeout
        );
        // the whole session plus agentd's margin, within CodeArtifact's least of 900 s
        assert_eq!(expires - asked_at, 900, "session {}", i + 1);
        assert!(expires > last, "each token ends later than the one before");
        last = expires;
    }
    let first = bodies[0]["packagesTokenExpiresAt"].as_u64().unwrap();
    assert_eq!(
        last - first,
        4 * (u64::from(SHORT_S) + END_S),
        "the fifth token starts four sessions of three minutes after the first"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b773_a_session_lost_among_five_restarts_from_its_snapshot_and_the_chain_of_heads_goes_on()
{
    let mem = seeded_short(Some(3_000));
    let mut w = SliceWorld::new(
        &mem,
        vec![
            Ends::Handoff(head_of(1)),
            Ends::Lost(Some(head_of(2))),
            Ends::Handoff(head_of(3)),
            Ends::Handoff(head_of(4)),
            Ends::Done(head_of(5)),
        ],
    );
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 5, "{:?}", out.trace);
    let kinds: Vec<_> = (2..=5).map(|k| plan_of(&mem, k).restore).collect();
    assert_eq!(kinds[0].head.as_deref(), Some(head_of(1).as_str()));
    assert_eq!(
        kinds[1].kind,
        run_session::slices::RestoreKind::Snapshot,
        "the session after the lost one restores the lost one's snapshot"
    );
    assert_eq!(kinds[1].head.as_deref(), Some(head_of(2).as_str()));
    assert_eq!(kinds[2].head.as_deref(), Some(head_of(3).as_str()));
    assert_eq!(kinds[3].head.as_deref(), Some(head_of(4).as_str()));
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert_eq!(w.called("packager").len(), 1);
    assert_eq!(
        w.called("bundle-validator")[0]["headSha"],
        json!(head_of(5))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b773_with_sixty_second_sessions_the_last_session_the_cap_allows_ends_the_run_however_much_of_the_day_is_left()
 {
    let mem = seeded_short(None);
    let script: Vec<Ends> = (1..=harness_protocol::direct::MAX_SLICES)
        .map(|n| Ends::Handoff(head_of(n)))
        .collect();
    let mut w = SliceWorld::new(&mem, script);
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    let cap = usize::try_from(harness_protocol::direct::MAX_SLICES).unwrap();
    assert_eq!(
        count(&out.trace, "StartSession"),
        cap,
        "{:?} {:?} {:?}",
        mem.run().status_reason,
        mem.run().status,
        end_of(
            &mem,
            u32::try_from(count(&out.trace, "StartSession")).unwrap()
        )
    );
    assert!(
        mem.blob(&s3::slice_record_key(
            RUN,
            harness_protocol::direct::MAX_SLICES + 1
        ))
        .is_none(),
        "no session is planned past the cap"
    );
    let run = mem.run();
    assert_eq!(run.status, GoalStatus::Paused);
    assert!(
        run.status_reason
            .as_deref()
            .unwrap_or_default()
            .starts_with("total_time: "),
        "{:?}",
        run.status_reason
    );
    let end = end_of(&mem, harness_protocol::direct::MAX_SLICES);
    assert!(!end.cont);
    assert_eq!(
        end.head.as_deref(),
        Some(head_of(harness_protocol::direct::MAX_SLICES).as_str())
    );
    assert_eq!(w.called("packager").len(), 1, "the work is delivered once");
    // Step Functions keeps at most 25,000 events of one execution; a state writes at most six (entered, scheduled, started, succeeded, exited, a retry)
    assert!(
        out.trace.len() * 6 < 25_000,
        "{} states walked, {} events at most",
        out.trace.len(),
        out.trace.len() * 6
    );
    // less than the 24 hours of the default total went by: the most sessions the cap allows, of three minutes each
    let spent_s = (mem.now_ms() - T0) / 1000;
    assert_eq!(
        spent_s,
        u64::from(harness_protocol::direct::MAX_SLICES) * (u64::from(SHORT_S) + END_S)
    );
    assert!(spent_s < 24 * 3600 * 3 / 5, "most of the day is left");
}

// The total time reached

#[tokio::test(flavor = "multi_thread")]
async fn b772_the_least_total_the_service_takes_gives_one_sixty_second_session_that_is_the_last() {
    let mem = seeded_short(Some(harness_protocol::direct::MIN_MAX_RUN_S));
    let mut w = SliceWorld::new(&mem, vec![Ends::Deadline(head_of(1))]);
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 1, "{:?}", out.trace);
    let plan = plan_of(&mem, 1);
    assert_eq!(plan.engine_s, SHORT_S);
    assert!(
        plan.last,
        "the reserve and the least session leave room for no second one"
    );
    assert!(
        mem.blob(&s3::slice_record_key(RUN, 2)).is_none(),
        "no second session is planned"
    );
    let body = &mem.invocations()[0];
    assert_eq!(body["timeoutS"], json!(SHORT_S));
    // a run of one session is told nothing about hand-overs: agentd ends it at its deadline with no hand-over
    assert!(body.get("slice").is_none_or(Value::is_null), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b772_the_run_ends_paused_at_its_total_time_and_the_record_says_why() {
    let mem = seeded_short(Some(harness_protocol::direct::MIN_MAX_RUN_S));
    let mut w = SliceWorld::new(&mem, vec![Ends::Deadline(head_of(1))]);
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    let end = end_of(&mem, 1);
    assert_eq!(end.kind, run_session::slices::EndKind::Deadline);
    assert!(!end.cont);
    assert_eq!(
        end.end_reason,
        Some(run_session::slices::EndReason::TotalTime)
    );
    assert_eq!(end.head.as_deref(), Some(head_of(1).as_str()));
    let run = mem.run();
    assert_eq!(run.status, GoalStatus::Paused);
    assert_eq!(run.resume_mode, Some(ResumeMode::FollowUp));
    assert_eq!(run.slice, Some(1));
    assert!(
        run.status_reason
            .as_deref()
            .unwrap_or_default()
            .starts_with("total_time: "),
        "{:?}",
        run.status_reason
    );
    let completed = mem.events().pop().unwrap().1;
    assert_eq!(completed["status"], json!("paused"));
    assert_eq!(
        completed["label"],
        json!("Cline stopped: the run reached its time limit")
    );
    let types = mem.event_types();
    assert_eq!(types.iter().filter(|t| *t == "run.completed").count(), 1);
    assert_eq!(types.iter().filter(|t| *t == "delivery.ready").count(), 1);
    assert_eq!(
        w.called("packager").len(),
        1,
        "what the one session did is delivered"
    );
    assert_eq!(
        w.called("bundle-validator")[0]["headSha"],
        json!(head_of(1))
    );
}

/// The lines of PROGRESS.md at each commit a scripted run produces: a session adds one line to the file it restored.
fn progress_lines(restored: &[String], session: u32) -> Vec<String> {
    let mut lines = restored.to_vec();
    lines.push(format!("tick {session}"));
    lines
}

#[tokio::test(flavor = "multi_thread")]
async fn b772_the_follow_up_starts_from_the_delivered_head_completes_and_keeps_the_first_runs_progress_lines()
 {
    use std::collections::BTreeMap;
    let mut at: BTreeMap<String, Vec<String>> = BTreeMap::new();
    at.insert(BASE.to_string(), vec![]);
    // the first run: one session, cut at its deadline, with its line committed
    let first = seeded_short(Some(harness_protocol::direct::MIN_MAX_RUN_S));
    let mut w1 = SliceWorld::new(&first, vec![Ends::Deadline(head_of(1))]);
    let out = walk(&direct_definition(), input(), &mut w1);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(first.run().status, GoalStatus::Paused);
    let delivered = w1.called("bundle-validator")[0]["headSha"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(delivered, head_of(1));
    at.insert(delivered.clone(), progress_lines(&at[BASE], 1));

    // the follow-up: a new run whose request names the first run and the head it delivered; the stage stages that head as the base
    let second = Mem::default();
    second.set_now_ms(T0);
    second.set_slice_deadline(SHORT_S);
    second.seed_with_members(
        RunTarget::S3Only,
        "Keep adding a tick line to PROGRESS.md\n\n---\nThis is a continuation.",
        MODEL,
        json!({"maxRunS": harness_protocol::direct::MIN_MAX_RUN_S,
               "continueFrom": {"runId": FIRST_RUN, "headSha": delivered}}),
    );
    // the first run is a run of the same owner: the follow-up's Prepare checks that before it asks the gateway for the delivered head
    let mut parent = second.run();
    parent.run_id = FIRST_RUN.to_string();
    parent.continue_from = None;
    parent.status = GoalStatus::Paused;
    second
        .0
        .runs
        .lock()
        .unwrap()
        .insert(FIRST_RUN.to_string(), parent);
    second.set_continue_from(FIRST_RUN);
    let mut w2 = SliceWorld::new(&second, vec![Ends::Done(head_of(2))]).staged_at(&delivered);
    let out = walk(&direct_definition(), input(), &mut w2);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(
        second.run().status,
        GoalStatus::Complete,
        "the follow-up completes: {:?} {:?} {:?}",
        second.run().status_reason,
        out.trace,
        second.events()
    );
    let staged = w2.called("repo-gateway")[0];
    assert_eq!(
        staged["fromRun"],
        json!({"runId": FIRST_RUN, "headSha": delivered}),
        "the gateway is asked for the code the first run delivered"
    );
    let plan = plan_of(&second, 1);
    assert_eq!(plan.base_sha.as_deref(), Some(delivered.as_str()));
    let validate = w2.called("bundle-validator")[0];
    assert_eq!(
        validate["baseSha"],
        json!(delivered),
        "the follow-up is measured from the delivered head"
    );
    assert_eq!(validate["headSha"], json!(head_of(2)));
    assert_eq!(w2.called("packager").len(), 1);
    // the session started on the delivered head, so its commit holds the first run's lines and one more
    let restored = at.get(plan.base_sha.as_deref().unwrap()).unwrap().clone();
    at.insert(head_of(2), progress_lines(&restored, 2));
    assert_eq!(
        at[&head_of(2)],
        vec!["tick 1".to_string(), "tick 2".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b772_a_follow_up_whose_stage_did_not_hold_the_delivered_head_never_starts_a_session() {
    let mem = Mem::default();
    mem.set_now_ms(T0);
    mem.set_slice_deadline(SHORT_S);
    mem.seed_with_members(
        RunTarget::S3Only,
        "Keep adding a tick line to PROGRESS.md",
        MODEL,
        json!({"maxRunS": harness_protocol::direct::MIN_MAX_RUN_S,
               "continueFrom": {"runId": FIRST_RUN, "headSha": head_of(1)}}),
    );
    // the staged base is the original one, not the head the first run delivered: the work would be lost
    let err = start(&mem, 1).await.unwrap_err();
    assert_eq!(err.error_type(), "RunSessionInvalid");
    assert!(
        err.to_string()
            .contains("is not the head the earlier run delivered"),
        "{err}"
    );
    assert!(mem.invocations().is_empty(), "no session was invoked");
}

#[test]
fn b773_the_run_machine_outlasts_the_longest_total_time_and_the_session_wait_outlasts_the_longest_session()
 {
    use harness_protocol::direct::MAX_MAX_RUN_S;
    let def = direct_definition();
    // the whole execution: the longest total the service accepts, and the end sequence of its last session
    let machine = def["TimeoutSeconds"].as_u64().unwrap();
    assert!(
        machine >= u64::from(MAX_MAX_RUN_S) + 3_600,
        "the run machine times out after {machine} s, the longest total is {MAX_MAX_RUN_S} s"
    );
    // one session: the longest prompt the stage allows (the 28,800 s lifetime less the reserve) and the time to report it; and the heartbeat that finds a lost one
    let start = &def["States"]["StartSession"];
    let longest = u64::from(run_session::slices::slice_cap_s(28_800, None));
    let wait = start["TimeoutSeconds"].as_u64().unwrap();
    assert!(
        wait >= longest + 300,
        "the wait for a session ends after {wait} s, a session may run {longest} s"
    );
    assert_eq!(start["HeartbeatSeconds"], json!(600));
    // a stage with the shortest session limit asks for the same machine: the limit changes the prompt deadline, not these times
    assert_eq!(
        run_session::slices::slice_cap_s(28_800, Some(SHORT_S)),
        SHORT_S
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b773_a_sixty_second_session_lost_before_any_snapshot_restarts_from_the_last_bundle() {
    // A session lost before its first snapshot has none: the next session restores the bundle of the session before the lost one,
    // and only the lost session's own minute is gone.
    let mem = seeded_short(Some(3_000));
    let mut w = SliceWorld::new(
        &mem,
        vec![
            Ends::Handoff(head_of(1)),
            Ends::Lost(None),
            Ends::Handoff(head_of(3)),
            Ends::Handoff(head_of(4)),
            Ends::Done(head_of(5)),
        ],
    );
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(count(&out.trace, "StartSession"), 5, "{:?}", out.trace);
    let third = plan_of(&mem, 3).restore;
    assert_eq!(third.kind, run_session::slices::RestoreKind::Bundle);
    assert_eq!(third.head.as_deref(), Some(head_of(1).as_str()));
    assert_eq!(third.key, s3::bundle_key_t(RUN, 1));
    // the lost session cost the run its engine time and the 600 s heartbeat; the chain goes on from session 3
    assert_eq!(
        plan_of(&mem, 4).restore.head.as_deref(),
        Some(head_of(3).as_str())
    );
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert_eq!(
        w.called("bundle-validator")[0]["headSha"],
        json!(head_of(5))
    );
}
