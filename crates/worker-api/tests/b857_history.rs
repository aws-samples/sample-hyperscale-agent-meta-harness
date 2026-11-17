//! `POST /internal/message-history` on the private API, through the real handler over the real ledger on the in-memory table.
//!
//! At the start of a session the worker asks for the messages of its run that were delivered to an engine in the sessions before it.
//! The route answers from the service's message ledger: the texts as they were stored when the user sent them. The run and the agent are
//! the caller's own IAM session; the route reads and changes nothing; a read that fails is an error and never an empty answer.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::internal::{PendingControl, ProcessCredentials};
use harness_protocol::common::Usage;
use harness_protocol::envelope::Envelope;
use harness_protocol::goal::GoalStatus;
use harness_protocol::messages::{
    ClaimRequest, ClaimResponse, HistoryResponse, MessageMode, MessageNotice, MessageState,
    OutcomeRequest, OutcomeResponse, SealRequest, SealResponse, SendMessageRequest, SessionAck,
    SessionReport,
};
use harness_protocol::store::{AgentItem, TurnItem};
use message_ledger::mem::MemTable;
use message_ledger::{Ledger, LedgerError, ManualClock};
use serde_json::{Value, json};
use std::sync::Mutex;
use worker_api::creds::MintPlan;
use worker_api::handler::{
    CallbackError, Callbacks, Deps, InternalRequest, MintError, Minter, Store, StoreError,
    TokenContext, handle,
};

const ROLE: &str = "hamh-direct-harness-worker-session";
const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const OTHER_RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTX";
const OWNER: &str = "arn:aws:sts::111122223333:assumed-role/Admin/owner";
const GEN: &str = "01M5G000000000000000000001";
/// 2026-10-16T14:02:11Z
const T0: i64 = 1_792_159_331;

