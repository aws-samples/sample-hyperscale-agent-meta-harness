//! Messages to a running Cline: `POST /v1/runs/{runId}/messages`, `GET.../messages`, `GET.../messages/{messageId}`
//! and `POST.../messages/{messageId}/withdraw` through the real handler over the real ledger on the in-memory table. The order of the
//! checks, the status and code of every refusal, the owner scoping, the idempotent retry, and the cancel that closes the run to messages.

#![allow(clippy::unwrap_used)]

use control_api::handler::{
    Blobs, DeployedAs, Deps, Execution, RunUsage, Store, StoreError, V1Request, handle,
    handle_logged,
};
use harness_protocol::api::v1::DownloadUrlResponse;
use harness_protocol::common::Engine;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::messages::{
    Admission, MessageOutcome, MessageState, MessagesPage, RejectReason, SendMessageResponse,
    SendOutcome,
};
use harness_protocol::store::{
    AgentItem, ArtifactItem, ControlItem, GoalItem, IdempotencyItem, RunItem, event_shard,
};
use message_ledger::mem::MemTable;
use message_ledger::{Admitted, CloseReason, Closed, Ledger, LedgerError, ManualClock};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

const ME: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
const OTHER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/someone";
const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const GEN: &str = "01M5G000000000000000000001";
/// 2026-10-16T14:02:11Z
const T0: i64 = 1_792_159_331;

fn mid(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn run_item(status: &str) -> RunItem {
    run_item_of(RUN, status)
}

/// A run of `ME` with the id `id`.
fn run_item_of(id: &str, status: &str) -> RunItem {
    serde_json::from_value(json!({
        "runId": id, "ownerPrincipalArn": ME, "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": status, "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-10-16T13:00:00Z", "version": 3
    }))
    .unwrap()
}

fn agent(status: &str, controls: &[&str]) -> AgentItem {
    serde_json::from_value(json!({
        "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:700000000011:runtime/hamh_direct_harness_worker_python-x",
        "runtimeSessionId": format!("{RUN}-direct-g1"), "generation": 1, "engine": "cline",
        "engineVersion": "3.0.71", "transport": "acp", "modelId": "us.anthropic.claude-sonnet-5-5",
        "pool": "direct", "status": status, "harnessControls": controls, "version": 1
    }))
    .unwrap()
}

fn question(blocking: bool) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: REQ.into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: 1,
        ts: "2026-10-16T14:00:00Z".into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline {
            value: json!({
                "turn": 1, "kind": "needs-input", "text": "Needs your input",
                "requestId": REQ, "n": 1, "reason": "engine_stop", "title": "Cline stopped its own prompt",
                "stopOutcome": "stopped_engine_cancelled",
                "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                             "lastError": null, "runS": 412, "sessions": 1},
                "lastMessage": "I will keep polling.", "files": [], "filesTotal": 0, "blocking": blocking,
                "options": ["continue_with_prompt", "finish"], "models": [], "recommended": null,
                "canContinue": true, "waitS": 1800, "expiresAt": "2099-01-01T00:00:00Z"
            }),
        },
    }
}

struct Fake {
    runs: Mutex<BTreeMap<String, RunItem>>,
    agents: Mutex<Vec<AgentItem>>,
    events: Mutex<Vec<Envelope>>,
    controls: Mutex<Vec<ControlItem>>,
    /// For each Cancel control queued, whether the ledger was already closed to messages at that moment.
    closed_at_cancel: Mutex<Vec<bool>>,
    table: MemTable,
    clock: ManualClock,
    /// The store keeps no ledger (the trait defaults answer).
    no_ledger: bool,
    /// The ledger's table fails every transaction.
    table_down: bool,
    /// Reading the open question fails.
    tail_down: bool,
}

