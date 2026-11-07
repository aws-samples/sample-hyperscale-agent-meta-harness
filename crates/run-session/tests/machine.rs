//! The real `statemachines/direct.asl.json` walked over in-process fakes of its four Lambdas: run-session (the real
//! ops over the in-memory ports) and stand-ins for repo-gateway, bundle-validator and packager that parse every payload
//! into the real request types of the delivery Lambdas. Paths: (a) reported done with a pull request, (b) `--s3-only`,
//! (c) a new repository, (d) a lost session with a snapshot, (e) the same without one, (f) a failed validation,
//! (g) a failed pull request push, (h) a failed packager, (i) a missed heartbeat the way Step Functions names it, `States.Timeout`;
//! plus the retried `waitForTaskToken` state.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::gateway::GatewayRequest;
use delivery::packager::PackageRequest;
use delivery::validator::ValidateRequest;
use harness_protocol::direct::{DirectOutcome, banned_claim_word};
use harness_protocol::goal::GoalStatus;
use harness_protocol::target::RunTarget;
use run_session::lambda::serve_event;
use run_session::ops::{Op, handle};
use run_session::plan_delivery::SESSION_TASK_TIMEOUT_S;
use serde_json::{Value, json};
use support::asl::{Lambdas, Wait, direct_definition, walk};
use support::fakes::*;

const PR: &str = "https://us-east-1.console.aws.amazon.com/codesuite/codecommit/repositories/hamh-e2e-python-calc/pull-requests/1/details";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ending {
    /// agentd finished: summary, bundle and turn-complete.
    Done(DirectOutcome),
    /// No heartbeat for 600 s; a snapshot exists when `bool`.
    Lost(bool),
    /// The task limit of 28,500 s.
    TimedOut,
    /// A missed heartbeat as Step Functions reports it on the live stage: `States.Timeout`, well before the limit; a snapshot
    /// exists when `bool`.
    MissedHeartbeat(bool),
}

struct World {
    mem: Mem,
    ending: Ending,
    /// Calls the delivery fakes received, `(function, payload)`.
    calls: Vec<(String, Value)>,
    validation_fails: bool,
    /// The bundle-validator role has no grant for something the validator reads: the call fails with `AccessDenied`.
    validation_denied: Option<String>,
    pr_fails: bool,
    /// The repo-gateway refuses `push-pr` with this reason (a delivery conflict of a follow-up run).
    pr_refusal: Option<String>,
    packager_fails: bool,
    /// Fail the first `n` start-session invocations with this error type.
    start_failures: u32,
    /// The token of each start-session attempt.
    tokens: Vec<String>,
    /// The function cannot answer the calls with these indexes (0 is its first call of the execution): `(range, errorType,
    /// errorMessage)`, as Lambda reports an init failure.
    dead: Option<(std::ops::Range<u32>, String, String)>,
    /// Calls the run-session function has received.
    rs_calls: u32,
    /// The asynchronous (`Event`) calls the function accepted, in order. They are not in `rs_calls`: a call the machine does not
    /// wait for must not move the index of the calls the kill tests count.
    events: Vec<Value>,
    /// The error names the next asynchronous calls fail with (the call itself failing, as Lambda reports it), one per attempt.
    event_faults: std::collections::VecDeque<String>,
    /// Every asynchronous call fails with this error.
    event_dead: Option<String>,
    /// What each accepted event did when the function ran it: its answer, or the error text. The machine never sees this.
    event_results: Vec<Result<Value, String>>,
    builder: tokio::runtime::Handle,
}

