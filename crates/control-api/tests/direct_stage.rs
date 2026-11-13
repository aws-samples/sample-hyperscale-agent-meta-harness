//! Run control of a direct run:
//!
//! - `POST /v1/runs/{id}/cancel` stops Cline and keeps the work: it queues a Cancel control for the `direct` agent and
//!   changes nothing else (the run stays `active`, the execution runs on, agentd ends the prompt within 10 s and the
//!   machine delivers what exists). `?discard=true` is the hard cancel: `cleared` plus a stopped execution. A run with no
//!   live agent yet has no work to keep and is cleared;
//! - `GET /v1/models` lists the six models of `models.json` without reading the DynamoDB catalog.

#![allow(clippy::unwrap_used)]

use control_api::direct_models::builtin;
use control_api::handler::{
    Blobs, DeployedAs, Deps, Execution, Store, StoreError, V1Request, handle,
};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::api::v1::{DownloadUrlResponse, ModelsResponse, RunControlResponse};
use harness_protocol::common::{Engine, Protocol};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, validate};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::{
    AgentItem, AgentStatus, ArtifactItem, ControlItem, GoalItem, IdempotencyItem, RunItem,
    event_shard,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

const ME: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";

const OTHER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/someone";

struct Fake {
    run: Mutex<RunItem>,
    goal: Mutex<GoalItem>,
    agents: Vec<AgentItem>,
    controls: Mutex<Vec<ControlItem>>,
    stopped: Mutex<Vec<(String, String)>>,
    updates: Mutex<Vec<GoalStatus>>,
    events: Mutex<Vec<Envelope>>,
    /// The next run update loses to a concurrent writer.
    conflict: Mutex<bool>,
    /// Stopping the execution fails.
    stop_down: bool,
    /// Queuing a control fails.
    controls_down: bool,
    /// The agent read fails.
    agents_down: bool,
}

fn run_item(status: &str) -> RunItem {
    serde_json::from_value(json!({
        "runId": RUN, "ownerPrincipalArn": ME, "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": status, "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-10-13T18:00:00Z", "version": 3
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

fn agent(status: &str) -> AgentItem {
    serde_json::from_value(json!({
        "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:700000000011:runtime/hamh_direct_harness_worker_python-x",
        "runtimeSessionId": format!("{RUN}-direct-g1"), "generation": 1, "engine": "cline",
        "engineVersion": "3.0.71", "transport": "acp", "modelId": "us.anthropic.claude-opus-5-5",
        "pool": "direct", "status": status, "version": 1
    }))
    .unwrap()
}

impl Fake {
    fn new(status: &str, agents: Vec<AgentItem>) -> Fake {
        Fake {
            run: Mutex::new(run_item(status)),
            goal: Mutex::new(goal_item(status)),
            agents,
            controls: Mutex::default(),
            stopped: Mutex::default(),
            updates: Mutex::default(),
            events: Mutex::default(),
            conflict: Mutex::new(false),
            stop_down: false,
            controls_down: false,
            agents_down: false,
        }
    }
    fn direct(agents: Vec<AgentItem>) -> Fake {
        Fake::new("active", agents)
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
        if self.agents_down {
            return Err(StoreError::Unavailable("throttled".into()));
        }
        Ok(self.agents.clone())
    }
    async fn update_run(
        &self,
        run: &RunItem,
        goal: Option<&GoalItem>,
        event: Option<&Envelope>,
    ) -> Result<(), StoreError> {
        if std::mem::take(&mut *self.conflict.lock().unwrap()) {
            return Err(StoreError::Conflict);
        }
        let old = self.run.lock().unwrap().version;
        assert_eq!(
            run.version,
            old + 1,
            "the Run item is written at its next version"
        );
        self.updates.lock().unwrap().push(run.status);
        *self.run.lock().unwrap() = run.clone();
        if let Some(g) = goal {
            assert_eq!(g.version, self.goal.lock().unwrap().version + 1);
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
        "hamh-direct-harness-700000000011"
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
        if self.stop_down {
            return Err(StoreError::Unavailable("states unreachable".into()));
        }
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
        account: "700000000011".into(),
        operators: vec![],
        repo_names: Default::default(),
    }
}

fn req(method: &str, path: &str, query: &[(&str, &str)]) -> V1Request {
    V1Request {
        method: method.into(),
        path: path.into(),
        caller_arn: Some(ME.into()),
        source_ip: None,
        headers: BTreeMap::new(),
        query: query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: String::new(),
    }
}

async fn call_as(f: &Fake, caller: &str, method: &str, path: &str) -> (u16, Value) {
    let mut r = req(method, path, &[]);
    r.caller_arn = Some(caller.into());
    let out = handle(r, &deps(f)).await;
    (out.status, serde_json::from_str(&out.body).unwrap())
}

async fn cancel(f: &Fake, query: &[(&str, &str)]) -> (u16, Value) {
    let out = handle(
        req("POST", &format!("/v1/runs/{RUN}/cancel"), query),
        &deps(f),
    )
    .await;
    (out.status, serde_json::from_str(&out.body).unwrap())
}

#[tokio::test]
async fn cancel_asks_cline_to_stop_and_keeps_the_run_going() {
    let f = Fake::direct(vec![agent("running")]);
    let (status, v) = cancel(&f, &[]).await;
    assert_eq!(status, 200, "{v}");
    let resp: RunControlResponse = serde_json::from_value(v).unwrap();
    assert_eq!(resp.status, GoalStatus::Active, "the run is not cleared");
    let controls = f.controls.lock().unwrap();
    assert_eq!(controls.len(), 1);
    assert_eq!(controls[0].agent_id, "direct");
    assert_eq!(controls[0].control, PendingControl::Cancel);
    assert!(
        f.stopped.lock().unwrap().is_empty(),
        "the execution runs on and delivers"
    );
    assert!(
        f.updates.lock().unwrap().is_empty(),
        "no status change is written"
    );
    assert_eq!(f.run.lock().unwrap().status, GoalStatus::Active);
}

#[tokio::test]
async fn cancel_with_discard_clears_the_run_and_stops_the_execution() {
    let f = Fake::direct(vec![agent("running")]);
    let (status, v) = cancel(&f, &[("discard", "true")]).await;
    assert_eq!(status, 200, "{v}");
    let resp: RunControlResponse = serde_json::from_value(v).unwrap();
    assert_eq!(resp.status, GoalStatus::Cleared);
    assert_eq!(f.run.lock().unwrap().status, GoalStatus::Cleared);
    assert_eq!(f.stopped.lock().unwrap().len(), 1);
    assert_eq!(f.stopped.lock().unwrap()[0].0, RUN);
    assert_eq!(
        f.controls.lock().unwrap().len(),
        1,
        "the agent is still told to stop"
    );
}

#[tokio::test]
async fn only_the_word_true_discards() {
    for value in ["false", "1", "", "TRUE", "yes"] {
        let f = Fake::direct(vec![agent("running")]);
        let (status, _) = cancel(&f, &[("discard", value)]).await;
        assert_eq!(status, 200);
        assert_eq!(
            f.run.lock().unwrap().status,
            GoalStatus::Active,
            "discard={value:?}"
        );
        assert!(f.stopped.lock().unwrap().is_empty(), "discard={value:?}");
    }
}

#[tokio::test]
async fn a_run_with_no_live_agent_has_no_work_to_keep_and_is_cleared() {
    for agents in [vec![], vec![agent("stopped")], vec![agent("failed")]] {
        let f = Fake::direct(agents);
        let (status, v) = cancel(&f, &[]).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(f.run.lock().unwrap().status, GoalStatus::Cleared);
        assert_eq!(
            f.stopped.lock().unwrap().len(),
            1,
            "the execution is stopped"
        );
    }
}

#[tokio::test]
async fn cancel_of_a_finished_run_is_409_and_of_a_cleared_one_is_a_no_op() {
    let done = Fake::new("complete", vec![agent("stopped")]);
    let (status, v) = cancel(&done, &[]).await;
    assert_eq!(status, 409, "{v}");
    assert!(done.controls.lock().unwrap().is_empty());
    let cleared = Fake::new("cleared", vec![agent("stopped")]);
    let (status, _) = cancel(&cleared, &[]).await;
    assert_eq!(status, 200);
    assert!(cleared.updates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancel_twice_queues_a_second_control_and_still_changes_nothing() {
    let f = Fake::direct(vec![agent("running")]);
    cancel(&f, &[]).await;
    let (status, _) = cancel(&f, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(f.controls.lock().unwrap().len(), 2);
    assert_eq!(f.run.lock().unwrap().status, GoalStatus::Active);
}

#[tokio::test]
async fn models_are_the_six_cline_rows_then_the_opencode_rows_of_models_json_with_no_catalog_read()
{
    let f = Fake::direct(vec![]);
    let out = handle(req("GET", "/v1/models", &[]), &deps(&f)).await;
    assert_eq!(out.status, 200, "{}", out.body);
    let m: ModelsResponse = serde_json::from_str(&out.body).unwrap();
    assert!(m.excluded.is_empty());
    // `GET /v1/models` shows the rows with a smoke record: all six, Claude Fable 5.2 among them.
    let want: Vec<&str> = builtin()
        .models()
        .iter()
        .filter(|m| m.selectable())
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(want.len(), 6);
    let got: Vec<&str> = m.rows.iter().map(|r| r.profile_id.as_str()).collect();
    assert_eq!(got[..6], want[..]);
    // After them come the rows of OpenCode, each naming its engine.
    let opencode: Vec<&str> = ham_core::models::builtin_catalog()
        .for_engine(Engine::OPENCODE)
        .unwrap()
        .models()
        .iter()
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(got[6..], opencode[..]);
    assert!(m.rows[6..].iter().all(|r| r.engine == Engine::OPENCODE));
    for (row, model) in m
        .rows
        .iter()
        .zip(builtin().models().iter().filter(|m| m.selectable()))
    {
        assert_eq!(row.region, model.region);
        assert_eq!(row.context_tokens, model.context_tokens);
        assert_eq!(row.max_output_tokens, model.max_output_tokens);
        assert_eq!(row.protocol, Protocol::Converse);
        assert_eq!(row.engine, Engine::CLINE);
        assert_eq!(row.base_model_id, model.base_model_id);
        assert_eq!(
            Some(row.smoke_record_id.as_str()),
            model.smoke_record_id.as_deref()
        );
    }
}

#[tokio::test]
async fn an_unlisted_model_without_a_smoke_record_is_not_listed() {
    // The list is `builtin()`: a row without a smoke record is not selectable (invariant 6) and `GET /v1/models` shows only
    // what a run may select.
    let f = Fake::direct(vec![]);
    let out = handle(req("GET", "/v1/models", &[]), &deps(&f)).await;
    let m: ModelsResponse = serde_json::from_str(&out.body).unwrap();
    assert!(m.rows.iter().all(|r| !r.smoke_record_id.trim().is_empty()));
    let _ = AgentStatus::Running;
}

#[tokio::test]
async fn a_discarding_cancel_rewrites_the_run_and_goal_and_publishes_the_status() {
    let f = Fake::direct(vec![agent("running"), agent("stopped")]);
    let (status, v) = cancel(&f, &[("discard", "true")]).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(f.run.lock().unwrap().version, 4);
    let g = f.goal.lock().unwrap().clone();
    assert_eq!((g.status, g.version), (GoalStatus::Cleared, 4));
    {
        let events = f.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, EventType::GoalStatus);
        assert_eq!(e.authority, Authority::Harness);
        assert!(e.task_id.is_none() && e.agent_id.is_none());
        assert_eq!(event_shard(e.authority, None), 0);
        assert!(validate(e).is_ok());
        let Payload::Inline { value } = &e.payload else {
            panic!()
        };
        assert_eq!(value, &json!({"status": "cleared", "phase": "green"}));
    }
    // One Cancel control for the live agent, and a cause that names the caller.
    assert_eq!(f.controls.lock().unwrap().len(), 1);
    let stopped = f.stopped.lock().unwrap().clone();
    assert_eq!(stopped.len(), 1);
    assert!(stopped[0].1.contains(ME), "{}", stopped[0].1);
    // A repeat writes no status but stops and signals again (a retry after a failure).
    let (status, _) = cancel(&f, &[("discard", "true")]).await;
    assert_eq!(status, 200);
    assert_eq!(f.events.lock().unwrap().len(), 1);
    assert_eq!(f.stopped.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn stopping_a_runs_work_queues_a_cancel_for_each_live_agent_and_stops_the_root_on_request() {
    let f = Fake::direct(vec![
        agent("running"),
        agent("stopped"),
        agent("failed"),
        agent("idle"),
    ]);
    let report = control_api::controls::stop_run_work(&&f, &&f, RUN, "run ended ABORTED", false)
        .await
        .unwrap();
    assert_eq!(
        report.controls, 2,
        "running and idle; stopped and failed get none"
    );
    assert!(
        f.stopped.lock().unwrap().is_empty(),
        "the ended root is not stopped again"
    );
    assert!(
        f.events.lock().unwrap().is_empty(),
        "stop_run_work writes no Run item or event"
    );
    control_api::controls::stop_run_work(&&f, &&f, RUN, "cancelled by me", true)
        .await
        .unwrap();
    assert_eq!(f.stopped.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancel_is_the_owners_and_other_methods_are_refused() {
    let f = Fake::direct(vec![agent("running")]);
    let path = format!("/v1/runs/{RUN}/cancel");
    let (code, body) = call_as(&f, OTHER, "POST", &path).await;
    assert_eq!((code, body["code"].as_str()), (404, Some("not-found")));
    // The Fake holds one run; a path that is not a ULID is not a run.
    let (code, _) = call_as(&f, ME, "POST", "/v1/runs/not-a-ulid/cancel").await;
    assert_eq!(code, 404);
    for method in ["GET", "DELETE", "PUT"] {
        let (code, body) = call_as(&f, ME, method, &path).await;
        assert_eq!(
            (code, body["code"].as_str()),
            (405, Some("method-not-allowed")),
            "{method}"
        );
    }
    assert!(f.controls.lock().unwrap().is_empty());
    assert!(f.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_lost_race_is_a_conflict_and_failures_ask_for_a_retry() {
    let f = Fake::direct(vec![agent("running")]);
    *f.conflict.lock().unwrap() = true;
    let (code, body) = cancel(&f, &[("discard", "true")]).await;
    assert_eq!((code, body["code"].as_str()), (409, Some("conflict")));
    assert_eq!(f.run.lock().unwrap().status, GoalStatus::Active);

    let f = Fake {
        stop_down: true,
        ..Fake::direct(vec![agent("running")])
    };
    let (code, body) = cancel(&f, &[("discard", "true")]).await;
    assert_eq!(code, 503, "{body}");
    assert!(body["message"].as_str().unwrap().contains("retry"));
    assert_eq!(
        f.run.lock().unwrap().status,
        GoalStatus::Cleared,
        "the status is written before the stop"
    );

    let f = Fake {
        controls_down: true,
        ..Fake::direct(vec![agent("running")])
    };
    let (code, body) = cancel(&f, &[]).await;
    assert_eq!(code, 503, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("retry ham stop"),
        "{body}"
    );

    let f = Fake {
        agents_down: true,
        ..Fake::direct(vec![agent("running")])
    };
    assert_eq!(cancel(&f, &[]).await.0, 503);
}

/// A store that has not implemented the answer route's reads reports itself unavailable, so the route answers 503
/// instead of guessing: the default bodies of the four `Store` methods.
#[tokio::test]
async fn a_store_without_the_answer_reads_is_unavailable() {
    let f = Fake::direct(vec![agent("running")]);
    let store = &f;
    assert!(matches!(
        store.get_event(RUN, 0, "e").await,
        Err(StoreError::Unavailable(_))
    ));
    assert!(matches!(
        store.get_control(RUN, "direct", "c").await,
        Err(StoreError::Unavailable(_))
    ));
    assert!(matches!(
        store.rearm_control(RUN, "direct", "c").await,
        Err(StoreError::Unavailable(_))
    ));
    assert!(matches!(
        store.latest_events(RUN, 0, 10).await,
        Err(StoreError::Unavailable(_))
    ));
}