fn mid(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

struct Fake {
    table: MemTable,
    clock: ManualClock,
    /// The read of the ledger fails.
    down: bool,
    /// The runs the history was read for, in order.
    asked: Mutex<Vec<String>>,
}

impl Fake {
    fn new() -> Fake {
        Fake {
            table: MemTable::new(),
            clock: ManualClock::at(T0),
            down: false,
            asked: Mutex::default(),
        }
    }
    fn ledger(&self) -> Ledger<MemTable, ManualClock> {
        Ledger::with_clock(self.table.clone(), self.clock.clone())
    }
}

impl Store for Fake {
    async fn get_turn(&self, _: &str, _: &str, _: u32) -> Result<Option<TurnItem>, StoreError> {
        Ok(None)
    }
    async fn mark_turn_ended(&self, _: &str, _: &str, _: u32) -> Result<(), StoreError> {
        Ok(())
    }
    async fn get_agent(&self, _: &str, _: &str) -> Result<Option<AgentItem>, StoreError> {
        Ok(None)
    }
    async fn get_goal_status(&self, _: &str) -> Result<Option<GoalStatus>, StoreError> {
        Ok(Some(GoalStatus::Active))
    }
    async fn rotate_nonce(&self, _: &str, _: &str, _: &str, _: &str) -> Result<(), StoreError> {
        Ok(())
    }
    async fn put_events(&self, _: &str, _: &str, _: &[Envelope]) -> Result<(), StoreError> {
        Ok(())
    }
    async fn record_heartbeat(&self, _: &str, _: &str, _: u32) -> Result<(), StoreError> {
        Ok(())
    }
    async fn record_turn_complete(&self, _: &str, _: &str, _: u32) -> Result<(), StoreError> {
        Ok(())
    }
    async fn record_turn_usage(
        &self,
        _: &str,
        _: &str,
        _: u32,
        _: &Usage,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    async fn record_turn_transcript(
        &self,
        _: &str,
        _: &str,
        _: u32,
        _: &harness_protocol::transcript::TranscriptSummary,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    async fn take_controls(&self, _: &str, _: &str) -> Result<Vec<PendingControl>, StoreError> {
        Ok(Vec::new())
    }
    async fn message_notices(&self, run: &str) -> Result<Vec<MessageNotice>, LedgerError> {
        self.ledger().notices(run).await
    }
    async fn message_session(
        &self,
        run: &str,
        r: &SessionReport,
    ) -> Result<SessionAck, LedgerError> {
        self.ledger()
            .report_session(run, &r.generation, &r.epoch, r.number)
            .await
    }
    async fn message_claim(
        &self,
        r: &ClaimRequest,
        cancel_queued: bool,
    ) -> Result<ClaimResponse, LedgerError> {
        self.ledger()
            .claim(
                &r.run_id,
                &r.generation,
                &r.epoch,
                r.message_id.as_deref(),
                r.limit.unwrap_or(1),
                cancel_queued,
            )
            .await
    }
    async fn message_seal(&self, r: &SealRequest) -> Result<SealResponse, LedgerError> {
        self.ledger()
            .seal(&r.run_id, &r.generation, &r.epoch, r.mode)
            .await
    }
    async fn message_outcome(&self, r: &OutcomeRequest) -> Result<OutcomeResponse, LedgerError> {
        self.ledger()
            .outcome(
                &r.run_id,
                &r.generation,
                &r.epoch,
                &r.message_id,
                r.attempt,
                &r.outcome,
            )
            .await
    }
    async fn message_history(&self, run: &str) -> Result<HistoryResponse, LedgerError> {
        self.asked.lock().unwrap().push(run.to_string());
        if self.down {
            return Err(LedgerError::Unavailable("the table is throttled".into()));
        }
        self.ledger().delivered(run).await
    }
}

/// A store as it was before the route: it has the four message calls of the trait's defaults and no history.
struct NoLedger;
impl Store for NoLedger {
    async fn get_turn(&self, _: &str, _: &str, _: u32) -> Result<Option<TurnItem>, StoreError> {
        Ok(None)
    }
    async fn mark_turn_ended(&self, _: &str, _: &str, _: u32) -> Result<(), StoreError> {
        Ok(())
    }
    async fn get_agent(&self, _: &str, _: &str) -> Result<Option<AgentItem>, StoreError> {
        Ok(None)
    }
    async fn get_goal_status(&self, _: &str) -> Result<Option<GoalStatus>, StoreError> {
        Ok(Some(GoalStatus::Active))
    }
    async fn rotate_nonce(&self, _: &str, _: &str, _: &str, _: &str) -> Result<(), StoreError> {
        Ok(())
    }
    async fn put_events(&self, _: &str, _: &str, _: &[Envelope]) -> Result<(), StoreError> {
        Ok(())
    }
    async fn record_heartbeat(&self, _: &str, _: &str, _: u32) -> Result<(), StoreError> {
        Ok(())
    }
    async fn record_turn_complete(&self, _: &str, _: &str, _: u32) -> Result<(), StoreError> {
        Ok(())
    }
    async fn record_turn_usage(
        &self,
        _: &str,
        _: &str,
        _: u32,
        _: &Usage,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    async fn record_turn_transcript(
        &self,
        _: &str,
        _: &str,
        _: u32,
        _: &harness_protocol::transcript::TranscriptSummary,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    async fn take_controls(&self, _: &str, _: &str) -> Result<Vec<PendingControl>, StoreError> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct NoMint;
impl Minter for NoMint {
    async fn mint(&self, _: &MintPlan) -> Result<ProcessCredentials, MintError> {
        Err(MintError("unused".into()))
    }
}
#[derive(Default)]
struct NoCallbacks;
impl Callbacks for NoCallbacks {
    async fn turn_done(&self, _: &str, _: &TokenContext, _: &str) -> Result<(), CallbackError> {
        Ok(())
    }
    async fn turn_alive(&self, _: &str, _: &TokenContext) -> Result<(), CallbackError> {
        Ok(())
    }
}

fn deps<S: Store>(store: S) -> Deps<S, NoMint, NoCallbacks> {
    Deps {
        store,
        minter: NoMint,
        callbacks: NoCallbacks,
        role_name: ROLE.into(),
        bucket: "b".into(),
    }
}

fn arn(run: &str, agent: &str) -> Option<String> {
    Some(format!(
        "arn:aws:sts::111122223333:assumed-role/{ROLE}/{run}.{agent}"
    ))
}

fn req(op: &str, caller: Option<String>, body: &Value) -> InternalRequest {
    InternalRequest {
        method: "POST".into(),
        path: format!("/direct/internal/{op}"),
        caller_arn: caller,
        completion_key: None,
        body: body.to_string(),
    }
}

async fn call<S: Store>(
    d: &Deps<S, NoMint, NoCallbacks>,
    run: &str,
    op: &str,
    body: Value,
) -> (u16, Value) {
    let out = handle(req(op, arn(run, "direct"), &body), d).await;
    (out.status, serde_json::from_str(&out.body).unwrap())
}

fn ask(run: &str) -> Value {
    json!({"runId": run, "agentId": "direct"})
}

async fn admit(d: &Deps<Fake, NoMint, NoCallbacks>, run: &str, n: u32, text: &str) {
    d.store
        .ledger()
        .admit(
            run,
            OWNER,
            &SendMessageRequest {
                message_id: mid(n),
                text: text.into(),
                mode: MessageMode::Next,
                confirms: None,
            },
        )
        .await
        .unwrap();
}

fn base(run: &str, epoch: &str) -> Value {
    json!({"runId": run, "agentId": "direct", "generation": GEN, "epoch": epoch})
}

fn with(mut v: Value, extra: Value) -> Value {
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

/// The worker's own calls for one delivery, through the routes: the claim of the next message and the report that it went out.
async fn deliver(d: &Deps<Fake, NoMint, NoCallbacks>, run: &str, turn: u32) -> String {
    let (status, v) = call(d, run, "message-claim", base(run, "s1")).await;
    assert_eq!(status, 200, "{v}");
    let id = v["messages"][0]["messageId"].as_str().unwrap().to_string();
    let attempt = v["messages"][0]["attempt"].clone();
    let (status, v) = call(
        d,
        run,
        "message-outcome",
        with(
            base(run, "s1"),
            json!({"messageId": id, "attempt": attempt, "outcome": {"kind": "submitted", "turn": turn}}),
        ),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    id
}

async fn session(d: &Deps<Fake, NoMint, NoCallbacks>, run: &str) {
    let (status, v) = call(
        d,
        run,
        "message-session",
        with(base(run, "s1"), json!({"number": 1})),
    )
    .await;
    assert_eq!(status, 200, "{v}");
}

fn texts(v: &Value) -> Vec<String> {
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap().to_string())
        .collect()
}

// ---- what the route answers --------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b857_the_route_answers_the_delivered_messages_of_the_callers_run_with_ids_and_stored_texts()
 {
    let d = deps(Fake::new());
    session(&d, RUN).await;
    admit(&d, RUN, 1, "use tabs, not spaces").await;
    admit(&d, RUN, 2, "name the file REPORT.md").await;
    admit(&d, RUN, 3, "still waiting").await;
    assert_eq!(deliver(&d, RUN, 2).await, mid(1));
    assert_eq!(deliver(&d, RUN, 3).await, mid(2));
    let (status, v) = call(&d, RUN, "message-history", ask(RUN)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v,
        json!({"messages": [
            {"messageId": mid(1), "seq": 1, "deliveredTurn": 2, "text": "use tabs, not spaces"},
            {"messageId": mid(2), "seq": 2, "deliveredTurn": 3, "text": "name the file REPORT.md"},
        ], "cut": 0}),
        "delivered only, in the ledger's order; the pending message is still to come as a prompt"
    );
    let read: HistoryResponse = serde_json::from_value(v).unwrap();
    assert_eq!(read.messages.len(), 2);
    assert_eq!(
        d.store
            .ledger()
            .get_message(RUN, &mid(3))
            .await
            .unwrap()
            .unwrap()
            .state,
        MessageState::Pending
    );
}

#[tokio::test]
async fn b857_a_run_without_messages_answers_an_empty_list() {
    let d = deps(Fake::new());
    let (status, v) = call(&d, RUN, "message-history", ask(RUN)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v, json!({"messages": [], "cut": 0}));
}

// ---- whose run -----------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b857_the_route_takes_post_from_the_direct_agents_own_session_only() {
    let d = deps(Fake::new());
    session(&d, RUN).await;
    admit(&d, RUN, 1, "words of the user").await;
    deliver(&d, RUN, 2).await;
    let body = ask(RUN);
    // not a worker session, and no caller at all
    for caller in [
        Some("arn:aws:sts::111122223333:assumed-role/Admin/owner".to_string()),
        None,
    ] {
        let out = handle(req("message-history", caller, &body), &d).await;
        assert_eq!(out.status, 403);
        assert!(!out.body.contains("words of the user"), "{}", out.body);
    }
    // GET
    let mut get = req("message-history", arn(RUN, "direct"), &body);
    get.method = "GET".into();
    assert_eq!(handle(get, &d).await.status, 405);
    // another agent of the run
    let out = handle(
        req(
            "message-history",
            arn(RUN, "worker-1"),
            &json!({"runId": RUN, "agentId": "worker-1"}),
        ),
        &d,
    )
    .await;
    assert_eq!(out.status, 403, "{}", out.body);
    assert!(out.body.contains("not_direct_agent"), "{}", out.body);
    // a body that is not the request: nothing is read
    let (status, v) = call(&d, RUN, "message-history", json!({"runId": RUN})).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("bad_request")));
    let (status, v) = call(
        &d,
        RUN,
        "message-history",
        json!({"runId": RUN, "agentId": "direct", "generation": GEN}),
    )
    .await;
    assert_eq!((status, v["code"].as_str()), (400, Some("bad_request")));
    assert!(
        d.store.asked.lock().unwrap().is_empty(),
        "none of these calls reached the ledger"
    );
}

#[tokio::test]
async fn b857_credentials_of_another_run_get_nothing_of_this_run() {
    let d = deps(Fake::new());
    session(&d, RUN).await;
    admit(&d, RUN, 1, "words for the first run").await;
    deliver(&d, RUN, 2).await;
    // The session of another run names this run in its body: refused, and the ledger is not read.
    let out = handle(
        req("message-history", arn(OTHER_RUN, "direct"), &ask(RUN)),
        &d,
    )
    .await;
    assert_eq!(out.status, 403, "{}", out.body);
    assert!(out.body.contains("caller_mismatch"), "{}", out.body);
    assert!(
        !out.body.contains("words for the first run"),
        "{}",
        out.body
    );
    assert!(d.store.asked.lock().unwrap().is_empty());
    // It asks for its own run: it gets its own run's messages, which are none.
    let (status, v) = call(&d, OTHER_RUN, "message-history", ask(OTHER_RUN)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v, json!({"messages": [], "cut": 0}));
    // The read is made for the run of the caller's session.
    assert_eq!(*d.store.asked.lock().unwrap(), [OTHER_RUN.to_string()]);
    // TRIGGER PRESENT: the first run's own session does read them.
    let (_, v) = call(&d, RUN, "message-history", ask(RUN)).await;
    assert_eq!(texts(&v), ["words for the first run"]);
}

// ---- the route reads and changes nothing ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn b857_the_route_writes_nothing_and_fences_nobody() {
    let d = deps(Fake::new());
    session(&d, RUN).await;
    admit(&d, RUN, 1, "one").await;
    admit(&d, RUN, 2, "two").await;
    deliver(&d, RUN, 2).await;
    // message 2 is claimed by the session that is running and has no outcome yet
    let (status, v) = call(&d, RUN, "message-claim", base(RUN, "s1")).await;
    assert_eq!(status, 200, "{v}");
    let attempt = v["messages"][0]["attempt"].clone();
    let before = d.store.table.all();
    let writes = d.store.table.stats().committed;
    for _ in 0..3 {
        let (status, v) = call(&d, RUN, "message-history", ask(RUN)).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(texts(&v), ["one"]);
    }
    assert_eq!(d.store.table.all(), before, "no item changed");
    assert_eq!(d.store.table.stats().committed, writes, "no write was made");
    // The incarnation that holds the claim is still the run's: its report is taken as before.
    let (status, v) = call(
        &d,
        RUN,
        "message-outcome",
        with(
            base(RUN, "s1"),
            json!({"messageId": mid(2), "attempt": attempt, "outcome": {"kind": "submitted", "turn": 3}}),
        ),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let (_, v) = call(&d, RUN, "message-history", ask(RUN)).await;
    assert_eq!(texts(&v), ["one", "two"]);
}

#[tokio::test]
async fn b857_the_route_needs_no_session_report_and_makes_no_message_control_item() {
    // A session in a new machine asks before its engine has a session id: the run of this test never reported one.
    let d = deps(Fake::new());
    let (status, v) = call(&d, RUN, "message-history", ask(RUN)).await;
    assert_eq!(status, 200, "{v}");
    assert!(d.store.table.all().is_empty(), "{:?}", d.store.table.all());
}

// ---- a worker's report supplies no text ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn b857_a_workers_report_names_a_message_by_id_and_cannot_supply_a_text() {
    let d = deps(Fake::new());
    session(&d, RUN).await;
    admit(&d, RUN, 1, "the text the user sent").await;
    admit(&d, RUN, 2, "never sent to the engine").await;
    let (_, v) = call(&d, RUN, "message-claim", base(RUN, "s1")).await;
    let attempt = v["messages"][0]["attempt"].clone();
    // A report with a text of its own is not a report: refused, and the message is not delivered by it.
    let (status, v) = call(
        &d,
        RUN,
        "message-outcome",
        with(
            base(RUN, "s1"),
            json!({"messageId": mid(1), "attempt": attempt, "text": "words the worker chose",
                   "outcome": {"kind": "submitted", "turn": 2}}),
        ),
    )
    .await;
    assert_eq!(
        (status, v["code"].as_str()),
        (400, Some("bad_request")),
        "{v}"
    );
    let (_, v) = call(&d, RUN, "message-history", ask(RUN)).await;
    assert_eq!(
        v,
        json!({"messages": [], "cut": 0, "unconfirmed": [
            {"messageId": mid(1), "seq": 1, "text": "the text the user sent"},
        ]}),
        "a refused report delivers nothing: the claim has no report, and its text is the stored one"
    );
    // The report as it is defined: the message is delivered, and its text is the stored one.
    let (status, _) = call(
        &d,
        RUN,
        "message-outcome",
        with(
            base(RUN, "s1"),
            json!({"messageId": mid(1), "attempt": attempt, "outcome": {"kind": "submitted", "turn": 2}}),
        ),
    )
    .await;
    assert_eq!(status, 200);
    // A failed message's detail is the worker's text: it is never a message of the user.
    let (_, v) = call(&d, RUN, "message-claim", base(RUN, "s1")).await;
    let attempt = v["messages"][0]["attempt"].clone();
    let (status, _) = call(
        &d,
        RUN,
        "message-outcome",
        with(
            base(RUN, "s1"),
            json!({"messageId": mid(2), "attempt": attempt,
                   "outcome": {"kind": "failed", "detail": "not-sent: words the worker chose"}}),
        ),
    )
    .await;
    assert_eq!(status, 200);
    let out = handle(req("message-history", arn(RUN, "direct"), &ask(RUN)), &d).await;
    assert_eq!(out.status, 200);
    assert!(!out.body.contains("words the worker chose"), "{}", out.body);
    let v: Value = serde_json::from_str(&out.body).unwrap();
    assert_eq!(texts(&v), ["the text the user sent"]);
}

// ---- a read that fails is an error, never an empty answer ----------------------------------------------------------------------------

#[tokio::test]
async fn b857_a_ledger_that_cannot_be_read_answers_503_and_never_an_empty_list() {
    let mut f = Fake::new();
    f.down = true;
    let d = deps(f);
    let (status, v) = call(&d, RUN, "message-history", ask(RUN)).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (503, Some("store_unavailable")),
        "{v}"
    );
    assert!(v.get("messages").is_none());
    // a store that keeps no messages says so the same way
    let d = deps(NoLedger);
    let (status, v) = call(&d, RUN, "message-history", ask(RUN)).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (503, Some("store_unavailable")),
        "{v}"
    );
}

// ---- a worker of the release before ------------------------------------------------------------------------------------------------------

/// A worker image of the release before never asks the new route. Every call it does make is answered with exactly the members it was
/// answered with before, so it reads this service as it read its own.
#[tokio::test]
async fn b857_a_worker_that_never_asks_the_route_is_answered_as_before() {
    let d = deps(Fake::new());
    let (status, v) = call(
        &d,
        RUN,
        "message-session",
        with(base(RUN, "s1"), json!({"number": 1})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        v,
        json!({"epochNo": 1, "held": 0, "deliveryUnknown": 0, "admission": "open"})
    );
    admit(&d, RUN, 1, "use tabs").await;
    let (status, v) = call(
        &d,
        RUN,
        "events",
        json!({"runId": RUN, "agentId": "direct", "events": []}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["accepted", "pending"]);
    assert_eq!(v["pending"][0]["kind"], "message", "{v}");
    let (status, v) = call(&d, RUN, "message-claim", base(RUN, "s1")).await;
    assert_eq!(status, 200);
    assert_eq!(v["outcome"], "claimed");
    let mut keys: Vec<&String> = v["messages"][0].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "acceptedAt",
            "attempt",
            "epochNo",
            "messageId",
            "mode",
            "seq",
            "text"
        ]
    );
    let (status, v) = call(
        &d,
        RUN,
        "message-outcome",
        with(
            base(RUN, "s1"),
            json!({"messageId": mid(1), "attempt": 1, "outcome": {"kind": "submitted", "turn": 2}}),
        ),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(v, json!({"messageId": mid(1), "state": "delivered"}));
    let (status, v) = call(&d, RUN, "message-seal", base(RUN, "s1")).await;
    assert_eq!(status, 200);
    assert_eq!(
        v,
        json!({"outcome": "sealed", "watermark": 1, "rejected": 0, "deliveryUnknown": 0})
    );
    assert!(
        d.store.asked.lock().unwrap().is_empty(),
        "none of these calls reads the history"
    );
}

// ---- messages handed to an earlier session with no report of their delivery ------------------------------------------------------

/// A worker that took a message and was lost before it reported the delivery leaves a claim without a report. The route names that
/// message apart from the delivered ones, with the id and the text the ledger stored, to the run's own session and to no other run.
#[tokio::test]
async fn b857_the_route_also_answers_the_messages_handed_over_without_a_delivery_report() {
    let d = deps(Fake::new());
    session(&d, RUN).await;
    admit(&d, RUN, 1, "use tabs, not spaces").await;
    admit(&d, RUN, 2, "name the file REPORT.md").await;
    assert_eq!(deliver(&d, RUN, 2).await, mid(1));
    // the worker takes the second message through the route and is lost before it reports
    let (status, v) = call(&d, RUN, "message-claim", base(RUN, "s1")).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["messages"][0]["messageId"],
        mid(2),
        "TRIGGER PRESENT: {v}"
    );
    let (status, v) = call(&d, RUN, "message-history", ask(RUN)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v,
        json!({"messages": [
            {"messageId": mid(1), "seq": 1, "deliveredTurn": 2, "text": "use tabs, not spaces"},
        ], "cut": 0, "unconfirmed": [
            {"messageId": mid(2), "seq": 2, "text": "name the file REPORT.md"},
        ]}),
        "the claim without a report is named apart from the delivered message"
    );
    // The read settled nothing: the message is as the ledger held it.
    assert_eq!(
        d.store
            .ledger()
            .get_message(RUN, &mid(2))
            .await
            .unwrap()
            .unwrap()
            .state,
        MessageState::Dispatching
    );
    // The session of another run gets nothing of it.
    let (status, v) = call(&d, OTHER_RUN, "message-history", ask(OTHER_RUN)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v, json!({"messages": [], "cut": 0}));
}