impl World {
    fn new(mem: &Mem, ending: Ending) -> World {
        World {
            mem: mem.clone(),
            ending,
            calls: vec![],
            validation_fails: false,
            validation_denied: None,
            pr_fails: false,
            pr_refusal: None,
            packager_fails: false,
            start_failures: 0,
            tokens: vec![],
            dead: None,
            rs_calls: 0,
            events: vec![],
            event_faults: Default::default(),
            event_dead: None,
            event_results: vec![],
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

impl Lambdas for World {
    fn invoke(&mut self, function: &str, payload: Value) -> Result<Value, (String, String)> {
        match function {
            "run-session" => {
                let call = self.rs_calls;
                self.rs_calls += 1;
                if let Some((range, error, message)) = &self.dead
                    && range.contains(&call)
                {
                    return Err((error.clone(), message.clone()));
                }
                let op: Op = serde_json::from_value(payload.clone())
                    .map_err(|e| ("RunSessionInvalid".into(), e.to_string()))?;
                if let Op::StartSession { task_token, .. } = &op {
                    self.tokens.push(task_token.clone());
                    if self.start_failures > 0 {
                        self.start_failures -= 1;
                        return Err(("RunSessionUnavailable".into(), "AgentCore 503".into()));
                    }
                }
                let deps = self.mem.deps();
                let h = self.builder.clone();
                tokio::task::block_in_place(|| h.block_on(handle(&deps, &op)))
                    .map_err(|e| (e.error_type().to_string(), e.to_string()))
            }
            "repo-gateway" => {
                let req: GatewayRequest = serde_json::from_value(payload.clone())
                    .expect("the gateway parses the payload");
                self.calls.push((function.into(), payload));
                match req {
                    // The real gateway stages the delivered head as the base of a follow-up run (`stage_from_run`).
                    GatewayRequest::Stage(r) => Ok(match r.from_run {
                        Some(from) => {
                            let mut st = staged();
                            st["baseSha"] = json!(from.head_sha);
                            st
                        }
                        None => staged(),
                    }),
                    GatewayRequest::NewRepo(_) => Ok(json!({"repo": "hamh-e2e-new-20261013"})),
                    GatewayRequest::PushPr(_) if self.pr_fails => {
                        Err(("Refused".into(), "refused: pull request".into()))
                    }
                    GatewayRequest::PushPr(_) if self.pr_refusal.is_some() => Err((
                        "Refused".into(),
                        self.pr_refusal.clone().unwrap_or_default(),
                    )),
                    GatewayRequest::PushPr(_) => Ok(json!({"url": PR})),
                }
            }
            "bundle-validator" => {
                serde_json::from_value::<ValidateRequest>(payload.clone())
                    .expect("the validator parses the payload");
                self.calls.push((function.into(), payload));
                if let Some(message) = &self.validation_denied {
                    return Err(("AccessDenied".into(), message.clone()));
                }
                if self.validation_fails {
                    Ok(
                        json!({"pass": false, "violations": [{"rule": "secret", "path": "src/a.py", "detail": "gitleaks rule aws-access-token"}]}),
                    )
                } else {
                    Ok(json!({"pass": true, "violations": []}))
                }
            }
            "packager" => {
                let req: PackageRequest = serde_json::from_value(payload.clone())
                    .expect("the packager parses the payload");
                self.calls.push((function.into(), payload));
                if self.packager_fails {
                    return Err(("Invalid".into(), "no source bundle".into()));
                }
                Ok(
                    json!({"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096, "prUrl": req.pr_url}),
                )
            }
            other => panic!("unknown function {other}"),
        }
    }

    fn event(&mut self, function: &str, payload: Value) -> Result<(), (String, String)> {
        assert_eq!(
            function, "run-session",
            "only run-session is called asynchronously"
        );
        let fault = self
            .event_faults
            .pop_front()
            .or_else(|| self.event_dead.clone());
        if let Some(error) = fault {
            return Err((error.clone(), format!("{error}: the call failed")));
        }
        self.events.push(payload.clone());
        // Lambda accepts the event and answers 202 before the function runs; what the function then does is the function's own.
        let op: Op = serde_json::from_value(payload).expect("the Name state sends an op");
        let deps = self.mem.deps();
        let h = self.builder.clone();
        let result = tokio::task::block_in_place(|| h.block_on(handle(&deps, &op)))
            .map_err(|e| e.to_string());
        self.event_results.push(result);
        Ok(())
    }

    fn wait(&mut self, _function: &str, token: &str) -> Wait {
        // The session this wait follows: the turn of the last invocation (a lost session is replayed as the next turn of the run).
        let n = self
            .mem
            .invocations()
            .last()
            .and_then(|b| b["turn"].as_u64())
            .map_or(1, |t| u32::try_from(t).unwrap());
        let ct = self.mem.turn_n(n).unwrap().task_token_ciphertext;
        assert!(
            ct.contains(token),
            "worker-api completes the token the Turn item holds ({ct}); this attempt's is {token}"
        );
        match self.ending {
            Ending::Done(outcome) => {
                self.mem.put_bundle_t(n);
                self.mem
                    .put_summary_t(n, &summary(outcome, Some("Added pct(a, b)."), true));
                Wait::Success(turn_output(true))
            }
            Ending::Lost(snapshot) => {
                if snapshot {
                    self.mem
                        .put_snapshot_t(n, "3333333333333333333333333333333333333333");
                }
                Wait::HeartbeatTimeout
            }
            Ending::TimedOut => {
                // The task limit comes 28,500 s after the task started: the Turn item says when it did.
                self.mem
                    .0
                    .turns
                    .lock()
                    .unwrap()
                    .get_mut(&("direct".to_string(), n))
                    .unwrap()
                    .started_at_ms = Some(T0 - SESSION_TASK_TIMEOUT_S * 1000);
                Wait::Timeout
            }
            Ending::MissedHeartbeat(snapshot) => {
                if snapshot {
                    self.mem
                        .put_snapshot_t(n, "3333333333333333333333333333333333333333");
                }
                Wait::Timeout
            }
        }
    }
}

fn input() -> Value {
    json!({"runId": RUN})
}

fn seeded(target: RunTarget) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    mem
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reported_done_run_with_a_pull_request_completes_with_the_label() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(
        out.trace,
        [
            "Name",
            "Prepare",
            "NeedsRepo",
            "StageRepo",
            "Init",
            "StartSession",
            "NextSlice",
            "SliceChoice",
            "Plan",
            "HasWork",
            "ValidateBundle",
            "ValidationOutcome",
            "HasPr",
            "PushPullRequest",
            "WithPrUrl",
            "Package",
            "Finalize"
        ]
    );
    assert!(
        out.retries.is_empty() && out.caught.is_empty(),
        "{:?} {:?}",
        out.retries,
        out.caught
    );
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert_eq!(
        mem.event_types(),
        [
            "goal.status",
            "session.started",
            "goal.status",
            "delivery.pr",
            "delivery.ready",
            "goal.status",
            "run.completed"
        ]
    );
    let completed = mem.events().pop().unwrap().1;
    assert_eq!(completed["label"], "Cline reported done");
    // the pull request URL reaches the packager through JsonMerge
    let pkg: PackageRequest = serde_json::from_value(w.called("packager")[0].clone()).unwrap();
    assert_eq!(pkg.pr_url.as_deref(), Some(PR));
    assert_eq!(pkg.direct.label, "Cline reported done");
    let pr = w
        .called("repo-gateway")
        .into_iter()
        .find(|v| v["operation"] == "push-pr")
        .unwrap()
        .clone();
    assert_eq!(pr["title"], "ham: Add percent_change(a, b) with tests");
    assert_eq!(banned_claim_word(pr["description"].as_str().unwrap()), None);
    // `NextSlice` stops the machine of the session that ended before it decides, and the end of the run stops it again (a repeat is safe).
    assert_eq!(mem.stops().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_s3_only_run_has_no_pull_request() {
    let mem = seeded(RunTarget::S3Only);
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(
        out.trace.contains(&"NoPr".to_string())
            && !out.trace.contains(&"PushPullRequest".to_string()),
        "{:?}",
        out.trace
    );
    assert_eq!(out.trace.last().unwrap(), "Finalize");
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert!(!mem.event_types().contains(&"delivery.pr".to_string()));
    let pkg: PackageRequest = serde_json::from_value(w.called("packager")[0].clone()).unwrap();
    assert_eq!(pkg.pr_url, None);
    assert_eq!(pkg.repo, "s3-only");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_repository_is_created_before_it_is_staged() {
    let mem = seeded(
        serde_json::from_value(json!({"mode": "new-repo", "repo": "cc:hamh-e2e-new-20261013"}))
            .unwrap(),
    );
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(
        &out.trace[..6],
        [
            "Name",
            "Prepare",
            "NeedsRepo",
            "CreateRepo",
            "StageRepo",
            "Init"
        ]
    );
    let ops: Vec<&str> = w
        .called("repo-gateway")
        .iter()
        .map(|v| v["operation"].as_str().unwrap())
        .collect();
    assert_eq!(ops, ["new-repo", "stage", "push-pr"]);
    assert_eq!(mem.run().status, GoalStatus::Complete);
}

/// The states a run walks to the end of its third lost session in a row: each lost session is replayed (`NextSlice` decides, `SliceChoice` sends the run
/// through `ResetSession` to `StartSession` again) until the third, and then the run goes on to `Plan`.
fn three_lost_sessions() -> Vec<&'static str> {
    let mut want = vec!["Name", "Prepare", "NeedsRepo", "StageRepo", "Init"];
    for _ in 0..2 {
        want.extend(["StartSession", "NextSlice", "SliceChoice", "ResetSession"]);
    }
    want.extend(["StartSession", "NextSlice", "SliceChoice", "Plan"]);
    want
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_session_with_a_snapshot_still_delivers() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Lost(true));
    let out = walk(&direct_definition(), input(), &mut w);
    // Each of the three sessions is lost; the third ends the run, and the snapshot it saved is what is delivered.
    assert_eq!(
        out.caught,
        vec![
            (
                "StartSession".to_string(),
                "States.HeartbeatTimeout".to_string()
            );
            3
        ]
    );
    assert_eq!(
        out.trace[..three_lost_sessions().len()],
        three_lost_sessions()[..]
    );
    assert_eq!(out.trace.last().unwrap(), "Finalize");
    assert_eq!(mem.run().status, GoalStatus::Stalled);
    let completed = mem.events().pop().unwrap().1;
    assert_eq!(completed["status"], "stalled");
    assert_eq!(completed["outcome"], "stopped_session_lost");
    assert_eq!(completed["label"], "Cline stopped: the session was lost");
    let v: ValidateRequest =
        serde_json::from_value(w.called("bundle-validator")[0].clone()).unwrap();
    assert!(v.source_bundle_uri.unwrap().ends_with("/snapshot.bundle"));
    let pkg: PackageRequest = serde_json::from_value(w.called("packager")[0].clone()).unwrap();
    assert_eq!(pkg.head_sha, "3333333333333333333333333333333333333333");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timed_out_session_delivers_what_it_saved_with_the_timeout_label() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::TimedOut);
    mem.put_snapshot("3333333333333333333333333333333333333333");
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.caught[0].1, "States.Timeout");
    assert_eq!(
        mem.events().pop().unwrap().1["label"],
        "Cline stopped: the run reached its time limit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missed_heartbeat_named_states_timeout_delivers_the_snapshot_as_a_lost_session() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::MissedHeartbeat(true));
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.caught[0].1, "States.Timeout");
    assert_eq!(out.trace.last().unwrap(), "Finalize");
    assert_eq!(mem.run().status, GoalStatus::Stalled);
    let completed = mem.events().pop().unwrap().1;
    assert_eq!(completed["outcome"], "stopped_session_lost", "{completed}");
    assert_eq!(completed["label"], "Cline stopped: the session was lost");
    assert_eq!(
        completed["detail"],
        "no heartbeat for 600 s. The work is the last saved snapshot, taken at 2026-10-13 18:50 UTC.; The run used 3 sessions."
    );
    let pkg: PackageRequest = serde_json::from_value(w.called("packager")[0].clone()).unwrap();
    // The zip and the pull request carry the stop's own detail; the event adds the sessions note after it.
    assert_eq!(
        pkg.direct.detail.as_deref(),
        Some(
            "no heartbeat for 600 s. The work is the last saved snapshot, taken at 2026-10-13 18:50 UTC."
        )
    );
    let pr = w
        .called("repo-gateway")
        .into_iter()
        .find(|v| v["operation"] == "push-pr")
        .unwrap()
        .clone();
    assert!(
        pr["description"]
            .as_str()
            .unwrap()
            .contains("holds the last saved snapshot of the work, taken at 2026-10-13 18:50 UTC, not Cline's final result."),
        "{pr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missed_heartbeat_with_nothing_saved_ends_blocked_as_a_lost_session() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::MissedHeartbeat(false));
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.trace.last().unwrap(), "Blocked");
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    let events = mem.events();
    let reason = events.iter().find(|(t, _)| t == "goal.blocked").unwrap().1["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        reason,
        "the session ended before any work was saved (no heartbeat for 600 s)"
    );
    let completed = events.last().unwrap();
    assert_eq!(completed.1["outcome"], "stopped_session_lost");
    assert_eq!(completed.1["label"], "Cline stopped: the session was lost");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_session_with_nothing_saved_ends_blocked() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Lost(false));
    let out = walk(&direct_definition(), input(), &mut w);
    let mut want = three_lost_sessions();
    want.extend(["HasWork", "Blocked"]);
    assert_eq!(out.trace, want);
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    let events = mem.events();
    let reason = events.iter().find(|(t, _)| t == "goal.blocked").unwrap().1["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(reason.contains("before any work was saved"), "{reason}");
    assert!(w.called("packager").is_empty() && w.called("bundle-validator").is_empty());
    let completed = events.last().unwrap();
    assert_eq!(completed.1["status"], "blocked");
    assert_eq!(completed.1["label"], "Cline stopped: the session was lost");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_validation_ends_blocked_with_the_violations_and_packages_nothing() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    w.validation_fails = true;
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(
        &out.trace[out.trace.len() - 3..],
        ["ValidateBundle", "ValidationOutcome", "Blocked"]
    );
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    let reason = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "goal.blocked")
        .unwrap()
        .1["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        reason,
        "bundle validation failed: secret src/a.py: gitleaks rule aws-access-token"
    );
    assert!(w.called("packager").is_empty());
}

/// The validator role lacked `s3:ListBucket` and `s3:GetObject` under the research items, so the third-party copy rule's listing
/// was refused. The refusal must reach the run's events as the reason, with the operation and the key, and package nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_validator_that_cannot_read_the_research_items_ends_the_run_blocked_with_the_denied_read()
{
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    let listed = format!("runs/{RUN}/agents/direct/research/items/");
    w.validation_denied = Some(format!("access denied: s3 list {listed}: AccessDenied"));
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(
        &out.trace[out.trace.len() - 2..],
        ["ValidateBundle", "Blocked"]
    );
    assert_eq!(
        out.caught,
        [("ValidateBundle".to_string(), "AccessDenied".to_string())],
        "the machine catches the error by name and does not retry it"
    );
    assert_eq!(out.failed, None, "the function answered the Blocked state");
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    let reason = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "goal.blocked")
        .unwrap()
        .1["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("access denied") && reason.contains(&format!("s3 list {listed}")),
        "{reason}"
    );
    assert!(w.called("packager").is_empty() && w.called("repo-gateway").len() == 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_pull_request_push_does_not_block_the_run() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    w.pr_fails = true;
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(
        out.trace.contains(&"PrFailed".to_string()),
        "{:?}",
        out.trace
    );
    assert_eq!(
        out.caught,
        [("PushPullRequest".to_string(), "Refused".to_string())]
    );
    assert_eq!(mem.run().status, GoalStatus::Complete);
    let completed = mem.events().pop().unwrap().1;
    assert_eq!(
        completed["detail"],
        "pull request not opened: refused: pull request"
    );
    assert!(!mem.event_types().contains(&"delivery.pr".to_string()));
    assert!(
        !w.called("packager").is_empty(),
        "the zip is still packaged"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_packager_ends_blocked() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    w.packager_fails = true;
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.trace.last().unwrap(), "Blocked");
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    let reason = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "goal.blocked")
        .unwrap()
        .1["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(reason, "no source bundle");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_cannot_start_ends_blocked_with_the_reason() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", "us.mistral.large-9");
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.trace, ["Name", "Prepare", "Blocked"]);
    assert_eq!(
        out.caught,
        [("Prepare".to_string(), "RunSessionInvalid".to_string())]
    );
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    assert!(mem.events().iter().any(|(t, v)| t == "goal.blocked"
        && v["reason"].as_str().unwrap().contains("us.mistral.large-9")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retried_start_state_waits_on_the_newest_token() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    w.start_failures = 2;
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.retries, [("StartSession".to_string(), 2)]);
    assert_eq!(
        w.tokens.len(),
        3,
        "every attempt of the state gets its own token"
    );
    assert_eq!(
        w.tokens
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert_eq!(mem.invocations().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_state_output_or_event_of_any_ending_says_the_harness_checked_the_work() {
    for ending in [
        Ending::Done(DirectOutcome::ReportedDone),
        Ending::Done(DirectOutcome::StoppedMaxTokens),
        Ending::Lost(true),
        Ending::Lost(false),
        Ending::TimedOut,
        Ending::MissedHeartbeat(true),
        Ending::MissedHeartbeat(false),
    ] {
        let mem = seeded(repo_target());
        let mut w = World::new(&mem, ending);
        let out = walk(&direct_definition(), input(), &mut w);
        for (t, v) in mem.events() {
            assert_eq!(
                banned_claim_word(&v.to_string()),
                None,
                "{ending:?} {t}: {v}"
            );
        }
        let plan = out
            .output
            .get("plan")
            .map(|p| p["result"]["label"].to_string())
            .unwrap_or_default();
        assert_eq!(banned_claim_word(&plan), None, "{ending:?}");
    }
}

// ---- a function that cannot answer (defect F1/2) --------------------------------------------------------------------------------

const INIT_ERROR: (&str, &str) = (
    "Runtime.ExitError",
    "RequestId: 8caf9a01 Error: Runtime exited with error: exit status 1",
);

fn dead_from(world: &mut World, first_call: u32) {
    world.dead = Some((
        first_call..u32::MAX,
        INIT_ERROR.0.into(),
        INIT_ERROR.1.into(),
    ));
}

/// Every Task state of the machine catches `States.ALL`: no failure of a Lambda, of its init or of a timeout leaves an
/// execution failing in a state that has nowhere to go (the Blocked state included, whose Catch ends the execution in a
/// Fail state with the reason).
#[test]
fn every_task_state_has_a_catch_for_every_error() {
    let def = direct_definition();
    let tasks: Vec<&str> = def["States"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, st)| st["Type"] == "Task")
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(tasks.len() >= 10, "{tasks:?}");
    for name in tasks {
        let catches = def["States"][name]["Catch"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} has no Catch"));
        assert!(
            catches.iter().any(|c| c["ErrorEquals"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e == "States.ALL")),
            "{name}: no Catch for States.ALL"
        );
        for c in catches {
            let next = c["Next"].as_str().unwrap();
            assert!(
                def["States"].get(next).is_some(),
                "{name} catches into {next}, which does not exist"
            );
        }
    }
    // The last resort is a state that cannot fail.
    assert_eq!(
        def["States"]["Blocked"]["Catch"][0]["Next"],
        "RunMachineFailed"
    );
    assert_eq!(def["States"]["RunMachineFailed"]["Type"], "Fail");
}

/// The cases the live scenario met: the function dies while it starts (a Lambda init error: `Runtime.ExitError`,
/// `Lambda.Unknown`), Lambda or the platform fails the call (`Lambda.ServiceException`), a task fails or times out. Each
/// reaches the Catch of the state that was running and the run ends `blocked` with the reason, because the function answers
/// the Blocked state.
#[tokio::test(flavor = "multi_thread")]
async fn each_kind_of_lambda_failure_reaches_the_catch_and_ends_the_run_blocked_with_the_reason() {
    // (error name, calls that fail before the function answers again)
    let cases: [(&str, u32); 5] = [
        ("Runtime.ExitError", 1),
        ("Lambda.Unknown", 1),
        ("States.TaskFailed", 1),
        ("States.Timeout", 1),
        // Prepare retries this one 6 times before it catches it.
        ("Lambda.ServiceException", 7),
    ];
    for (error, failing_calls) in cases {
        let mem = seeded(repo_target());
        let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
        w.dead = Some((
            0..failing_calls,
            error.into(),
            format!("{error}: the function failed"),
        ));
        let out = walk(&direct_definition(), input(), &mut w);
        assert_eq!(out.trace, ["Name", "Prepare", "Blocked"], "{error}");
        assert_eq!(
            out.caught,
            [("Prepare".to_string(), error.to_string())],
            "{error}"
        );
        assert_eq!(
            out.failed, None,
            "{error}: the function answered the Blocked state"
        );
        assert_eq!(mem.run().status, GoalStatus::Blocked, "{error}");
        let reason = mem
            .events()
            .into_iter()
            .find(|(t, _)| t == "goal.blocked")
            .unwrap()
            .1["reason"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(reason.contains("the function failed"), "{error}: {reason}");
    }
}

/// The failure of 2026-10-14: the function dies at init on every call, so Prepare fails, the Blocked state fails too, and the
/// execution has nothing left to call. It ends in the Fail state with the cause (so EventBridge reports it), the run is still
/// `active` because nothing could write it, and the reaper machine clears it when the function answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_cannot_start_fails_the_execution_with_the_cause_and_the_reaper_clears_the_run_later()
 {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    dead_from(&mut w, 0);
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(
        out.trace,
        ["Name", "Prepare", "Blocked", "RunMachineFailed"]
    );
    assert_eq!(
        out.caught,
        [
            ("Prepare".to_string(), INIT_ERROR.0.to_string()),
            ("Blocked".to_string(), INIT_ERROR.0.to_string()),
        ]
    );
    let (error, cause) = out.failed.clone().unwrap();
    assert_eq!(error, "RunSessionUnreachable");
    assert!(cause.contains("Runtime exited with error"), "{cause}");
    assert_eq!(
        mem.run().status,
        GoalStatus::Active,
        "nothing could write the run"
    );
    assert!(w.called("repo-gateway").is_empty() && w.called("packager").is_empty());

    // EventBridge reports the execution as FAILED with that error and cause; the reaper machine delivers it once the function works.
    let event = json!({"source": "aws.states", "detail-type": "Step Functions Execution Status Change",
        "detail": {"status": "FAILED", "name": RUN, "stateMachineArn": MACHINE, "executionArn": format!("{MACHINE}:{RUN}"),
            "stopDate": 1_790_000_100_000u64, "error": error, "cause": cause}});
    let (r, _) = serve_event(&mem.deps(), event).await;
    assert_eq!(r.unwrap()["cleared"], true);
    let run = mem.run();
    assert_eq!(run.status, GoalStatus::Cleared);
    let aborted = serde_json::to_value(&*run.aborted.unwrap()).unwrap();
    assert_eq!(aborted["status"], "FAILED");
    assert_eq!(aborted["error"], "RunSessionUnreachable");
    assert!(
        aborted["cause"]
            .as_str()
            .unwrap()
            .contains("Runtime exited with error")
    );
    assert!(mem.events().iter().any(|(t, _)| t == "run.aborted"));
}

/// The function dies after the session ended and the delivery was planned: Finalize cannot run, the Blocked state cannot
/// either, and the execution fails with the cause. The planned delivery stays in S3 (`evidence/direct.json`), and the run is
/// cleared by the reaper, not left `active`.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_dies_before_finalize_fails_the_execution_and_the_run_is_cleared_not_left_active()
 {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    // Prepare, StartSession, NextSlice and Plan answered; Finalize is the fifth call.
    dead_from(&mut w, 4);
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(
        out.trace
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .last(),
        Some(&"RunMachineFailed")
    );
    assert_eq!(
        out.caught
            .iter()
            .map(|(s, _)| s.as_str())
            .collect::<Vec<_>>(),
        ["Finalize", "Blocked"]
    );
    assert!(out.failed.is_some());
    assert_ne!(mem.run().status, GoalStatus::Complete);
    assert!(
        mem.blob(&format!("runs/{RUN}/evidence/direct.json"))
            .is_some(),
        "the plan is kept"
    );
    let event = json!({"source": "aws.states", "detail-type": "Step Functions Execution Status Change",
        "detail": {"status": "FAILED", "name": RUN, "stateMachineArn": MACHINE, "stopDate": 1_790_000_100_000u64,
            "error": out.failed.as_ref().unwrap().0, "cause": out.failed.as_ref().unwrap().1}});
    let (r, _) = serve_event(&mem.deps(), event).await;
    assert!(r.is_ok());
    assert_eq!(mem.run().status, GoalStatus::Cleared);
    // The session was stopped when it ended (`NextSlice` stops the machine of a slice before it decides) and the reaper stops it again: nothing else
    // would, and stopping twice is safe.
    assert_eq!(mem.0.stops.lock().unwrap().len(), 2);
    assert_eq!(
        mem.agent().unwrap().status,
        harness_protocol::store::AgentStatus::Stopped
    );
}

/// A stage whose worker runtime is not deployed (the half-deployed `direct` stage of the live scenario): the run does not wait
/// on a function that cannot start. The function starts, `start-session` says why in one sentence, and the run ends blocked.
#[tokio::test(flavor = "multi_thread")]
async fn a_stage_without_its_worker_runtime_ends_the_run_blocked_naming_the_missing_parameter() {
    let mem = seeded(repo_target());
    mem.runtime_from_lookup(Err(run_session::error::RunSessionError::Invalid(
        "the worker runtime is not deployed: SSM parameter /hamh/direct/worker-python-runtime-arn does not exist".into(),
    )));
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.trace[..2], ["Name", "Prepare"]);
    assert_eq!(
        out.trace
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .last(),
        Some(&"Blocked")
    );
    assert_eq!(out.failed, None);
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    let reason = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "goal.blocked")
        .unwrap()
        .1["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("worker-python-runtime-arn") && reason.contains("not deployed"),
        "{reason}"
    );
}

// ---- a follow-up run on a repository -------------------------------------------------------------------------

const SOURCE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z";
const SOURCE_HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const SOURCE_PR: &str = "https://us-east-1.console.aws.amazon.com/codesuite/codecommit/repositories/hamh-e2e-python-calc/pull-requests/7/details";

/// A repository run that continues `SOURCE`, a repository run of the same owner that opened pull request 7.
fn seeded_follow_up() -> Mem {
    let mem = Mem::default();
    let mut request = mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    request.continue_from = Some(harness_protocol::direct::ContinueFrom {
        run_id: SOURCE.into(),
        head_sha: Some(SOURCE_HEAD.into()),
    });
    mem.replace_request(serde_json::to_vec(&request).unwrap());
    let source: harness_protocol::store::RunItem = serde_json::from_value(json!({
        "runId": SOURCE, "ownerPrincipalArn": OWNER, "promptSha256": "cd".repeat(32),
        "target": {"mode": "repo", "repo": "cc:hamh-e2e-python-calc"}, "status": "complete", "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-10-13T17:00:00Z", "version": 3
    }))
    .unwrap();
    mem.0.runs.lock().unwrap().insert(SOURCE.into(), source);
    let e = run_session::events::delivery_pr(SOURCE, 1_790_000_000_000, SOURCE_PR);
    mem.0.events.lock().unwrap().insert(e.event_id.clone(), e);
    mem
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_follow_up_walks_the_machine_with_the_old_run_in_every_payload_it_needs() {
    let mem = seeded_follow_up();
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    w.pr_refusal = None;
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.trace.last().unwrap(), "Finalize", "{:?}", out.trace);
    assert!(out.caught.is_empty(), "{:?}", out.caught);
    let calls = w.called("repo-gateway");
    let stage = calls.iter().find(|v| v["operation"] == "stage").unwrap();
    assert_eq!(
        stage["fromRun"],
        json!({"runId": SOURCE, "headSha": SOURCE_HEAD})
    );
    assert_eq!(stage["target"], json!("cc:hamh-e2e-python-calc"));
    let push = calls.iter().find(|v| v["operation"] == "push-pr").unwrap();
    assert_eq!(
        push["followUp"],
        json!({"runId": SOURCE, "headSha": SOURCE_HEAD, "pullRequestUrl": SOURCE_PR})
    );
    // The zip is this run's, on the delivered commit.
    let pkg: PackageRequest = serde_json::from_value(w.called("packager")[0].clone()).unwrap();
    assert_eq!(pkg.run_id, RUN);
    assert_eq!(pkg.base_sha.as_deref(), Some(SOURCE_HEAD));
    assert_eq!(mem.run().status, GoalStatus::Complete);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delivery_conflict_leaves_the_run_complete_with_its_zip_and_says_why() {
    let mem = seeded_follow_up();
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    let conflict = format!(
        "delivery conflict: branch harness/{SOURCE} was moved after run {SOURCE} delivered 9d2f1a7c4b8e; it now points at 3333333333333333. No pull request was opened, because its list of changes would be wrong. The zip holds this run's code."
    );
    w.pr_refusal = Some(conflict.clone());
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(
        out.trace.contains(&"PrFailed".to_string()),
        "{:?}",
        out.trace
    );
    assert_eq!(
        out.caught,
        [("PushPullRequest".to_string(), "Refused".to_string())]
    );
    assert_eq!(out.trace.last().unwrap(), "Finalize");
    assert_eq!(mem.run().status, GoalStatus::Complete);
    let completed = mem.events().pop().unwrap().1;
    assert_eq!(
        completed["detail"],
        format!("pull request not opened: {conflict}"),
        "the run shows the conflict"
    );
    assert!(
        !mem.events()
            .iter()
            .any(|(t, v)| t == "delivery.pr" && v["url"] != SOURCE_PR),
        "no pull request link for this run"
    );
    let pkg: PackageRequest = serde_json::from_value(w.called("packager")[0].clone()).unwrap();
    assert_eq!(pkg.run_id, RUN, "the zip belongs to the new run");
    assert!(pkg.pr_url.is_none());
}

// ---- the Name state -------------------------------------------------------------------------------------------------------

/// `Name` is the first state. It asks run-session to name the run and does not wait for the answer: the call is asynchronous, its
/// result is dropped (`ResultPath: null`), so `Prepare` gets the input the execution started with, and the synchronous calls to the
/// function keep the indexes the kill tests count.
#[tokio::test(flavor = "multi_thread")]
async fn name_is_first_and_never_blocks_the_run() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.trace[..2], ["Name", "Prepare"]);
    assert_eq!(out.trace.last().unwrap(), "Finalize");
    assert_eq!(
        w.events,
        [json!({"op": "name-run", "runId": RUN})],
        "one event per execution"
    );
    // ResultPath null: the state Prepare is entered with is the input of the execution.
    assert_eq!(out.entered[1], ("Prepare".to_string(), input()));
    // The synchronous calls are the five of a finished run (Prepare, StartSession, NextSlice, Plan, Finalize); the event is not one of them.
    assert_eq!(w.rs_calls, 5);
    assert!(out.retries.is_empty() && out.caught.is_empty());
    assert_eq!(mem.run().status, GoalStatus::Complete);
    // The function ran the event with the naming switched off: it answered and changed nothing.
    assert_eq!(
        w.event_results,
        [Ok(json!({"named": "off", "reason": "disabled"}))]
    );
    // The end of the run wrote its fields on the search item; no name is among them.
    assert!(!mem.search_item().unwrap().contains_key("runName"));
}

/// The same execution with naming on: the event the machine sent is the op that names the run, and the name is on the search item
/// when the run is over.
#[tokio::test(flavor = "multi_thread")]
async fn the_name_is_written_by_the_event_the_machine_sends() {
    let mem = seeded(repo_target());
    let client = mem.enable_naming(vec![Scripted::title(
        "Add percent change function with tests",
    )]);
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.trace.last().unwrap(), "Finalize");
    assert_eq!(client.requests().len(), 1);
    let item = mem.search_item().unwrap();
    assert_eq!(item["runName"], "Add percent change function with tests");
    assert_eq!(item["runNameSource"], "ai");
    // The end of the run wrote its fields on the same item and kept the name.
    assert_eq!(item["runNameModel"], client.requests()[0].route);
    assert!(
        item.contains_key("lastActivity") && item.contains_key("outcome"),
        "{item:?}"
    );
}