impl Fake {
    /// An active run of `ME` with a live direct agent that registered both message controls.
    fn new() -> Fake {
        Fake {
            runs: Mutex::new(BTreeMap::from([(RUN.to_string(), run_item("active"))])),
            agents: Mutex::new(vec![agent("running", &["message.next", "message.now"])]),
            events: Mutex::default(),
            controls: Mutex::default(),
            closed_at_cancel: Mutex::default(),
            table: MemTable::new(),
            clock: ManualClock::at(T0),
            no_ledger: false,
            table_down: false,
            tail_down: false,
        }
    }
    fn ledger(&self) -> Ledger<MemTable, ManualClock> {
        Ledger::with_clock(self.table.clone(), self.clock.clone())
    }
    fn set_status(&self, s: &str) {
        self.runs.lock().unwrap().insert(RUN.into(), run_item(s));
    }
    fn set_agents(&self, a: Vec<AgentItem>) {
        *self.agents.lock().unwrap() = a;
    }
    fn down(&self) -> Option<LedgerError> {
        if self.no_ledger {
            return Some(LedgerError::Unavailable(
                "this store keeps no messages".into(),
            ));
        }
        if self.table_down {
            return Some(LedgerError::Unavailable("table down".into()));
        }
        None
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
    async fn run_usage(&self, _: &str) -> Result<RunUsage, StoreError> {
        Ok(RunUsage::default())
    }
    async fn list_agents(&self, _: &str) -> Result<Vec<AgentItem>, StoreError> {
        Ok(self.agents.lock().unwrap().clone())
    }
    async fn update_run(
        &self,
        run: &RunItem,
        _: Option<&GoalItem>,
        _: Option<&Envelope>,
    ) -> Result<(), StoreError> {
        self.runs
            .lock()
            .unwrap()
            .insert(run.run_id.clone(), run.clone());
        Ok(())
    }
    async fn put_control(&self, c: &ControlItem) -> Result<(), StoreError> {
        if matches!(
            c.control,
            harness_protocol::api::internal::PendingControl::Cancel
        ) {
            let closed = self
                .ledger()
                .list(RUN)
                .await
                .is_ok_and(|p| p.admission == Admission::Closed);
            self.closed_at_cancel.lock().unwrap().push(closed);
        }
        self.controls.lock().unwrap().push(c.clone());
        Ok(())
    }
    async fn latest_events(
        &self,
        run_id: &str,
        shard: u8,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
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
    async fn message_admit(
        &self,
        run_id: &str,
        principal: &str,
        req: &harness_protocol::messages::SendMessageRequest,
    ) -> Result<Admitted, LedgerError> {
        if let Some(e) = self.down() {
            return Err(e);
        }
        if self.table_down {
            return Err(LedgerError::Unavailable("table down".into()));
        }
        self.ledger().admit(run_id, principal, req).await
    }
    async fn message_get(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<Option<harness_protocol::messages::MessageView>, LedgerError> {
        if let Some(e) = self.down() {
            return Err(e);
        }
        self.ledger().get_message(run_id, message_id).await
    }
    async fn message_list(&self, run_id: &str) -> Result<MessagesPage, LedgerError> {
        if let Some(e) = self.down() {
            return Err(e);
        }
        self.ledger().list(run_id).await
    }
    async fn message_withdraw(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<harness_protocol::messages::MessageView, LedgerError> {
        if let Some(e) = self.down() {
            return Err(e);
        }
        self.ledger().withdraw(run_id, message_id).await
    }
    async fn message_close(
        &self,
        run_id: &str,
        reason: CloseReason,
    ) -> Result<Closed, LedgerError> {
        if self.no_ledger {
            return Ok(Closed::default());
        }
        if let Some(e) = self.down() {
            return Err(e);
        }
        self.ledger().close(run_id, reason).await
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
    async fn stop(&self, _: &str, _: &str) -> Result<(), StoreError> {
        Ok(())
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

async fn call(f: &Fake, caller: &str, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let b = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    let out = handle(request(caller, method, path, &b), &deps(f)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
    )
}

async fn post_message(f: &Fake, n: u32, text: &str) -> (u16, Value) {
    call(
        f,
        ME,
        "POST",
        &format!("/v1/runs/{RUN}/messages"),
        &json!({"messageId": mid(n), "text": text}),
    )
    .await
}

/// A message to the run `run`.
async fn post_to(f: &Fake, run: &str, n: u32, text: &str) -> (u16, Value) {
    call(
        f,
        ME,
        "POST",
        &format!("/v1/runs/{run}/messages"),
        &json!({"messageId": mid(n), "text": text}),
    )
    .await
}

/// Withdraws message `n` of the run `run` (it frees one of the three slots) and returns the status.
async fn withdraw_from(f: &Fake, run: &str, n: u32) -> u16 {
    call(
        f,
        ME,
        "POST",
        &format!("/v1/runs/{run}/messages/{}/withdraw", mid(n)),
        &Value::Null,
    )
    .await
    .0
}

fn code(v: &Value) -> &str {
    v["code"].as_str().unwrap_or_default()
}

// ---- send -----------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_message_is_accepted_with_202_and_is_pending() {
    let f = Fake::new();
    let (status, body) = post_message(&f, 1, "please add a README section").await;
    assert_eq!(status, 202, "{body}");
    let r: SendMessageResponse = serde_json::from_value(body).unwrap();
    assert_eq!(r.outcome, SendOutcome::Accepted);
    assert_eq!(
        (r.message.state, r.message.seq, r.message.message_id),
        (MessageState::Pending, 1, mid(1))
    );
    assert_eq!(r.message.text, "please add a README section");
}

#[tokio::test]
async fn an_identical_retry_is_200_already_accepted_and_the_same_id_with_other_text_is_409() {
    let f = Fake::new();
    post_message(&f, 1, "same").await;
    let (status, body) = post_message(&f, 1, "same").await;
    assert_eq!(status, 200);
    assert_eq!(body["outcome"], "already-accepted");
    let (status, body) = post_message(&f, 1, "other").await;
    assert_eq!((status, code(&body)), (409, "idempotency-conflict"));
    let page: MessagesPage = serde_json::from_value(
        call(
            &f,
            ME,
            "GET",
            &format!("/v1/runs/{RUN}/messages"),
            &Value::Null,
        )
        .await
        .1,
    )
    .unwrap();
    assert_eq!((page.messages.len(), page.watermark), (1, 1));
}

#[tokio::test]
async fn a_retry_after_the_run_finished_returns_the_stored_message_not_run_finished() {
    let f = Fake::new();
    post_message(&f, 1, "just before the end").await;
    f.set_status("complete");
    let (status, body) = post_message(&f, 1, "just before the end").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["outcome"], "already-accepted");
    // a new message is refused
    let (status, body) = post_message(&f, 2, "new").await;
    assert_eq!((status, code(&body)), (409, "run-finished"));
}

#[tokio::test]
async fn the_body_is_checked_before_anything_is_read() {
    let f = Fake::new();
    let path = format!("/v1/runs/{RUN}/messages");
    for (body, want_code) in [
        (
            json!({"messageId": mid(1), "text": "x", "priority": 3}),
            "bad-request",
        ),
        (
            json!({"messageId": mid(1), "text": "x", "colour": "blue"}),
            "bad-request",
        ),
        (
            json!({"messageId": mid(1), "text": "x", "mode": "steer"}),
            "bad-request",
        ),
        (json!({"messageId": mid(1)}), "bad-request"),
        (json!({"messageId": "nope", "text": "x"}), "bad-input"),
        (json!({"messageId": mid(1), "text": "   "}), "bad-input"),
        (
            json!({"messageId": mid(1), "text": "a".repeat(262_145)}),
            "bad-input",
        ),
        (
            json!({"messageId": mid(1), "text": "x", "confirms": {"messageId": mid(1), "epoch": "s"}}),
            "bad-input",
        ),
    ] {
        let (status, v) = call(&f, ME, "POST", &path, &body).await;
        assert_eq!((status, code(&v)), (400, want_code), "{body}");
    }
    let out = handle(request(ME, "POST", &path, "not json"), &deps(&f)).await;
    assert_eq!(out.status, 400);
    assert_eq!(f.table.stats().transactions, 0);
}

#[tokio::test]
async fn another_callers_run_and_messages_are_404() {
    let f = Fake::new();
    post_message(&f, 1, "mine").await;
    let id = mid(1);
    let msgs = format!("/v1/runs/{RUN}/messages");
    for (method, path, body) in [
        (
            "POST",
            msgs.clone(),
            json!({"messageId": mid(2), "text": "x"}),
        ),
        ("GET", msgs.clone(), Value::Null),
        ("GET", format!("{msgs}/{id}"), Value::Null),
        ("POST", format!("{msgs}/{id}/withdraw"), Value::Null),
    ] {
        let (status, v) = call(&f, OTHER, method, &path, &body).await;
        assert_eq!(status, 404, "{method} {path}: {v}");
    }
    // nothing of the owner's changed
    assert_eq!(
        f.ledger()
            .get_message(RUN, &id)
            .await
            .unwrap()
            .unwrap()
            .state,
        MessageState::Pending
    );
    // a run that does not exist
    let (status, _) = call(
        &f,
        ME,
        "GET",
        "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X9Z/messages",
        &Value::Null,
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_finished_run_or_one_without_a_live_session_refuses_a_new_message() {
    let f = Fake::new();
    f.set_status("cleared");
    let (status, v) = post_message(&f, 1, "x").await;
    assert_eq!((status, code(&v)), (409, "run-finished"));
    f.set_status("active");
    f.set_agents(vec![agent("stopped", &["message.next"])]);
    let (status, v) = post_message(&f, 1, "x").await;
    assert_eq!((status, code(&v)), (409, "no-live-session"));
    f.set_agents(vec![]);
    let (status, v) = post_message(&f, 1, "x").await;
    assert_eq!((status, code(&v)), (409, "no-live-session"));
    assert_eq!(f.table.stats().committed, 0, "nothing was stored");
}

#[tokio::test]
async fn a_run_whose_agent_registered_no_message_control_refuses_and_stores_nothing() {
    let f = Fake::new();
    f.set_agents(vec![agent("running", &[])]);
    let (status, v) = post_message(&f, 1, "x").await;
    assert_eq!((status, code(&v)), (409, "no-message-control"));
    assert!(v["message"].as_str().unwrap().contains("/cancel"));
    // next registered, now not: a next message passes and a now message does not
    f.set_agents(vec![agent("running", &["message.next"])]);
    assert_eq!(post_message(&f, 1, "x").await.0, 202);
    let (status, v) = call(
        &f,
        ME,
        "POST",
        &format!("/v1/runs/{RUN}/messages"),
        &json!({"messageId": mid(2), "text": "now", "mode": "now"}),
    )
    .await;
    assert_eq!((status, code(&v)), (409, "no-message-control"));
    assert_eq!(f.ledger().list(RUN).await.unwrap().messages.len(), 1);
}

#[tokio::test]
async fn an_open_question_takes_the_text_as_its_answer_and_a_message_is_refused_with_its_request_id()
 {
    let f = Fake::new();
    f.events.lock().unwrap().push(question(true));
    let (status, v) = post_message(&f, 1, "x").await;
    assert_eq!((status, code(&v)), (409, "question-open"));
    assert!(v["message"].as_str().unwrap().contains(REQ));
    assert_eq!(f.table.stats().committed, 0);
    // a notice that is not a blocking question does not stop a message
    f.events.lock().unwrap().clear();
    f.events.lock().unwrap().push(question(false));
    assert_eq!(post_message(&f, 1, "x").await.0, 202);
}

#[tokio::test]
async fn a_failed_question_read_does_not_stop_the_message_and_is_logged() {
    let mut f = Fake::new();
    f.tail_down = true;
    let (out, logs) = handle_logged(
        request(
            ME,
            "POST",
            &format!("/v1/runs/{RUN}/messages"),
            &json!({"messageId": mid(1), "text": "x"}).to_string(),
        ),
        &deps(&f),
    )
    .await;
    assert_eq!(out.status, 202);
    assert!(
        logs.iter().any(|l| l.contains("question check")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn the_limits_have_their_own_status_and_code() {
    let f = Fake::new();
    // the run's rate limit of ten a minute: each message is withdrawn after it is sent, so the count of waiting messages stays out of the way
    for n in 1..=10 {
        assert_eq!(post_message(&f, n, "m").await.0, 202, "message {n}");
        assert_eq!(withdraw_from(&f, RUN, n).await, 200, "withdraw {n}");
    }
    let (status, v) = post_message(&f, 11, "m").await;
    assert_eq!((status, code(&v)), (429, "rate-limited"));
    // the next minute admits again
    f.clock.advance(60);
    assert_eq!(post_message(&f, 11, "m").await.0, 202);
}

/// follow-up: twenty messages may wait for a running Cline. The clock moves so that the run's rate limit of ten a minute stays out of
/// the way while the twenty are sent.
#[tokio::test]
async fn b776w_the_twenty_first_waiting_message_is_409_too_many_pending_with_the_number_in_its_words()
 {
    let f = Fake::new();
    for n in 1..=20u32 {
        f.clock.set(T0 + i64::from(n / 5) * 60);
        assert_eq!(post_message(&f, n, "m").await.0, 202, "message {n}");
    }
    let (status, v) = post_message(&f, 21, "m").await;
    assert_eq!((status, code(&v)), (409, "too-many-pending"), "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("20 messages are waiting for Cline already"),
        "{v}"
    );
    // a refused message stores nothing, and a withdrawn one frees its slot
    assert_eq!(f.ledger().list(RUN).await.unwrap().watermark, 20);
    assert_eq!(withdraw_from(&f, RUN, 3).await, 200);
    assert_eq!(post_message(&f, 22, "m").await.0, 202);
}

/// The run limits and the owner limit through the real route, not only in the ledger (`message-ledger/tests/admit.rs` pins the
/// numbers). Each message is withdrawn after it is sent, which frees its slot of three; the clock moves so that the rate limit of ten a minute for the run
/// stays out of the way until a test asks for it.
#[tokio::test]
async fn the_101st_message_of_a_run_is_refused_with_409_run_limit() {
    let f = Fake::new();
    for n in 1..=100u32 {
        f.clock.set(T0 + i64::from(n / 5) * 60);
        assert_eq!(post_message(&f, n, "m").await.0, 202, "message {n}");
        assert_eq!(withdraw_from(&f, RUN, n).await, 200, "withdraw {n}");
    }
    f.clock.advance(60);
    let (status, v) = post_message(&f, 101, "the 101st").await;
    assert_eq!((status, code(&v)), (409, "run-limit"), "{v}");
    assert!(
        v["message"].as_str().unwrap_or_default().contains("100"),
        "the refusal does not name the limit: {v}"
    );
    // Nothing was stored for it: the run's count of admitted messages stays 100.
    assert_eq!(f.ledger().list(RUN).await.unwrap().watermark, 100);
}

#[tokio::test]
async fn a_message_that_takes_a_run_past_4_mib_is_refused_with_409_run_limit() {
    let f = Fake::new();
    let chunk = "x".repeat(131_000);
    // 32 messages of 131,000 bytes make 4,192,000 bytes; the 33rd would make 4,323,000, over 4,194,304 (4 MiB = 4096 KiB).
    for n in 1..=32u32 {
        f.clock.set(T0 + i64::from(n / 5) * 60);
        assert_eq!(post_message(&f, n, &chunk).await.0, 202, "message {n}");
        assert_eq!(withdraw_from(&f, RUN, n).await, 200, "withdraw {n}");
    }
    f.clock.advance(120);
    let (status, v) = post_message(&f, 33, &chunk).await;
    assert_eq!((status, code(&v)), (409, "run-limit"), "{v}");
    assert!(
        v["message"].as_str().unwrap_or_default().contains("4096"),
        "the refusal does not name the limit: {v}"
    );
    // The cap is on bytes and not on count: a short message still fits.
    assert_eq!(post_message(&f, 34, &"y".repeat(2_000)).await.0, 202);
}

#[tokio::test]
async fn the_31st_admission_of_an_owner_in_a_minute_is_refused_with_429_rate_limited() {
    let f = Fake::new();
    // Three runs of one owner take ten messages each in the same minute (the run limit is ten a minute); a fourth run takes the 31st.
    let runs = [
        RUN,
        "01M4ZHFBV5SDX7J92E6YRH4QT1",
        "01M4ZHFBV5SDX7J92E6YRH4QT2",
        "01M4ZHFBV5SDX7J92E6YRH4QT3",
    ];
    for run in &runs[1..] {
        f.runs
            .lock()
            .unwrap()
            .insert((*run).to_string(), run_item_of(run, "active"));
    }
    for (i, run) in runs[..3].iter().enumerate() {
        for k in 1..=10u32 {
            let n = i as u32 * 10 + k;
            assert_eq!(post_to(&f, run, n, "m").await.0, 202, "message {n}");
            assert_eq!(withdraw_from(&f, run, n).await, 200, "withdraw {n}");
        }
    }
    let (status, v) = post_to(&f, runs[3], 31, "the 31st").await;
    assert_eq!((status, code(&v)), (429, "rate-limited"), "{v}");
    // It is the owner's limit and not the run's: the fourth run holds no message yet.
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("from you"),
        "the refusal is not the owner's: {v}"
    );
    assert_eq!(f.ledger().list(runs[3]).await.unwrap().watermark, 0);
    // The window moves with the clock: the next minute admits it.
    f.clock.advance(60);
    assert_eq!(post_to(&f, runs[3], 31, "the 31st").await.0, 202);
}

#[tokio::test]
async fn a_store_without_a_ledger_or_with_a_failing_table_is_503() {
    let mut f = Fake::new();
    f.no_ledger = true;
    let (out, logs) = handle_logged(
        request(
            ME,
            "POST",
            &format!("/v1/runs/{RUN}/messages"),
            &json!({"messageId": mid(1), "text": "x"}).to_string(),
        ),
        &deps(&f),
    )
    .await;
    assert_eq!(out.status, 503);
    assert!(out.body.contains("retry"), "{}", out.body);
    assert!(
        !out.body.contains("keeps no messages"),
        "the store's reason stays in the log"
    );
    assert!(
        logs.iter().any(|l| l.contains("keeps no messages")),
        "{logs:?}"
    );
    let mut f = Fake::new();
    f.table_down = true;
    let (status, _) = post_message(&f, 1, "x").await;
    assert_eq!(status, 503);
}

// ---- read and withdraw -----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_list_and_the_single_read_show_the_state_the_server_holds() {
    let f = Fake::new();
    post_message(&f, 1, "one").await;
    post_message(&f, 2, "two").await;
    let ledger = f.ledger();
    ledger.report_session(RUN, GEN, "sess-1", 1).await.unwrap();
    ledger
        .claim(RUN, GEN, "sess-1", Some(&mid(1)), 1, false)
        .await
        .unwrap();
    ledger
        .outcome(
            RUN,
            GEN,
            "sess-1",
            &mid(1),
            1,
            &MessageOutcome::Submitted { turn: 2 },
        )
        .await
        .unwrap();
    let (status, v) = call(
        &f,
        ME,
        "GET",
        &format!("/v1/runs/{RUN}/messages"),
        &Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    let page: MessagesPage = serde_json::from_value(v).unwrap();
    assert_eq!(page.run_id, RUN);
    assert_eq!(
        page.messages.iter().map(|m| m.state).collect::<Vec<_>>(),
        [MessageState::Delivered, MessageState::Pending]
    );
    assert_eq!(
        (page.epoch.as_deref(), page.epoch_no, page.watermark),
        (Some("sess-1"), 1, 2)
    );
    let (status, v) = call(
        &f,
        ME,
        "GET",
        &format!("/v1/runs/{RUN}/messages/{}", mid(1)),
        &Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(v["message"]["deliveredTurn"], 2);
    let (status, _) = call(
        &f,
        ME,
        "GET",
        &format!("/v1/runs/{RUN}/messages/{}", mid(9)),
        &Value::Null,
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = call(
        &f,
        ME,
        "GET",
        &format!("/v1/runs/{RUN}/messages/not-an-id"),
        &Value::Null,
    )
    .await;
    assert_eq!(status, 404);
    // a run that never had a message reads as an empty open page
    let f2 = Fake::new();
    let (status, v) = call(
        &f2,
        ME,
        "GET",
        &format!("/v1/runs/{RUN}/messages"),
        &Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        (
            v["messages"].as_array().unwrap().len(),
            v["admission"].as_str(),
            v["watermark"].as_u64()
        ),
        (0, Some("open"), Some(0))
    );
}

#[tokio::test]
async fn a_withdrawal_answers_200_twice_and_is_refused_once_agentd_has_the_message() {
    let f = Fake::new();
    post_message(&f, 1, "withdraw me").await;
    post_message(&f, 2, "agentd takes me").await;
    let path = |n: u32| format!("/v1/runs/{RUN}/messages/{}/withdraw", mid(n));
    for _ in 0..2 {
        let (status, v) = call(&f, ME, "POST", &path(1), &Value::Null).await;
        assert_eq!(
            (status, v["message"]["state"].as_str()),
            (200, Some("withdrawn"))
        );
    }
    let ledger = f.ledger();
    ledger.report_session(RUN, GEN, "sess-1", 1).await.unwrap();
    ledger
        .claim(RUN, GEN, "sess-1", Some(&mid(2)), 1, false)
        .await
        .unwrap();
    let (status, v) = call(&f, ME, "POST", &path(2), &Value::Null).await;
    assert_eq!((status, code(&v)), (409, "already-dispatching"));
    let (status, v) = call(&f, ME, "POST", &path(9), &Value::Null).await;
    assert_eq!((status, code(&v)), (404, "not-found"));
}

#[tokio::test]
async fn a_held_message_is_confirmed_by_a_new_message_that_names_it_and_the_session_shown() {
    let f = Fake::new();
    let ledger = f.ledger();
    ledger.report_session(RUN, GEN, "sess-1", 1).await.unwrap();
    post_message(&f, 1, "carry on with the tests").await;
    ledger.report_session(RUN, GEN, "sess-2", 2).await.unwrap();
    let path = format!("/v1/runs/{RUN}/messages");
    let confirm = |n: u32, epoch: &str| {
        json!({"messageId": mid(n), "text": "carry on with the tests",
               "confirms": {"messageId": mid(1), "epoch": epoch}})
    };
    let (status, v) = call(&f, ME, "POST", &path, &confirm(2, "sess-1")).await;
    assert_eq!((status, code(&v)), (409, "epoch-changed"));
    let (status, v) = call(&f, ME, "POST", &path, &confirm(2, "sess-2")).await;
    assert_eq!(status, 202, "{v}");
    assert_eq!(v["message"]["confirms"], mid(1));
    let (_, v) = call(&f, ME, "GET", &format!("{path}/{}", mid(1)), &Value::Null).await;
    assert_eq!(
        (
            v["message"]["state"].as_str(),
            v["message"]["supersededBy"].as_str().map(str::to_string)
        ),
        (Some("superseded"), Some(mid(2)))
    );
}

// ---- cancel ----------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_cancel_closes_the_run_to_messages_and_rejects_the_open_ones_as_cancelled() {
    let f = Fake::new();
    post_message(&f, 1, "pending when the user stops the run").await;
    let (status, _) = call(
        &f,
        ME,
        "POST",
        &format!("/v1/runs/{RUN}/cancel"),
        &Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    let m = f.ledger().get_message(RUN, &mid(1)).await.unwrap().unwrap();
    assert_eq!(
        (m.state, m.reason),
        (MessageState::Rejected, Some(RejectReason::RunCancelled))
    );
    let (status, v) = post_message(&f, 2, "too late").await;
    assert_eq!((status, code(&v)), (409, "run-sealed"));
    assert!(v["message"].as_str().unwrap().contains("stopped"));
}

#[tokio::test]
async fn the_run_is_closed_to_messages_before_the_cancel_control_is_queued() {
    // review of 2026-10-16: with the Cancel queued first, agentd could claim a message between the two writes and send it to Cline
    for query in [None, Some(("discard", "true"))] {
        let f = Fake::new();
        post_message(&f, 1, "pending when the user stops the run").await;
        let mut req = request(ME, "POST", &format!("/v1/runs/{RUN}/cancel"), "");
        if let Some((k, v)) = query {
            req.query.insert(k.into(), v.into());
        }
        let out = handle(req, &deps(&f)).await;
        assert_eq!(out.status, 200, "{query:?}: {}", out.body);
        assert_eq!(
            *f.closed_at_cancel.lock().unwrap(),
            [true],
            "{query:?}: the Cancel control was queued while the run still took messages"
        );
    }
}

#[tokio::test]
async fn a_cancel_is_queued_even_when_the_messages_cannot_be_closed() {
    let mut f = Fake::new();
    f.table_down = true;
    let (out, logs) = handle_logged(
        request(ME, "POST", &format!("/v1/runs/{RUN}/cancel"), ""),
        &deps(&f),
    )
    .await;
    assert_eq!(out.status, 200, "{}", out.body);
    assert_eq!(
        f.controls.lock().unwrap().len(),
        1,
        "the Cancel is the user's safety action"
    );
    assert!(
        logs.iter().any(|l| l.contains("not closed by the cancel")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn a_cancel_on_a_store_without_a_ledger_still_works() {
    let mut f = Fake::new();
    f.no_ledger = true;
    let (out, logs) = handle_logged(
        request(ME, "POST", &format!("/v1/runs/{RUN}/cancel"), ""),
        &deps(&f),
    )
    .await;
    assert_eq!(out.status, 200);
    assert!(logs.is_empty(), "{logs:?}");
}

#[tokio::test]
async fn the_routes_allow_only_their_methods() {
    let f = Fake::new();
    let msgs = format!("/v1/runs/{RUN}/messages");
    for (method, path) in [
        ("PUT", msgs.clone()),
        ("DELETE", msgs.clone()),
        ("POST", format!("{msgs}/{}", mid(1))),
        ("GET", format!("{msgs}/{}/withdraw", mid(1))),
    ] {
        let (status, v) = call(&f, ME, method, &path, &Value::Null).await;
        assert_eq!(
            (status, code(&v)),
            (405, "method-not-allowed"),
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn b788_the_refusals_of_a_message_name_the_engine_of_the_run() {
    let f = Fake::new();
    for (engine, name) in [(None, "Cline"), (Some(Engine::OPENCODE), "OpenCode")] {
        let mut run = run_item("active");
        run.engine = engine;
        f.runs.lock().unwrap().insert(RUN.into(), run);
        f.set_agents(vec![]);
        let (status, v) = post_message(&f, 1, "x").await;
        assert_eq!((status, code(&v)), (409, "no-live-session"));
        assert_eq!(
            v["message"],
            format!("the run has no live {name} session to take a message")
        );
        // An agent that registered no message control.
        f.set_agents(vec![agent("running", &[])]);
        let (status, v) = post_message(&f, 2, "x").await;
        assert_eq!(status, 409);
        let text = v["message"].as_str().unwrap();
        assert!(
            text.starts_with(&format!("this run's {name} cannot take messages")),
            "{text}"
        );
    }
}
