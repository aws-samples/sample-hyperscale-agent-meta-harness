//! `POST /v1/runs/{runId}/input`: the answer of a user to the question a stuck Cline raised. The route validates the body,
//! checks the owner, the run, the live agent and that the request id names a blocking `needs-input` event of the direct agent,
//! and queues one `Instruct` control whose id IS the request id (one answer per request).
//!
//! The events and bodies are plain JSON (the golden bytes of `fixtures/direct-input/`), so these tests pin the wire and not
//! the Rust names of the protocol types.

#![allow(clippy::unwrap_used)]

use control_api::handler::{
    Blobs, DeployedAs, Deps, Execution, RunUsage, Store, StoreError, V1Request, handle,
    handle_logged,
};
use control_api::ids::rfc3339;
use harness_protocol::api::v1::DownloadUrlResponse;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::{
    AgentItem, ArtifactItem, ControlItem, GoalItem, IdempotencyItem, RunItem, event_shard,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ME: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
const OTHER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/someone";
const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const RUN2: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z";
/// The request id: the `eventId` of the `needs-input` event.
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const OPUS: &str = "us.anthropic.claude-opus-5-5";
const TEXT: &str =
    "Run the sleeps in the background and poll the log file instead of waiting on the command.";

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// RFC 3339 of now plus `delta` seconds.
fn at(delta: i64) -> String {
    rfc3339(u64::try_from(now() + delta).unwrap())
}

fn run_item(id: &str, status: &str) -> RunItem {
    serde_json::from_value(json!({
        "runId": id, "ownerPrincipalArn": ME, "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": status, "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-10-15T13:00:00Z", "version": 3
    }))
    .unwrap()
}

fn agent(status: &str) -> AgentItem {
    serde_json::from_value(json!({
        "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:700000000011:runtime/hamh_direct_harness_worker_python-x",
        "runtimeSessionId": format!("{RUN}-direct-g1"), "generation": 1, "engine": "cline",
        "engineVersion": "3.0.71", "transport": "acp", "modelId": OPUS,
        "pool": "direct", "status": status, "version": 1
    }))
    .unwrap()
}

/// The value of a `needs-input` event, `expires_in` seconds from now.
fn needs_input_value(blocking: bool, expires_in: i64) -> Value {
    json!({
        "turn": 1, "kind": "needs-input",
        "text": "Needs your input: Cline stopped its own prompt (ham say, ham new-session or ham finish)",
        "requestId": REQ, "n": 1, "reason": "engine_stop", "title": "Cline stopped its own prompt",
        "stopOutcome": "stopped_engine_cancelled",
        "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                     "lastError": null, "runS": 412, "sessions": 1},
        "lastMessage": "I will keep polling the build until it finishes.",
        "files": ["PROGRESS.md"], "filesTotal": 1, "blocking": blocking,
        "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
        "models": [{"id": OPUS, "name": "Claude Opus 5.5", "contextTokens": 1_000_000}],
        "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": at(expires_in)
    })
}

fn progress(event_id: &str, run: &str, agent_id: &str, value: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: event_id.into(),
        run_id: run.into(),
        task_id: None,
        agent_id: Some(agent_id.into()),
        seq: 1,
        ts: at(0),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline { value },
    }
}

fn needs_input(blocking: bool, expires_in: i64) -> Envelope {
    progress(REQ, RUN, "direct", needs_input_value(blocking, expires_in))
}

/// An acknowledgement or closing event of the request, sorted after it.
fn after_request(kind: &str) -> Envelope {
    progress(
        "01M5A2R7K3C9XJ4D8E6FQ0WT2A",
        RUN,
        "direct",
        json!({"turn": 1, "kind": kind, "text": "x", "requestId": REQ, "action": "continue_with_prompt", "origin": "user"}),
    )
}

struct Fake {
    runs: Mutex<BTreeMap<String, RunItem>>,
    agents: Vec<AgentItem>,
    events: Mutex<Vec<Envelope>>,
    controls: Mutex<Vec<ControlItem>>,
    /// Queuing a control fails.
    controls_down: bool,
    /// Reading the agent fails.
    agents_down: bool,
    /// Reading one event fails.
    event_down: bool,
    /// The tail read of the run list fails.
    tail_down: bool,
    /// Reading events after a cursor fails.
    scan_down: bool,
    /// Another caller removed `deliveredAt` first: the conditional re-arm finds nothing to remove.
    rearm_lost: bool,
    /// Re-arming fails.
    rearm_down: bool,
    /// Reading a control fails.
    control_read_down: bool,
    /// `put_control` reports a conflict although no control is stored.
    ghost_conflict: bool,
    /// Calls of `latest_events`.
    tail_reads: AtomicUsize,
    /// Calls of `rearm_control`.
    rearms: AtomicUsize,
}