/// A function that cannot be reached for the asynchronous call does not stop the run: Lambda's own errors are retried twice, then
/// the Catch sends the execution on to `Prepare` with its input, and the run ends as it would have.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_event_function_does_not_stop_the_run() {
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    w.event_dead = Some("Lambda.ServiceException".into());
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.retries, [("Name".to_string(), 2)]);
    assert_eq!(
        out.caught,
        [("Name".to_string(), "Lambda.ServiceException".to_string())]
    );
    assert_eq!(out.trace[..2], ["Name", "Prepare"]);
    assert_eq!(out.entered[1], ("Prepare".to_string(), input()));
    assert!(w.events.is_empty(), "the call never reached the function");
    assert_eq!(out.trace.last().unwrap(), "Finalize");
    assert_eq!(out.failed, None);
    assert_eq!(mem.run().status, GoalStatus::Complete);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_error_is_retried_then_caught_into_prepare() {
    // Twice a Lambda error, then the call goes through: retried, never caught.
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    w.event_faults = ["Lambda.ServiceException", "Lambda.TooManyRequestsException"]
        .map(String::from)
        .into();
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.retries, [("Name".to_string(), 2)]);
    assert!(out.caught.is_empty(), "{:?}", out.caught);
    assert_eq!(w.events.len(), 1);
    assert_eq!(out.trace[..2], ["Name", "Prepare"]);
    assert_eq!(mem.run().status, GoalStatus::Complete);

    // An error Lambda's list does not name is not retried; the Catch still reaches Prepare.
    let mem = seeded(repo_target());
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    w.event_faults = ["Lambda.AWSLambdaException".to_string()].into();
    let out = walk(&direct_definition(), input(), &mut w);
    assert!(out.retries.is_empty(), "{:?}", out.retries);
    assert_eq!(
        out.caught,
        [("Name".to_string(), "Lambda.AWSLambdaException".to_string())]
    );
    assert_eq!(out.trace[..2], ["Name", "Prepare"]);
    assert_eq!(out.trace.last().unwrap(), "Finalize");
    assert_eq!(mem.run().status, GoalStatus::Complete);
}

