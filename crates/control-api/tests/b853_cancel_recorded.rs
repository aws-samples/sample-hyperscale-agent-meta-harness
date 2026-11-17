//! `POST /v1/runs/{id}/cancel` records a cancel that keeps the work.
//!
//! A cancel of a run that is `active` and has an Agent item is stored on the Run item (`cancelRequestedAt`) with one harness event of kind
//! `cancel-recorded`, in one write of the store (`Store::record_cancel`). The first time stands: a repeated cancel answers it again and
//! adds no event. The answer carries what was recorded in one of three forms: a session that is asked to stop (with the latest time at
//! which a silent session is called lost), no session that answers, and a run that is already delivering. Nothing is cleared for a run
//! that has an Agent item, so a cancel while the run delivers does not throw the delivery away; `?discard=true` is the explicit clear.
//! Every status but `active` takes the path it took before.
#![allow(clippy::unwrap_used)]

use control_api::handler::{
    Blobs, CancelWrite, DeployedAs, Deps, Execution, Store, StoreError, V1Request, handle_logged,
};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::api::v1::{CancelRecorded, DownloadUrlResponse, RunControlResponse};
use harness_protocol::common::is_utc_timestamp;
use harness_protocol::direct_input::CANCEL_RECORDED_KIND;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, validate};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::{
    AgentItem, ArtifactItem, ControlItem, GoalItem, IdempotencyItem, RunItem,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

const ME: &str = "arn:aws:sts::123456789012:assumed-role/Admin/tester";
const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
/// A cancel that was recorded earlier.
const FIRST: &str = "2026-11-16T07:05:22Z";

struct Fake {
    run: Mutex<RunItem>,
    goal: Mutex<GoalItem>,
    agents: Vec<AgentItem>,
    controls: Mutex<Vec<ControlItem>>,
    stopped: Mutex<Vec<(String, String)>>,
    updates: Mutex<Vec<GoalStatus>>,
    events: Mutex<Vec<Envelope>>,
    /// Queuing a control fails.
    controls_down: bool,
    /// The answers of the next `record_cancel` calls; with none left the store records as the table does.
    scripted: Mutex<VecDeque<CancelWrite>>,
    /// The status another writer gave the run when a scripted `NotActive` is answered.
    becomes: Mutex<Option<GoalStatus>>,
    record_calls: Mutex<u32>,
}

fn run_item(status: &str, phase: &str) -> RunItem {
    serde_json::from_value(json!({
        "runId": RUN, "ownerPrincipalArn": ME, "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": status, "phase": phase,
        "enginePolicyVersion": 0, "createdAt": "2026-11-16T07:00:00Z", "version": 3
    }))
    .unwrap()
}

fn goal_item(status: &str) -> GoalItem {
    serde_json::from_value(json!({
        "runId": RUN, "objective": "x", "requirements": [], "specSha256": "", "acceptanceSha256": "",
        "status": status, "phase": "green", "version": 3
    }))
    .unwrap()
}

/// The direct Agent item in `status`, with the members of `extra`.
fn agent_with(status: &str, extra: Value) -> AgentItem {
    let mut v = json!({
        "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:123456789012:runtime/hamh_direct_harness_worker_python-x",
        "runtimeSessionId": format!("{RUN}-direct-g1"), "generation": 1, "engine": "cline",
        "engineVersion": "3.0.71", "transport": "acp", "modelId": "us.anthropic.claude-opus-5-5",
        "pool": "direct", "status": status, "version": 1, "sessionGeneration": 1,
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::from_value(v).unwrap()
}

fn agent(status: &str) -> AgentItem {
    agent_with(status, json!({}))
}

impl Fake {
    fn new(status: &str, phase: &str, agents: Vec<AgentItem>) -> Fake {
        Fake {
            run: Mutex::new(run_item(status, phase)),
            goal: Mutex::new(goal_item(status)),
            agents,
            controls: Mutex::default(),
            stopped: Mutex::default(),
            updates: Mutex::default(),
            events: Mutex::default(),
            controls_down: false,
            scripted: Mutex::default(),
            becomes: Mutex::default(),
            record_calls: Mutex::default(),
        }
    }
    /// An active run that works (phase `green`).
    fn active(agents: Vec<AgentItem>) -> Fake {
        Fake::new("active", "green", agents)
    }
    fn stored_cancel(&self) -> Option<String> {
        self.run.lock().unwrap().cancel_requested_at.clone()
    }
    fn status(&self) -> GoalStatus {
        self.run.lock().unwrap().status
    }
    fn script(&self, answers: &[CancelWrite]) {
        self.scripted
            .lock()
            .unwrap()
            .extend(answers.iter().cloned());
    }
}

impl Store for &Fake {
    async fn claim_idempotency(&self, c: &IdempotencyItem) -> Result<IdempotencyItem, StoreError> {
        Ok(c.clone())
    }
    async fn create_run(&self, _: &RunItem, _: &GoalItem, _: &Envelope) -> Result<(), StoreError> {
        Ok(())
    }
    async fn get_run(&self, _: &str) -> Result<Option<RunItem>, StoreError> {
        Ok(Some(self.run.lock().unwrap().clone()))
    }
    async fn get_root_goal(&self, _: &str) -> Result<Option<GoalItem>, StoreError> {
        Ok(Some(self.goal.lock().unwrap().clone()))
    }
    async fn list_runs(
        &self,
        _: &str,
        _: Option<&str>,
    ) -> Result<(Vec<RunItem>, Option<String>), StoreError> {
        Ok((vec![], None))
    }
    async fn events(
        &self,
        _: &str,
        _: u8,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        Ok(vec![])
    }
    async fn list_artifacts(&self, _: &str) -> Result<Vec<ArtifactItem>, StoreError> {
        Ok(vec![])
    }
    async fn get_artifact(&self, _: &str, _: &str) -> Result<Option<ArtifactItem>, StoreError> {
        Ok(None)
    }
    async fn put_control(&self, c: &ControlItem) -> Result<(), StoreError> {
        if self.controls_down {
            return Err(StoreError::Unavailable("throttled".into()));
        }
        self.controls.lock().unwrap().push(c.clone());
        Ok(())
    }
    async fn list_agents(&self, _: &str) -> Result<Vec<AgentItem>, StoreError> {
        Ok(self.agents.clone())
    }
    /// The table's rule: the time and the event are written once, and only for a run that is active.
    async fn record_cancel(
        &self,
        _: &str,
        at: &str,
        event: &Envelope,
    ) -> Result<CancelWrite, StoreError> {
        *self.record_calls.lock().unwrap() += 1;
        if let Some(answer) = self.scripted.lock().unwrap().pop_front() {
            if answer == CancelWrite::NotActive
                && let Some(to) = self.becomes.lock().unwrap().take()
            {
                self.run.lock().unwrap().status = to;
                self.goal.lock().unwrap().status = to;
            }
            return Ok(answer);
        }
        let mut run = self.run.lock().unwrap();
        if run.status != GoalStatus::Active {
            return Ok(CancelWrite::NotActive);
        }
        if let Some(first) = &run.cancel_requested_at {
            return Ok(CancelWrite::Already(first.clone()));
        }
        run.cancel_requested_at = Some(at.to_string());
        self.events.lock().unwrap().push(event.clone());
        Ok(CancelWrite::Recorded)
    }
    async fn update_run(
        &self,
        run: &RunItem,
        goal: Option<&GoalItem>,
        event: Option<&Envelope>,
    ) -> Result<(), StoreError> {
        let old = self.run.lock().unwrap().version;
        assert_eq!(
            run.version,
            old + 1,
            "the Run item is written at its next version"
        );
        self.updates.lock().unwrap().push(run.status);
        *self.run.lock().unwrap() = run.clone();
        if let Some(g) = goal {
            *self.goal.lock().unwrap() = g.clone();
        }
        if let Some(e) = event {
            self.events.lock().unwrap().push(e.clone());
        }
        Ok(())
    }
}

impl Blobs for &Fake {
    async fn put(&self, key: &str, _: &[u8]) -> Result<String, StoreError> {
        Ok(format!("s3://b/{key}"))
    }
    fn bucket(&self) -> &str {
        "hamh-direct-harness-123456789012"
    }
    async fn presign_get(&self, _: &str, _: Duration) -> Result<DownloadUrlResponse, StoreError> {
        Err(StoreError::Unavailable("not used".into()))
    }
}

impl Execution for &Fake {
    async fn start(&self, _: &str, _: &Value) -> Result<(), StoreError> {
        Ok(())
    }
    async fn stop(&self, name: &str, cause: &str) -> Result<(), StoreError> {
        self.stopped
            .lock()
            .unwrap()
            .push((name.to_string(), cause.to_string()));
        Ok(())
    }
    async fn deployment(&self) -> Result<DeployedAs, StoreError> {
        Ok(DeployedAs::default())
    }
}

fn deps(f: &Fake) -> Deps<&Fake, &Fake, &Fake> {
    Deps {
        store: f,
        blobs: f,
        execution: f,
        account: "123456789012".into(),
        operators: vec![],
        repo_names: Default::default(),
    }
}

/// `POST /v1/runs/<run>/cancel`: the status, the body and the lines the function logs.
async fn cancel(f: &Fake, query: &[(&str, &str)]) -> (u16, Value, Vec<String>) {
    let req = V1Request {
        method: "POST".into(),
        path: format!("/v1/runs/{RUN}/cancel"),
        caller_arn: Some(ME.into()),
        source_ip: None,
        headers: BTreeMap::new(),
        query: query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: String::new(),
    };
    let (out, logs) = handle_logged(req, &deps(f)).await;
    (out.status, serde_json::from_str(&out.body).unwrap(), logs)
}

fn recorded(v: &Value) -> CancelRecorded {
    let resp: RunControlResponse = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(resp.run_id, RUN);
    resp.cancel.expect("the answer carries what was recorded")
}

/// A session that beats: its last heartbeat and the start of its generation.
fn beating() -> AgentItem {
    agent_with(
        "running",
        json!({"heartbeatAt": "2026-11-16T07:05:22Z", "generationStartedAt": "2026-11-16T07:03:00Z"}),
    )
}

// ---- T2 ----------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b853_cancel_records_the_flag_and_keeps_the_first_time() {
    let f = Fake::active(vec![beating()]);
    let (status, v, logs) = cancel(&f, &[]).await;
    assert_eq!(status, 200, "{v}");
    assert!(logs.is_empty(), "{logs:?}");
    assert_eq!(v["status"], json!("active"), "the status does not change");
    let first = recorded(&v);
    assert!(is_utc_timestamp(&first.requested_at), "{first:?}");
    assert_eq!(
        f.stored_cancel().as_deref(),
        Some(first.requested_at.as_str()),
        "the Run item holds the time"
    );
    assert_eq!(first.session, Some(1));
    assert!(first.session_live && !first.delivering, "{first:?}");
    assert_eq!(
        first.last_heartbeat_at.as_deref(),
        Some("2026-11-16T07:05:22Z")
    );
    assert_eq!(
        first.lost_after.as_deref(),
        Some("2026-11-16T07:15:22Z"),
        "the last heartbeat plus 600 s"
    );
    // One harness event says it, with the same facts.
    {
        let events = f.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, EventType::RunWaiting);
        assert_eq!(e.authority, Authority::Harness);
        assert!(e.task_id.is_none() && e.agent_id.is_none());
        assert!(validate(e).is_ok(), "{:?}", validate(e));
        let Payload::Inline { value } = &e.payload else {
            panic!("an inline payload")
        };
        assert_eq!(value["kind"], json!(CANCEL_RECORDED_KIND));
        let said: CancelRecorded = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(said, first);
    }
    assert!(f.updates.lock().unwrap().is_empty(), "no status is written");
    assert!(
        f.stopped.lock().unwrap().is_empty(),
        "the execution runs on"
    );

    // The first time stands. (The stored time is moved back so that a second cancel in the same second cannot pass by accident.)
    f.run.lock().unwrap().cancel_requested_at = Some(FIRST.into());
    let (status, v, _) = cancel(&f, &[]).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(recorded(&v).requested_at, FIRST);
    assert_eq!(f.stored_cancel().as_deref(), Some(FIRST));
    assert_eq!(f.events.lock().unwrap().len(), 1, "no second event");
    // Each cancel queues its own control for the live session.
    let controls = f.controls.lock().unwrap().clone();
    assert_eq!(controls.len(), 2);
    assert!(
        controls
            .iter()
            .all(|c| c.agent_id == "direct" && c.control == PendingControl::Cancel)
    );

    // A session that has sent no heartbeat yet is called lost 600 s after its start.
    let f = Fake::active(vec![agent_with(
        "running",
        json!({"sessionGeneration": 2, "generationStartedAt": "2026-11-16T07:03:00Z"}),
    )]);
    let c = recorded(&cancel(&f, &[]).await.1);
    assert_eq!(c.session, Some(2));
    assert_eq!(c.last_heartbeat_at, None);
    assert_eq!(c.lost_after.as_deref(), Some("2026-11-16T07:13:00Z"));
    // The later of the two times counts.
    let f = Fake::active(vec![agent_with(
        "running",
        json!({"heartbeatAt": "2026-11-16T07:00:00Z", "generationStartedAt": "2026-11-16T07:03:00Z"}),
    )]);
    let c = recorded(&cancel(&f, &[]).await.1);
    assert_eq!(c.lost_after.as_deref(), Some("2026-11-16T07:13:00Z"));
}

#[tokio::test]
async fn b853_a_control_that_cannot_be_queued_does_not_fail_a_recorded_cancel() {
    let f = Fake {
        controls_down: true,
        ..Fake::active(vec![beating()])
    };
    let (status, v, logs) = cancel(&f, &[]).await;
    assert_eq!(status, 200, "the cancel did not fail: {v}");
    assert!(f.stored_cancel().is_some());
    assert!(recorded(&v).session_live);
    assert!(
        logs.iter()
            .any(|l| l.contains("the Cancel control was not queued")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn b853_a_write_that_met_another_writer_is_tried_once_more() {
    // Another writer changed the item at the same instant, once: the second try records the cancel.
    let f = Fake::active(vec![beating()]);
    f.script(&[CancelWrite::Busy]);
    let (status, v, _) = cancel(&f, &[]).await;
    assert_eq!(status, 200, "{v}");
    assert!(f.stored_cancel().is_some());
    assert_eq!(*f.record_calls.lock().unwrap(), 2);
    // Twice: nothing is recorded and the caller is asked to retry.
    let f = Fake::active(vec![beating()]);
    f.script(&[CancelWrite::Busy, CancelWrite::Busy]);
    let (status, v, _) = cancel(&f, &[]).await;
    assert_eq!(status, 503, "{v}");
    assert!(v["message"].as_str().unwrap().contains("retry"), "{v}");
    assert_eq!(f.stored_cancel(), None);
    assert!(
        f.controls.lock().unwrap().is_empty(),
        "nothing is queued for a cancel that was not recorded"
    );
    assert_eq!(
        *f.record_calls.lock().unwrap(),
        2,
        "one more try, not a loop"
    );
}

// ---- T2b: every status but `active` takes the path it took before -------------------------------------------------------------

#[tokio::test]
async fn b853_cancel_of_a_run_that_is_not_active_takes_todays_path() {
    for status in ["blocked", "usage_limited", "budget_limited"] {
        let f = Fake::new(status, "green", vec![agent("stopped")]);
        let (code, v, _) = cancel(&f, &[]).await;
        assert_eq!(code, 200, "{status}: {v}");
        assert_eq!(f.status(), GoalStatus::Cleared, "{status}");
        assert_eq!(
            f.stopped.lock().unwrap().len(),
            1,
            "{status}: the execution is stopped"
        );
        assert_eq!(f.stored_cancel(), None, "{status}");
        assert_eq!(
            *f.record_calls.lock().unwrap(),
            0,
            "{status}: the record is not tried"
        );
        assert!(v.get("cancel").is_none(), "{status}: {v}");
    }
    let done = Fake::new("complete", "green", vec![agent("stopped")]);
    let (code, v, _) = cancel(&done, &[]).await;
    assert_eq!(
        (code, v["code"].as_str()),
        (409, Some("run-finished")),
        "{v}"
    );
    assert_eq!(*done.record_calls.lock().unwrap(), 0);
    let cleared = Fake::new("cleared", "green", vec![agent("stopped")]);
    let (code, _, _) = cancel(&cleared, &[]).await;
    assert_eq!(code, 200);
    assert!(cleared.updates.lock().unwrap().is_empty());
    assert_eq!(
        cleared.stopped.lock().unwrap().len(),
        1,
        "the stop is sent again"
    );
    assert_eq!(*cleared.record_calls.lock().unwrap(), 0);

    // The run ended between the route's read and its write: the route goes on from the status the run has now.
    let f = Fake::active(vec![agent("stopped")]);
    f.script(&[CancelWrite::NotActive]);
    *f.becomes.lock().unwrap() = Some(GoalStatus::Blocked);
    let (code, v, _) = cancel(&f, &[]).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(
        f.status(),
        GoalStatus::Cleared,
        "a blocked run is cleared, as before"
    );
    assert_eq!(f.stopped.lock().unwrap().len(), 1);
    let f = Fake::active(vec![agent("stopped")]);
    f.script(&[CancelWrite::NotActive]);
    *f.becomes.lock().unwrap() = Some(GoalStatus::Complete);
    let (code, v, _) = cancel(&f, &[]).await;
    assert_eq!(
        (code, v["code"].as_str()),
        (409, Some("run-finished")),
        "{v}"
    );
    assert!(f.stopped.lock().unwrap().is_empty());
    // A store that says "not active" for a run that reads as active is not obeyed with a clear: the caller retries.
    let f = Fake::active(vec![agent("running")]);
    f.script(&[CancelWrite::NotActive]);
    let (code, v, _) = cancel(&f, &[]).await;
    assert_eq!(code, 503, "{v}");
    assert_eq!(f.status(), GoalStatus::Active);
    assert!(f.stopped.lock().unwrap().is_empty());
}

// ---- T2c: the three forms of the answer ---------------------------------------------------------------------------------------

#[tokio::test]
async fn b853_the_answer_has_three_forms() {
    // A session the service has not ended: it is asked to stop, and the answer says when it is called lost at the latest.
    let f = Fake::active(vec![beating()]);
    let c = recorded(&cancel(&f, &[]).await.1);
    assert!(c.session_live && !c.delivering);
    assert!(c.lost_after.is_some());
    // No session that answers (the service ended it), and the run is not delivering yet.
    for status in ["stopped", "failed"] {
        let f = Fake::active(vec![agent_with(
            status,
            json!({"heartbeatAt": "2026-11-16T07:05:22Z"}),
        )]);
        let (code, v, _) = cancel(&f, &[]).await;
        assert_eq!(code, 200, "{status}: {v}");
        let c = recorded(&v);
        assert!(!c.session_live && !c.delivering, "{status}: {c:?}");
        assert_eq!(c.lost_after, None, "{status}");
        assert_eq!(c.session, Some(1));
        assert!(
            v["cancel"].get("delivering").is_none(),
            "left out when false: {v}"
        );
    }
    // A run that is delivering, whatever the Agent item says: nothing will be called lost.
    for status in ["running", "stopped"] {
        let f = Fake::new(
            "active",
            "delivering",
            vec![agent_with(
                status,
                json!({"heartbeatAt": "2026-11-16T07:05:22Z", "generationStartedAt": "2026-11-16T07:03:00Z"}),
            )],
        );
        let (code, v, _) = cancel(&f, &[]).await;
        assert_eq!(code, 200, "{status}: {v}");
        let c = recorded(&v);
        assert!(c.delivering && !c.session_live, "{status}: {c:?}");
        assert_eq!(c.lost_after, None, "{status}");
        assert_eq!(v["cancel"]["delivering"], json!(true));
    }
}

// ---- T3 ----------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b853_cancel_while_delivering_clears_nothing() {
    // The run delivers after a session that left no result: plan-delivery marked the agent stopped, minutes before the run ends.
    let f = Fake::new("active", "delivering", vec![agent("stopped")]);
    let (code, v, _) = cancel(&f, &[]).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(
        f.status(),
        GoalStatus::Active,
        "the delivery is not thrown away"
    );
    assert!(f.stored_cancel().is_some(), "the cancel is recorded");
    assert!(
        f.stopped.lock().unwrap().is_empty(),
        "the execution runs on and delivers"
    );
    assert!(f.updates.lock().unwrap().is_empty());
    assert!(
        f.controls.lock().unwrap().is_empty(),
        "no session is there to tell"
    );
    assert!(recorded(&v).delivering);
    // A second cancel while the cancelled run delivers: the same time, no second event, still nothing cleared.
    let first = f.stored_cancel();
    let (code, v, _) = cancel(&f, &[]).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(Some(recorded(&v).requested_at), first);
    assert_eq!(f.events.lock().unwrap().len(), 1);
    assert_eq!(f.status(), GoalStatus::Active);

    // A run with no Agent item at all: no session ever ran, nothing was saved, and the run is cleared as before.
    let f = Fake::active(vec![]);
    let (code, v, _) = cancel(&f, &[]).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(f.status(), GoalStatus::Cleared);
    assert_eq!(f.stopped.lock().unwrap().len(), 1);
    assert_eq!(f.stored_cancel(), None);
    assert!(v.get("cancel").is_none(), "{v}");
}

// ---- T3b (the route) ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b853_discard_is_the_explicit_clear() {
    // An active run whose session was ended: the keep-the-work cancel records, the discard clears.
    let f = Fake::new("active", "delivering", vec![agent("stopped")]);
    let (code, v, _) = cancel(&f, &[("discard", "true")]).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(f.status(), GoalStatus::Cleared);
    assert_eq!(f.stopped.lock().unwrap().len(), 1);
    assert_eq!(
        *f.record_calls.lock().unwrap(),
        0,
        "a discard records no cancel"
    );
    assert!(v.get("cancel").is_none(), "{v}");
    // A discard after a recorded cancel: the run is cleared, and the record stays as history.
    let f = Fake::active(vec![beating()]);
    cancel(&f, &[]).await;
    let first = f.stored_cancel();
    assert!(first.is_some());
    let (code, v, _) = cancel(&f, &[("discard", "true")]).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(f.status(), GoalStatus::Cleared);
    assert_eq!(f.stored_cancel(), first, "the record is untouched");
    assert_eq!(f.stopped.lock().unwrap().len(), 1);
}

// ---- T2c with the read of a stopped session's storage: a fenced generation ----------------------------------------------------

/// A session the service has fenced (its worker gave notice of its end, or the service is stopping it to read its storage back) is no
/// session that answers. The answer of the cancel says so and names no time at which the session would be called lost, and no Cancel
/// is queued for it: worker-api hands a fenced session no control.
#[tokio::test]
async fn b853_a_fenced_generation_is_no_session_that_answers() {
    // TRIGGER PRESENT: the same session without the fence is live and is told.
    let f = Fake::active(vec![beating()]);
    let c = recorded(&cancel(&f, &[]).await.1);
    assert!(
        c.session_live && c.lost_after.is_some(),
        "TRIGGER PRESENT: {c:?}"
    );
    assert_eq!(
        f.controls.lock().unwrap().len(),
        1,
        "TRIGGER PRESENT: the live session is told"
    );

    // The generation the Agent item is at is fenced.
    let fenced = agent_with(
        "running",
        json!({"heartbeatAt": "2026-11-16T07:05:22Z", "generationStartedAt": "2026-11-16T07:03:00Z", "fencedGeneration": 1}),
    );
    let f = Fake::active(vec![fenced]);
    let (code, v, _) = cancel(&f, &[]).await;
    assert_eq!(code, 200, "{v}");
    let c = recorded(&v);
    assert!(!c.session_live && !c.delivering, "{c:?}");
    assert_eq!(c.lost_after, None, "{c:?}");
    assert_eq!(c.session, Some(1));
    assert!(f.stored_cancel().is_some(), "the cancel is recorded");
    assert_eq!(f.status(), GoalStatus::Active);
    assert!(
        f.controls.lock().unwrap().is_empty(),
        "a fenced session gets no Cancel"
    );

    // The fence of an earlier generation says nothing about the session the run is in now.
    let later = agent_with(
        "running",
        json!({"heartbeatAt": "2026-11-16T07:05:22Z", "generationStartedAt": "2026-11-16T07:03:00Z",
               "sessionGeneration": 2, "fencedGeneration": 1}),
    );
    let f = Fake::active(vec![later]);
    let c = recorded(&cancel(&f, &[]).await.1);
    assert!(c.session_live && c.lost_after.is_some(), "{c:?}");
    assert_eq!(c.session, Some(2));
    assert_eq!(f.controls.lock().unwrap().len(), 1);
}