impl Fake {
    /// An active run of `ME` with a live direct agent and one open blocking question that expires in `expires_in` seconds.
    fn open(expires_in: i64) -> Fake {
        let f = Fake::bare(vec![agent("running")]);
        f.events.lock().unwrap().push(needs_input(true, expires_in));
        f
    }
    fn bare(agents: Vec<AgentItem>) -> Fake {
        Fake {
            runs: Mutex::new(BTreeMap::from([(RUN.to_string(), run_item(RUN, "active"))])),
            agents,
            events: Mutex::default(),
            controls: Mutex::default(),
            controls_down: false,
            agents_down: false,
            event_down: false,
            tail_down: false,
            scan_down: false,
            rearm_lost: false,
            rearm_down: false,
            control_read_down: false,
            ghost_conflict: false,
            tail_reads: AtomicUsize::new(0),
            rearms: AtomicUsize::new(0),
        }
    }
    fn set_status(&self, status: &str) {
        self.runs
            .lock()
            .unwrap()
            .insert(RUN.into(), run_item(RUN, status));
    }
    /// Marks the stored control as taken by agentd `secs_ago` seconds ago.
    fn delivered(&self, secs_ago: i64) {
        self.controls.lock().unwrap()[0].delivered_at = Some(at(-secs_ago));
    }
    fn queued(&self) -> Vec<Value> {
        self.controls
            .lock()
            .unwrap()
            .iter()
            .map(|c| serde_json::to_value(&c.control).unwrap())
            .collect()
    }
}

impl Store for &Fake {
    async fn claim_idempotency(&self, c: &IdempotencyItem) -> Result<IdempotencyItem, StoreError> {
        Ok(c.clone())
    }
    async fn create_run(&self, _: &RunItem, _: &GoalItem, _: &Envelope) -> Result<(), StoreError> {
        Ok(())
    }
    async fn get_run(&self, id: &str) -> Result<Option<RunItem>, StoreError> {
        Ok(self.runs.lock().unwrap().get(id).cloned())
    }
    async fn get_root_goal(&self, _: &str) -> Result<Option<GoalItem>, StoreError> {
        Ok(None)
    }
    async fn list_runs(
        &self,
        owner: &str,
        _: Option<&str>,
    ) -> Result<(Vec<RunItem>, Option<String>), StoreError> {
        let mut out: Vec<RunItem> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.owner_principal_arn == owner)
            .cloned()
            .collect();
        out.sort_by(|a, b| b.run_id.cmp(&a.run_id));
        Ok((out, None))
    }
    async fn events(
        &self,
        run_id: &str,
        shard: u8,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        if self.scan_down {
            return Err(StoreError::Unavailable("events throttled".into()));
        }
        let mut out: Vec<Envelope> = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| {
                e.run_id == run_id && event_shard(e.authority, e.agent_id.as_deref()) == shard
            })
            .filter(|e| after.is_none_or(|a| e.event_id.as_str() > a))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        out.truncate(limit);
        Ok(out)
    }
    async fn list_artifacts(&self, _: &str) -> Result<Vec<ArtifactItem>, StoreError> {
        Ok(vec![])
    }
    async fn get_artifact(&self, _: &str, _: &str) -> Result<Option<ArtifactItem>, StoreError> {
        Ok(None)
    }
    async fn run_usage(&self, _: &str) -> Result<RunUsage, StoreError> {
        Ok(RunUsage::default())
    }
    async fn list_agents(&self, _: &str) -> Result<Vec<AgentItem>, StoreError> {
        if self.agents_down {
            return Err(StoreError::Unavailable("agents throttled".into()));
        }
        Ok(self.agents.clone())
    }
    async fn put_control(&self, c: &ControlItem) -> Result<(), StoreError> {
        if self.controls_down {
            return Err(StoreError::Unavailable("throttled".into()));
        }
        if self.ghost_conflict {
            return Err(StoreError::Conflict);
        }
        let mut g = self.controls.lock().unwrap();
        if g.iter()
            .any(|x| x.agent_id == c.agent_id && x.control_id == c.control_id)
        {
            return Err(StoreError::Conflict);
        }
        g.push(c.clone());
        Ok(())
    }
    async fn get_event(
        &self,
        run_id: &str,
        shard: u8,
        event_id: &str,
    ) -> Result<Option<Envelope>, StoreError> {
        if self.event_down {
            return Err(StoreError::Unavailable("event read throttled".into()));
        }
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .find(|e| {
                e.run_id == run_id
                    && e.event_id == event_id
                    && event_shard(e.authority, e.agent_id.as_deref()) == shard
            })
            .cloned())
    }
    async fn get_control(
        &self,
        run_id: &str,
        agent_id: &str,
        control_id: &str,
    ) -> Result<Option<ControlItem>, StoreError> {
        if self.control_read_down {
            return Err(StoreError::Unavailable("control read throttled".into()));
        }
        Ok(self
            .controls
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.run_id == run_id && c.agent_id == agent_id && c.control_id == control_id)
            .cloned())
    }
    async fn rearm_control(
        &self,
        run_id: &str,
        agent_id: &str,
        control_id: &str,
    ) -> Result<bool, StoreError> {
        self.rearms.fetch_add(1, Ordering::SeqCst);
        if self.rearm_down {
            return Err(StoreError::Unavailable("rearm throttled".into()));
        }
        if self.rearm_lost {
            return Ok(false);
        }
        let mut g = self.controls.lock().unwrap();
        let Some(c) = g
            .iter_mut()
            .find(|c| c.run_id == run_id && c.agent_id == agent_id && c.control_id == control_id)
        else {
            return Ok(false);
        };
        Ok(c.delivered_at.take().is_some())
    }
    async fn latest_events(
        &self,
        run_id: &str,
        shard: u8,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        self.tail_reads.fetch_add(1, Ordering::SeqCst);
        if self.tail_down {
            return Err(StoreError::Unavailable("tail read throttled".into()));
        }
        let mut all: Vec<Envelope> = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| {
                e.run_id == run_id && event_shard(e.authority, e.agent_id.as_deref()) == shard
            })
            .cloned()
            .collect();
        all.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        let skip = all.len().saturating_sub(limit);
        Ok(all.split_off(skip))
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