/// The event the function runs may fail without the machine ever knowing: Lambda retries it on its own. A run whose name op ends in an
/// error still completes, and the calls the kill tests count keep their indexes.
#[tokio::test(flavor = "multi_thread")]
async fn a_naming_failure_inside_the_function_is_not_the_machines() {
    let mem = seeded(repo_target());
    let _client = mem.enable_naming(vec![Scripted::title(
        "Add percent change function with tests",
    )]);
    mem.fail(
        "put_run_name",
        run_session::error::RunSessionError::Unavailable("DynamoDB is down".into()),
    );
    let mut w = World::new(&mem, Ending::Done(DirectOutcome::ReportedDone));
    let out = walk(&direct_definition(), input(), &mut w);
    assert_eq!(out.trace.last().unwrap(), "Finalize");
    assert_eq!(w.rs_calls, 5);
    assert_eq!(mem.run().status, GoalStatus::Complete);
    assert!(out.caught.is_empty() && out.retries.is_empty());
    assert_eq!(w.event_results.len(), 1);
    assert!(
        w.event_results[0]
            .as_ref()
            .unwrap_err()
            .contains("DynamoDB is down")
    );
    assert!(
        mem.name_writes().is_empty(),
        "the write failed before it happened"
    );
    assert!(!mem.search_item().unwrap().contains_key("runName"));
}