fn request(caller: &str, method: &str, path: &str, body: &str) -> V1Request {
    V1Request {
        method: method.into(),
        path: path.into(),
        caller_arn: Some(caller.into()),
        source_ip: None,
        headers: BTreeMap::new(),
        query: BTreeMap::new(),
        body: body.into(),
    }
}

async fn send_as(f: &Fake, caller: &str, run: &str, body: &str) -> (u16, Value) {
    let out = handle(
        request(caller, "POST", &format!("/v1/runs/{run}/input"), body),
        &deps(f),
    )
    .await;
    (out.status, serde_json::from_str(&out.body).unwrap())
}

async fn send(f: &Fake, body: &Value) -> (u16, Value) {
    send_as(f, ME, RUN, &body.to_string()).await
}

fn say(text: &str) -> Value {
    json!({"requestId": REQ, "action": "continue_with_prompt", "text": text})
}

fn control_of(action: &str, text: Option<&str>, model: Option<&str>) -> Value {
    let mut v = json!({"kind": "instruct", "requestId": REQ, "action": action, "origin": "user"});
    if let Some(t) = text {
        v["text"] = json!(t);
    }
    if let Some(m) = model {
        v["model"] = json!(m);
    }
    v
}

// ---- the happy paths ----

#[tokio::test]
async fn an_answer_queues_one_instruct_control_for_the_direct_agent_with_the_request_id_as_its_id()
{
    let f = Fake::open(1800);
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 202, "{v}");
    assert_eq!(v["state"], "queued");
    assert_eq!(v["runId"], RUN);
    assert_eq!(v["requestId"], REQ);
    let controls = f.controls.lock().unwrap();
    assert_eq!(controls.len(), 1);
    assert_eq!(controls[0].run_id, RUN);
    assert_eq!(controls[0].agent_id, "direct");
    assert_eq!(controls[0].control_id, REQ, "the request id is the nonce");
    assert_eq!(controls[0].delivered_at, None);
    drop(controls);
    assert_eq!(
        f.queued(),
        vec![control_of("continue_with_prompt", Some(TEXT), None)]
    );
}

#[tokio::test]
async fn each_action_is_queued_with_exactly_its_own_members() {
    let cases = [
        (
            json!({"requestId": REQ, "action": "new_session_with_prompt", "text": TEXT}),
            control_of("new_session_with_prompt", Some(TEXT), None),
        ),
        (
            json!({"requestId": REQ, "action": "new_session_with_prompt"}),
            control_of("new_session_with_prompt", None, None),
        ),
        (
            json!({"requestId": REQ, "action": "try_model", "model": OPUS}),
            control_of("try_model", None, Some(OPUS)),
        ),
        (
            json!({"requestId": REQ, "action": "try_model", "model": OPUS, "text": TEXT}),
            control_of("try_model", Some(TEXT), Some(OPUS)),
        ),
        (
            json!({"requestId": REQ, "action": "finish"}),
            control_of("finish", None, None),
        ),
    ];
    for (body, want) in cases {
        let f = Fake::open(1800);
        let (status, v) = send(&f, &body).await;
        assert_eq!(status, 202, "{body}: {v}");
        assert_eq!(f.queued(), vec![want], "{body}");
    }
}

#[tokio::test]
async fn an_alternate_route_of_a_listed_model_is_a_valid_try_model_target() {
    // The question's `models` list is the run's routes, alternates included; the catalog check must accept what the question offers.
    let f = Fake::open(1800);
    let body = json!({"requestId": REQ, "action": "try_model", "model": "global.anthropic.claude-opus-5-5"});
    let (status, v) = send(&f, &body).await;
    assert_eq!(status, 202, "{v}");
}

#[tokio::test]
async fn the_text_limit_is_262144_bytes_and_there_is_no_character_count() {
    // The old limits were 8,000 characters and 32,000 bytes.
    let f = Fake::open(1800);
    let (status, v) = send(&f, &say(&"a".repeat(8_001))).await;
    assert_eq!(status, 202, "{v}");
    let f = Fake::open(1800);
    let (status, v) = send(&f, &say(&"a".repeat(262_144))).await;
    assert_eq!(status, 202, "{v}");
    let f = Fake::open(1800);
    let (status, v) = send(&f, &say(&"a".repeat(262_145))).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], "bad-input");
    assert!(v["message"].as_str().unwrap().contains("262144"), "{v}");
    assert!(f.queued().is_empty());
    // Four-byte characters count by their bytes: 65,536 of them are exactly the limit.
    let f = Fake::open(1800);
    let (status, v) = send(&f, &say(&"\u{1F600}".repeat(65_536))).await;
    assert_eq!(status, 202, "{v}");
    let f = Fake::open(1800);
    let (status, _) = send(&f, &say(&"\u{1F600}".repeat(65_537))).await;
    assert_eq!(status, 400);
}

// ---- the body ----

#[tokio::test]
async fn bad_bodies_are_400_and_queue_nothing() {
    let bad_input: Vec<Value> = vec![
        json!({"requestId": REQ, "action": "continue_with_prompt"}),
        json!({"requestId": REQ, "action": "continue_with_prompt", "text": "   \n"}),
        json!({"requestId": REQ, "action": "new_session_with_prompt", "text": ""}),
        json!({"requestId": REQ, "action": "finish", "text": "bye"}),
        json!({"requestId": REQ, "action": "finish", "model": OPUS}),
        json!({"requestId": REQ, "action": "try_model"}),
        json!({"requestId": REQ, "action": "try_model", "model": "  "}),
        json!({"requestId": REQ, "action": "continue_with_prompt", "text": TEXT, "model": OPUS}),
        json!({"requestId": REQ, "action": "new_session_with_prompt", "model": OPUS}),
    ];
    for body in bad_input {
        let f = Fake::open(1800);
        let (status, v) = send(&f, &body).await;
        assert_eq!(status, 400, "{body}: {v}");
        assert_eq!(v["code"], "bad-input", "{body}");
        assert!(f.queued().is_empty(), "{body}");
    }
    let bad_request: Vec<String> = vec![
        "not json".into(),
        "{}".into(),
        json!({"requestId": "not-a-ulid", "action": "finish"}).to_string(),
        json!({"requestId": REQ, "action": "dance"}).to_string(),
        json!({"requestId": REQ, "action": "finish", "surprise": 1}).to_string(),
    ];
    for body in bad_request {
        let f = Fake::open(1800);
        let (status, v) = send_as(&f, ME, RUN, &body).await;
        assert_eq!(status, 400, "{body}: {v}");
        assert_eq!(v["code"], "bad-request", "{body}");
        assert!(f.queued().is_empty(), "{body}");
    }
}

#[tokio::test]
async fn origin_is_not_client_settable() {
    // The body refuses the member, so a client cannot claim to be the analyzer; control-api writes `user`.
    let f = Fake::open(1800);
    let mut body = say(TEXT);
    body["origin"] = json!("analyzer");
    let (status, v) = send(&f, &body).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], "bad-request");
    assert!(f.queued().is_empty());
    let (status, _) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 202);
    assert_eq!(f.queued()[0]["origin"], "user");
}

#[tokio::test]
async fn try_model_outside_the_catalog_is_422_and_queues_nothing() {
    for model in ["nope", "us.anthropic.claude-opus-9", "opus"] {
        let f = Fake::open(1800);
        let body = json!({"requestId": REQ, "action": "try_model", "model": model});
        let (status, v) = send(&f, &body).await;
        assert_eq!(status, 422, "{model}: {v}");
        assert_eq!(v["code"], "unknown-model", "{model}");
        assert!(f.queued().is_empty(), "{model}");
    }
}

// ---- who, which run, which question ----

#[tokio::test]
async fn another_callers_run_and_a_bad_run_id_are_404() {
    let f = Fake::open(1800);
    let (status, v) = send_as(&f, OTHER, RUN, &say(TEXT).to_string()).await;
    assert_eq!(status, 404, "{v}");
    assert_eq!(v["code"], "not-found");
    for run in ["not-a-ulid", "01K5ZQ4T8M3N6P7R9S0V1W2X9Z"] {
        let (status, v) = send_as(&f, ME, run, &say(TEXT).to_string()).await;
        assert_eq!(status, 404, "{run}: {v}");
        assert_eq!(v["code"], "not-found", "{run}");
    }
    assert!(f.queued().is_empty());
}

#[tokio::test]
async fn a_finished_run_is_409_run_finished() {
    for status_word in ["complete", "cleared"] {
        let f = Fake::open(1800);
        f.set_status(status_word);
        let (status, v) = send(&f, &say(TEXT)).await;
        assert_eq!(status, 409, "{status_word}: {v}");
        assert_eq!(v["code"], "run-finished", "{status_word}");
        assert!(f.queued().is_empty());
    }
}

#[tokio::test]
async fn a_run_with_no_live_agent_is_409_no_live_session() {
    for agents in [vec![], vec![agent("stopped")], vec![agent("failed")]] {
        let f = Fake::bare(agents);
        f.events.lock().unwrap().push(needs_input(true, 1800));
        let (status, v) = send(&f, &say(TEXT)).await;
        assert_eq!(status, 409, "{v}");
        assert_eq!(v["code"], "no-live-session");
        assert!(f.queued().is_empty());
    }
}

#[tokio::test]
async fn a_request_id_that_names_no_blocking_question_of_the_direct_agent_is_404_no_such_request() {
    // No event at all: a run on an image that cannot raise the question never gets an Instruct.
    let f = Fake::bare(vec![agent("running")]);
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 404, "{v}");
    assert_eq!(v["code"], "no-such-request");
    let events: Vec<(&str, Envelope)> = vec![
        ("a non-blocking notice", needs_input(false, 1800)),
        (
            "another progress kind",
            progress(
                REQ,
                RUN,
                "direct",
                json!({"turn": 1, "kind": "provider-retry", "text": "x"}),
            ),
        ),
        (
            "another agent",
            progress(REQ, RUN, "planner", needs_input_value(true, 1800)),
        ),
        (
            "another run",
            progress(REQ, RUN2, "direct", needs_input_value(true, 1800)),
        ),
        (
            "a harness event",
            Envelope {
                authority: Authority::Harness,
                agent_id: None,
                ..needs_input(true, 1800)
            },
        ),
        (
            "an event whose payload is not a question",
            progress(
                REQ,
                RUN,
                "direct",
                json!({"kind": "needs-input", "blocking": "yes"}),
            ),
        ),
    ];
    for (what, event) in events {
        let f = Fake::bare(vec![agent("running")]);
        f.events.lock().unwrap().push(event);
        let (status, v) = send(&f, &say(TEXT)).await;
        assert_eq!(status, 404, "{what}: {v}");
        assert_eq!(v["code"], "no-such-request", "{what}");
        assert!(f.queued().is_empty(), "{what}");
    }
}

#[tokio::test]
async fn an_answer_to_a_question_of_an_earlier_round_is_refused() {
    // Round 1 closed and round 2 is open under another id: the old id is still a real event, but its window is long gone.
    let f = Fake::bare(vec![agent("running")]);
    f.events.lock().unwrap().push(needs_input(true, -3600));
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 410, "{v}");
    assert_eq!(v["code"], "request-expired");
    assert!(f.queued().is_empty());
}

// ---- the window ----

#[tokio::test]
async fn the_window_is_open_30_seconds_past_expires_at() {
    let f = Fake::open(-20);
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 202, "{v}");
    let f = Fake::open(-40);
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 410, "{v}");
    assert_eq!(v["code"], "request-expired");
    assert!(v["message"].as_str().unwrap().contains("closed at"), "{v}");
}

#[tokio::test]
async fn a_repeat_of_an_answer_that_is_already_queued_still_reads_as_queued_after_the_window_closed()
 {
    let f = Fake::open(1800);
    let (status, _) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 202);
    // The same answer arrives again after the window and its grace: the stored control decides, not the clock.
    f.events.lock().unwrap()[0] = needs_input(true, -3600);
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["state"], "already-queued");
    let (status, v) = send(&f, &json!({"requestId": REQ, "action": "finish"})).await;
    assert_eq!(status, 409, "{v}");
    assert_eq!(v["code"], "already-answered");
}

// ---- races ----

#[tokio::test]
async fn two_terminals_one_wins_and_the_other_is_told_what_won() {
    let f = Fake::open(1800);
    let (a, _) = send(&f, &say(TEXT)).await;
    assert_eq!(a, 202);
    let (b, v) = send(
        &f,
        &json!({"requestId": REQ, "action": "new_session_with_prompt"}),
    )
    .await;
    assert_eq!(b, 409, "{v}");
    assert_eq!(v["code"], "already-answered");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("continue_with_prompt"),
        "names the winning action: {v}"
    );
    // The same action with other text is a different answer too.
    let (c, v) = send(&f, &say("something else")).await;
    assert_eq!(c, 409, "{v}");
    assert_eq!(f.queued().len(), 1, "one answer per request");
    assert_eq!(
        f.queued()[0],
        control_of("continue_with_prompt", Some(TEXT), None)
    );
}

#[tokio::test]
async fn the_same_post_repeated_after_a_network_error_is_idempotent() {
    let f = Fake::open(1800);
    let (a, v) = send(&f, &say(TEXT)).await;
    assert_eq!((a, v["state"].as_str()), (202, Some("queued")));
    let (b, v) = send(&f, &say(TEXT)).await;
    assert_eq!(
        (b, v["state"].as_str()),
        (200, Some("already-queued")),
        "{v}"
    );
    assert_eq!(f.queued().len(), 1);
    assert_eq!(f.rearms.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_answer_agentd_took_but_never_acknowledged_is_re_armed_by_the_resend() {
    let f = Fake::open(1800);
    send(&f, &say(TEXT)).await;
    f.delivered(20);
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 202, "{v}");
    assert_eq!(v["state"], "re-queued");
    assert_eq!(f.rearms.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.controls.lock().unwrap()[0].delivered_at,
        None,
        "deliveredAt is removed, so agentd's next events post takes it again"
    );
    assert_eq!(f.queued().len(), 1, "no second control");
}

#[tokio::test]
async fn a_control_delivered_less_than_12_seconds_ago_is_left_alone() {
    let f = Fake::open(1800);
    send(&f, &say(TEXT)).await;
    f.delivered(5);
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(
        (status, v["state"].as_str()),
        (200, Some("already-queued")),
        "{v}"
    );
    assert_eq!(f.rearms.load(Ordering::SeqCst), 0);
    assert!(f.controls.lock().unwrap()[0].delivered_at.is_some());
}

#[tokio::test]
async fn an_acknowledged_answer_is_not_re_armed() {
    for kind in ["input-received", "input-ignored", "input-closed"] {
        let f = Fake::open(1800);
        send(&f, &say(TEXT)).await;
        f.delivered(60);
        f.events.lock().unwrap().push(after_request(kind));
        let (status, v) = send(&f, &say(TEXT)).await;
        assert_eq!(
            (status, v["state"].as_str()),
            (200, Some("already-queued")),
            "{kind}: {v}"
        );
        assert_eq!(f.rearms.load(Ordering::SeqCst), 0, "{kind}");
    }
    // An acknowledgement of another request does not count.
    let f = Fake::open(1800);
    send(&f, &say(TEXT)).await;
    f.delivered(60);
    let mut other = after_request("input-received");
    other.payload = Payload::Inline {
        value: json!({"turn": 1, "kind": "input-received", "requestId": "01M5B8T4W2N6QJ3C7D9EH1FZ0A"}),
    };
    f.events.lock().unwrap().push(other);
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(
        (status, v["state"].as_str()),
        (202, Some("re-queued")),
        "{v}"
    );
}

#[tokio::test]
async fn a_re_arm_another_caller_got_to_first_reads_as_already_queued() {
    let mut f = Fake::open(1800);
    send(&f, &say(TEXT)).await;
    f.delivered(60);
    // The conditional update finds deliveredAt already gone: someone else queued the control again.
    f.rearm_lost = true;
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(
        (status, v["state"].as_str()),
        (200, Some("already-queued")),
        "{v}"
    );
    assert_eq!(f.rearms.load(Ordering::SeqCst), 1);
}

// ---- store failures ----

#[tokio::test]
async fn store_failures_are_503_and_queue_nothing() {
    let mut f = Fake::open(1800);
    f.controls_down = true;
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["code"], "unavailable");
    let mut f = Fake::open(1800);
    f.agents_down = true;
    assert_eq!(send(&f, &say(TEXT)).await.0, 503);
    let mut f = Fake::open(1800);
    f.event_down = true;
    assert_eq!(send(&f, &say(TEXT)).await.0, 503);
    // The resend path reads events after the cursor.
    let mut f = Fake::open(1800);
    send(&f, &say(TEXT)).await;
    f.delivered(60);
    f.scan_down = true;
    assert_eq!(send(&f, &say(TEXT)).await.0, 503);
    assert_eq!(f.rearms.load(Ordering::SeqCst), 0);
    // A diagnostic goes to the log, never to the caller.
    let mut f = Fake::open(1800);
    f.controls_down = true;
    let (resp, logs) = handle_logged(
        request(
            ME,
            "POST",
            &format!("/v1/runs/{RUN}/input"),
            &say(TEXT).to_string(),
        ),
        &deps(&f),
    )
    .await;
    assert!(!resp.body.contains("throttled"), "{}", resp.body);
    assert!(logs.iter().any(|l| l.contains("throttled")), "{logs:?}");
}

#[tokio::test]
async fn other_methods_on_the_input_route_are_405() {
    let f = Fake::open(1800);
    for method in ["GET", "PUT", "DELETE"] {
        let out = handle(
            request(ME, method, &format!("/v1/runs/{RUN}/input"), ""),
            &deps(&f),
        )
        .await;
        assert_eq!(out.status, 405, "{method}");
    }
}

// ---- GET /v1/runs: the needsInput mark ----

async fn list(f: &Fake) -> (u16, Value) {
    let out = handle(request(ME, "GET", "/v1/runs", ""), &deps(f)).await;
    (out.status, serde_json::from_str(&out.body).unwrap())
}

#[tokio::test]
async fn an_active_run_with_an_open_question_lists_its_needs_input_brief() {
    let f = Fake::open(1800);
    let (status, v) = list(&f).await;
    assert_eq!(status, 200, "{v}");
    let mark = &v["runs"][0]["needsInput"];
    assert_eq!(mark["requestId"], REQ, "{v}");
    assert_eq!(mark["reason"], "engine_stop");
    assert_eq!(mark["title"], "Cline stopped its own prompt");
    assert!(mark["expiresAt"].is_string());
    assert_eq!(
        mark.as_object().unwrap().len(),
        4,
        "requestId, reason, title, expiresAt and nothing else: {mark}"
    );
}

#[tokio::test]
async fn no_mark_for_an_answered_question_a_notice_or_a_run_that_is_not_active() {
    // Answered (input-received closes it): the tail is read and holds no open question.
    let f = Fake::open(1800);
    f.events
        .lock()
        .unwrap()
        .push(after_request("input-received"));
    assert!(list(&f).await.1["runs"][0].get("needsInput").is_none());
    assert_eq!(f.tail_reads.load(Ordering::SeqCst), 1);
    // The non-blocking outage notice is not a question.
    let f = Fake::bare(vec![agent("running")]);
    f.events.lock().unwrap().push(needs_input(false, 1800));
    assert!(list(&f).await.1["runs"][0].get("needsInput").is_none());
    assert_eq!(f.tail_reads.load(Ordering::SeqCst), 1);
    // Not active: the event log is not even read.
    let f = Fake::open(1800);
    f.set_status("complete");
    assert!(list(&f).await.1["runs"][0].get("needsInput").is_none());
    assert_eq!(f.tail_reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_tail_read_is_of_the_direct_agents_shard_and_a_failure_never_fails_the_list() {
    // An open question on another agent's shard is not read.
    let f = Fake::bare(vec![agent("running")]);
    f.events
        .lock()
        .unwrap()
        .push(progress(REQ, RUN, "planner", needs_input_value(true, 1800)));
    let (status, v) = list(&f).await;
    assert_eq!(status, 200);
    assert!(v["runs"][0].get("needsInput").is_none());
    assert_eq!(f.tail_reads.load(Ordering::SeqCst), 1);
    // The read fails: the list is still 200, the field is absent, the reason goes to the log.
    let mut f = Fake::open(1800);
    f.tail_down = true;
    let (resp, logs) = handle_logged(request(ME, "GET", "/v1/runs", ""), &deps(&f)).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let v: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(v["runs"].as_array().unwrap().len(), 1);
    assert!(v["runs"][0].get("needsInput").is_none());
    assert!(
        logs.iter().any(|l| l.contains("tail read throttled")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn the_mark_looks_at_the_latest_events_only() {
    // 300 later events on the same shard: the open question is still in the tail the list reads.
    let f = Fake::open(1800);
    for n in 0..150 {
        let id = format!("01M5A2R7K3C9XJ4D8E6FQ1{n:04}");
        f.events.lock().unwrap().push(progress(
            &id,
            RUN,
            "direct",
            json!({"turn": 1, "kind": "provider-retry", "text": "retrying"}),
        ));
    }
    let (_, v) = list(&f).await;
    assert_eq!(v["runs"][0]["needsInput"]["requestId"], REQ, "{v}");
}

// ---- the failure and defensive branches of the answer path ----

#[tokio::test]
async fn a_conflict_whose_control_cannot_be_read_back_is_503() {
    // The conditional put lost, and the read of the winner is absent or fails: nothing can be said about the answer.
    let mut f = Fake::open(1800);
    f.ghost_conflict = true;
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["code"], "unavailable");
    let mut f = Fake::open(1800);
    send(&f, &say(TEXT)).await;
    f.control_read_down = true;
    assert_eq!(send(&f, &say(TEXT)).await.0, 503);
    // The same read after the window closed.
    f.events.lock().unwrap()[0] = needs_input(true, -3600);
    assert_eq!(send(&f, &say(TEXT)).await.0, 503);
    assert_eq!(f.queued().len(), 1);
}

#[tokio::test]
async fn a_failed_re_arm_is_503() {
    let mut f = Fake::open(1800);
    send(&f, &say(TEXT)).await;
    f.delivered(60);
    f.rearm_down = true;
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(f.rearms.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_control_of_another_kind_at_the_request_id_is_another_answer() {
    // A Cancel stored under the request id (cannot happen through this route) is never mistaken for the user's answer.
    let f = Fake::open(1800);
    f.controls.lock().unwrap().push(ControlItem {
        run_id: RUN.into(),
        agent_id: "direct".into(),
        control_id: REQ.into(),
        control: harness_protocol::api::internal::PendingControl::Cancel,
        created_at: at(0),
        delivered_at: None,
    });
    let (status, v) = send(&f, &say(TEXT)).await;
    assert_eq!(status, 409, "{v}");
    assert_eq!(v["code"], "already-answered");
    assert!(
        v["message"].as_str().unwrap().contains("another answer"),
        "{v}"
    );
}

/// A store that implements only what every store must: the four methods of protocol 1.37.0 answer unavailable.
struct Bare;

impl Store for &Bare {
    async fn claim_idempotency(&self, c: &IdempotencyItem) -> Result<IdempotencyItem, StoreError> {
        Ok(c.clone())
    }
    async fn create_run(&self, _: &RunItem, _: &GoalItem, _: &Envelope) -> Result<(), StoreError> {
        Ok(())
    }
    async fn get_run(&self, _: &str) -> Result<Option<RunItem>, StoreError> {
        Ok(None)
    }
    async fn get_root_goal(&self, _: &str) -> Result<Option<GoalItem>, StoreError> {
        Ok(None)
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
    async fn put_control(&self, _: &ControlItem) -> Result<(), StoreError> {
        Ok(())
    }
}

#[tokio::test]
async fn a_store_that_does_not_implement_the_new_reads_reports_itself_unavailable() {
    let bare = &Bare;
    let shard = event_shard(Authority::Agent, Some("direct"));
    assert!(matches!(
        bare.get_event(RUN, shard, REQ).await,
        Err(StoreError::Unavailable(_))
    ));
    assert!(matches!(
        bare.get_control(RUN, "direct", REQ).await,
        Err(StoreError::Unavailable(_))
    ));
    assert!(matches!(
        bare.rearm_control(RUN, "direct", REQ).await,
        Err(StoreError::Unavailable(_))
    ));
    assert!(matches!(
        bare.latest_events(RUN, shard, 10).await,
        Err(StoreError::Unavailable(_))
    ));
}
