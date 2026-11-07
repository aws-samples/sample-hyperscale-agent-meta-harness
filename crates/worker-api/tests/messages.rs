//! Message control on the private API: the four routes agentd calls (`message-session`, `message-claim`,
//! `message-seal`, `message-outcome`) and the notices of pending messages in the events response, through the real handler over the real
//! ledger on the in-memory table. A message is claimed once, a stale incarnation is fenced out, a queued Cancel wins, and a message the
//! user withdrew is never handed out again.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::internal::{EventsBatchResponse, PendingControl, ProcessCredentials};
use harness_protocol::common::Usage;
use harness_protocol::envelope::Envelope;
use harness_protocol::goal::GoalStatus;
use harness_protocol::messages::{MessageMode, MessageNotice, MessageState, SendMessageRequest};
use harness_protocol::store::{AgentItem, TurnItem};
use message_ledger::mem::MemTable;
use message_ledger::{Ledger, LedgerError, ManualClock};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use worker_api::creds::MintPlan;
use worker_api::handler::{
    CallbackError, Callbacks, Deps, InternalRequest, MintError, Minter, Store, StoreError,
    TokenContext, handle, handle_logged,
};

const ROLE: &str = "hamh-direct-harness-worker-session";
const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const OWNER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
const GEN: &str = "01M5G000000000000000000001";
const GEN2: &str = "01M5G000000000000000000002";
/// 2026-10-16T14:02:11Z
const T0: i64 = 1_792_159_331;

fn mid(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

struct Fake {
    table: MemTable,
    clock: ManualClock,
    cancel: AtomicBool,
    no_ledger: bool,
    notices_down: bool,
    cancel_down: bool,
    seen: Mutex<Vec<String>>,
}

impl Fake {
    fn new() -> Fake {
        Fake {
            table: MemTable::new(),
            clock: ManualClock::at(T0),
            cancel: AtomicBool::new(false),
            no_ledger: false,
            notices_down: false,
            cancel_down: false,
            seen: Mutex::default(),
        }
    }
    fn ledger(&self) -> Ledger<MemTable, ManualClock> {
        Ledger::with_clock(self.table.clone(), self.clock.clone())
    }
    fn gate(&self) -> Result<(), LedgerError> {
        if self.no_ledger {
            return Err(LedgerError::Unavailable(
                "this store keeps no messages".into(),
            ));
        }
        Ok(())
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
        self.seen.lock().unwrap().push(format!("notices {run}"));
        if self.notices_down {
            return Err(LedgerError::Unavailable("notices throttled".into()));
        }
        self.gate()?;
        self.ledger().notices(run).await
    }
    async fn cancel_queued(&self, _: &str, _: &str) -> Result<bool, StoreError> {
        if self.cancel_down {
            return Err(StoreError::Unavailable("cancel read throttled".into()));
        }
        Ok(self.cancel.load(Ordering::SeqCst))
    }
    async fn message_session(
        &self,
        run: &str,
        r: &harness_protocol::messages::SessionReport,
    ) -> Result<harness_protocol::messages::SessionAck, LedgerError> {
        self.gate()?;
        self.ledger()
            .report_session(run, &r.generation, &r.epoch, r.number)
            .await
    }
    async fn message_claim(
        &self,
        r: &harness_protocol::messages::ClaimRequest,
        cancel_queued: bool,
    ) -> Result<harness_protocol::messages::ClaimResponse, LedgerError> {
        self.gate()?;
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
    async fn message_seal(
        &self,
        r: &harness_protocol::messages::SealRequest,
    ) -> Result<harness_protocol::messages::SealResponse, LedgerError> {
        self.gate()?;
        self.ledger()
            .seal(&r.run_id, &r.generation, &r.epoch, r.mode)
            .await
    }
    async fn message_outcome(
        &self,
        r: &harness_protocol::messages::OutcomeRequest,
    ) -> Result<harness_protocol::messages::OutcomeResponse, LedgerError> {
        self.gate()?;
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

fn deps(f: Fake) -> Deps<Fake, NoMint, NoCallbacks> {
    Deps {
        store: f,
        minter: NoMint,
        callbacks: NoCallbacks,
        role_name: ROLE.into(),
        bucket: "b".into(),
    }
}

fn arn(agent: &str) -> Option<String> {
    Some(format!(
        "arn:aws:sts::700000000011:assumed-role/{ROLE}/{RUN}.{agent}"
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

async fn call(d: &Deps<Fake, NoMint, NoCallbacks>, op: &str, body: Value) -> (u16, Value) {
    let out = handle(req(op, arn("direct"), &body), d).await;
    (out.status, serde_json::from_str(&out.body).unwrap())
}

fn base(generation: &str, epoch: &str) -> Value {
    json!({"runId": RUN, "agentId": "direct", "generation": generation, "epoch": epoch})
}

/// A session report: the number is the ordinal of the session in its incarnation, here the digits of its name ("s2" is the second).
fn sess(generation: &str, epoch: &str) -> Value {
    let number: u32 = epoch
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(1);
    with(base(generation, epoch), json!({ "number": number }))
}

fn with(mut v: Value, extra: Value) -> Value {
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

async fn admit(d: &Deps<Fake, NoMint, NoCallbacks>, n: u32, text: &str, mode: MessageMode) {
    d.store
        .ledger()
        .admit(
            RUN,
            OWNER,
            &SendMessageRequest {
                message_id: mid(n),
                text: text.into(),
                mode,
                confirms: None,
            },
        )
        .await
        .unwrap();
}

async fn events(d: &Deps<Fake, NoMint, NoCallbacks>, take: Option<bool>) -> EventsBatchResponse {
    let mut body = json!({"runId": RUN, "agentId": "direct", "events": []});
    if let Some(t) = take {
        body["takeControls"] = json!(t);
    }
    let out = handle(req("events", arn("direct"), &body), d).await;
    assert_eq!(out.status, 200, "{}", out.body);
    serde_json::from_str(&out.body).unwrap()
}

fn notices(r: &EventsBatchResponse) -> Vec<&MessageNotice> {
    r.pending
        .iter()
        .filter_map(|c| match c {
            PendingControl::Message(n) => Some(n),
            _ => None,
        })
        .collect()
}

// ---- the routes ------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_four_routes_take_post_from_the_agents_own_session_only() {
    let d = deps(Fake::new());
    for op in [
        "message-session",
        "message-claim",
        "message-seal",
        "message-outcome",
    ] {
        let mut body = base(GEN, "s1");
        if op == "message-session" {
            body = with(body, json!({"number": 1}));
        }
        if op == "message-outcome" {
            // the claim and the seal take the base alone; an outcome also names a message, its attempt and what happened to it
            body = with(
                body,
                json!({"messageId": mid(1), "attempt": 1, "outcome": {"kind": "released"}}),
            );
        }
        // not a worker session
        let out = handle(
            req(
                op,
                Some("arn:aws:sts::700000000011:assumed-role/Admin/testuser".into()),
                &body,
            ),
            &d,
        )
        .await;
        assert_eq!(out.status, 403, "{op}");
        let out = handle(req(op, None, &body), &d).await;
        assert_eq!(out.status, 403, "{op}");
        // GET
        let mut get = req(op, arn("direct"), &body);
        get.method = "GET".into();
        assert_eq!(handle(get, &d).await.status, 405, "{op}");
        // another run's body
        let mut other = body.clone();
        other["runId"] = json!("01M4ZHFBV5SDX7J92E6YRH4QTX");
        let out = handle(req(op, arn("direct"), &other), &d).await;
        assert_eq!(out.status, 403, "{op}");
        // another agent of the same run
        let out = handle(
            req(
                op,
                arn("planner"),
                &with(body.clone(), json!({"agentId": "planner"})),
            ),
            &d,
        )
        .await;
        assert_eq!((out.status, op), (403, op));
        assert!(out.body.contains("not_direct_agent"), "{}", out.body);
        // unknown members and a body that is not JSON
        let out = handle(
            req(
                op,
                arn("direct"),
                &with(body.clone(), json!({"colour": "blue"})),
            ),
            &d,
        )
        .await;
        assert_eq!(out.status, 400, "{op}");
    }
    let out = handle(
        InternalRequest {
            method: "POST".into(),
            path: "/direct/internal/message-claim".into(),
            caller_arn: arn("direct"),
            completion_key: None,
            body: "nope".into(),
        },
        &d,
    )
    .await;
    assert_eq!(out.status, 400);
    assert_eq!(
        handle(req("message-unknown", arn("direct"), &base(GEN, "s1")), &d)
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn an_agent_reports_its_session_and_a_message_before_it_is_handed_out_waits_for_it() {
    let d = deps(Fake::new());
    admit(&d, 1, "written before the first session", MessageMode::Next).await;
    let (status, ack) = call(&d, "message-session", sess(GEN, "s1")).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(
        (
            ack["epochNo"].as_u64(),
            ack["held"].as_u64(),
            ack["admission"].as_str()
        ),
        (Some(1), Some(0), Some("open"))
    );
    let r = events(&d, None).await;
    assert_eq!(notices(&r).len(), 1);
}

// ---- the hand-out ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn pending_messages_are_handed_out_in_every_events_response_until_claimed_and_never_after() {
    let d = deps(Fake::new());
    call(&d, "message-session", sess(GEN, "s1")).await;
    assert!(notices(&events(&d, None).await).is_empty());
    admit(&d, 1, "one", MessageMode::Next).await;
    admit(&d, 2, "two", MessageMode::Now).await;
    let r = events(&d, None).await;
    let got: Vec<(String, MessageMode)> = notices(&r)
        .iter()
        .map(|n| (n.message_id.clone(), n.mode))
        .collect();
    assert_eq!(
        got,
        [(mid(2), MessageMode::Now), (mid(1), MessageMode::Next)],
        "interrupts first"
    );
    assert!(notices(&r)[0].epoch.as_deref() == Some("s1") && notices(&r)[0].bytes == 3);
    // the same again: a hand-out changes nothing
    assert_eq!(notices(&events(&d, None).await).len(), 2);
    // a final flush takes no controls and no notices
    assert!(notices(&events(&d, Some(false)).await).is_empty());
    // claimed: no longer handed out
    call(
        &d,
        "message-claim",
        with(base(GEN, "s1"), json!({"messageId": mid(2)})),
    )
    .await;
    let ids: Vec<String> = notices(&events(&d, None).await)
        .iter()
        .map(|n| n.message_id.clone())
        .collect();
    assert_eq!(ids, [mid(1)]);
    // withdrawn: never handed out again
    d.store.ledger().withdraw(RUN, &mid(1)).await.unwrap();
    assert!(notices(&events(&d, None).await).is_empty());
}

#[tokio::test]
async fn a_held_message_is_not_handed_out() {
    let d = deps(Fake::new());
    call(&d, "message-session", sess(GEN, "s1")).await;
    admit(&d, 1, "one", MessageMode::Next).await;
    call(&d, "message-session", sess(GEN, "s2")).await;
    assert!(notices(&events(&d, None).await).is_empty());
}

#[tokio::test]
async fn a_failed_notice_read_is_a_diagnostic_and_never_fails_the_events_post() {
    let mut f = Fake::new();
    f.notices_down = true;
    let d = deps(f);
    let body = json!({"runId": RUN, "agentId": "direct", "events": []});
    let (out, log) = handle_logged(req("events", arn("direct"), &body), &d).await;
    assert_eq!(out.status, 200);
    assert!(log.iter().any(|l| l.contains("message notices")), "{log:?}");
}

#[tokio::test]
async fn a_store_without_a_ledger_hands_out_nothing_and_claims_nothing() {
    let mut f = Fake::new();
    f.no_ledger = true;
    let d = deps(f);
    let r = events(&d, None).await;
    assert!(notices(&r).is_empty());
    let (status, body) = call(&d, "message-claim", base(GEN, "s1")).await;
    assert_eq!(status, 503);
    assert!(!body.to_string().contains("keeps no messages"), "{body}");
}

// ---- the claim -------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_returns_the_text_once_and_a_claim_asked_again_is_marked_redelivered() {
    let d = deps(Fake::new());
    call(&d, "message-session", sess(GEN, "s1")).await;
    admit(&d, 1, "do this", MessageMode::Next).await;
    let (status, v) = call(
        &d,
        "message-claim",
        with(base(GEN, "s1"), json!({"limit": 3})),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["outcome"], "claimed");
    assert_eq!(v["messages"][0]["text"], "do this");
    assert!(v.get("redelivered").is_none());
    let (_, again) = call(&d, "message-claim", base(GEN, "s1")).await;
    assert_eq!(
        (again["outcome"].as_str(), again["redelivered"].as_bool()),
        (Some("claimed"), Some(true))
    );
    let (_, none) = call(
        &d,
        "message-claim",
        with(base(GEN, "s1"), json!({"messageId": mid(9)})),
    )
    .await;
    assert_eq!(none["code"], "not-found");
    assert_eq!(
        d.store
            .ledger()
            .get_message(RUN, &mid(1))
            .await
            .unwrap()
            .unwrap()
            .state,
        MessageState::Dispatching
    );
}

#[tokio::test]
async fn a_stale_incarnation_or_session_is_a_409_with_its_code() {
    let d = deps(Fake::new());
    let (status, v) = call(&d, "message-claim", base(GEN, "s1")).await;
    assert_eq!((status, v["code"].as_str()), (409, Some("no-session")));
    call(&d, "message-session", sess(GEN2, "s1")).await;
    let (status, v) = call(&d, "message-claim", base(GEN, "s1")).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (409, Some("stale-generation"))
    );
    let (status, v) = call(&d, "message-claim", base(GEN2, "other")).await;
    assert_eq!((status, v["code"].as_str()), (409, Some("stale-epoch")));
    let (status, v) = call(&d, "message-seal", base(GEN, "s1")).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (409, Some("stale-generation"))
    );
    // an incarnation the server has not seen replaces the current one, whatever its ULID is; the replaced one is refused for good
    let (status, _) = call(&d, "message-session", sess(GEN, "s1")).await;
    assert_eq!(status, 200);
    let (status, v) = call(&d, "message-session", sess(GEN2, "s1")).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (409, Some("stale-generation"))
    );
    let (status, v) = call(&d, "message-session", sess("not-a-ulid", "s1")).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("bad-input")));
}

#[tokio::test]
async fn a_queued_cancel_wins_over_a_claim() {
    let f = Fake::new();
    f.cancel.store(true, Ordering::SeqCst);
    let d = deps(f);
    call(&d, "message-session", sess(GEN, "s1")).await;
    admit(&d, 1, "too late", MessageMode::Next).await;
    let (status, v) = call(&d, "message-claim", base(GEN, "s1")).await;
    assert_eq!(
        (status, v["outcome"].as_str(), v["reason"].as_str()),
        (200, Some("refused"), Some("cancel-queued"))
    );
    let m = d
        .store
        .ledger()
        .get_message(RUN, &mid(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.state, MessageState::Rejected);
}

#[tokio::test]
async fn a_failed_cancel_read_is_logged_and_the_claim_goes_on() {
    let mut f = Fake::new();
    f.cancel_down = true;
    let d = deps(f);
    call(&d, "message-session", sess(GEN, "s1")).await;
    admit(&d, 1, "x", MessageMode::Next).await;
    let (out, log) = handle_logged(req("message-claim", arn("direct"), &base(GEN, "s1")), &d).await;
    assert_eq!(out.status, 200);
    assert!(log.iter().any(|l| l.contains("cancel check")), "{log:?}");
}

// ---- the seal and the outcome ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_seal_returns_a_late_message_and_then_closes_admission_for_good() {
    let d = deps(Fake::new());
    call(&d, "message-session", sess(GEN, "s1")).await;
    admit(&d, 1, "arrived at the last moment", MessageMode::Next).await;
    let (status, v) = call(&d, "message-seal", base(GEN, "s1")).await;
    assert_eq!((status, v["outcome"].as_str()), (200, Some("pending")));
    assert_eq!(v["messages"][0]["messageId"], mid(1));
    call(&d, "message-claim", base(GEN, "s1")).await;
    let (status, v) = call(
        &d,
        "message-outcome",
        with(
            base(GEN, "s1"),
            json!({"messageId": mid(1), "attempt": 1, "outcome": {"kind": "submitted", "turn": 2}}),
        ),
    )
    .await;
    assert_eq!((status, v["state"].as_str()), (200, Some("delivered")));
    let (_, v) = call(&d, "message-seal", base(GEN, "s1")).await;
    assert_eq!(
        (v["outcome"].as_str(), v["watermark"].as_u64()),
        (Some("sealed"), Some(1))
    );
    // admission is closed: the next message is refused by the ledger
    let e = d
        .store
        .ledger()
        .admit(
            RUN,
            OWNER,
            &SendMessageRequest {
                message_id: mid(2),
                text: "x".into(),
                mode: MessageMode::Next,
                confirms: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        e,
        LedgerError::Refused(message_ledger::Refusal::RunSealed(_))
    ));
    assert!(notices(&events(&d, None).await).is_empty());
}

#[tokio::test]
async fn an_outcome_the_agent_may_not_report_is_refused_and_changes_nothing() {
    let d = deps(Fake::new());
    call(&d, "message-session", sess(GEN, "s1")).await;
    admit(&d, 1, "x", MessageMode::Next).await;
    let (status, v) = call(
        &d,
        "message-outcome",
        with(
            base(GEN, "s1"),
            json!({"messageId": mid(1), "attempt": 1, "outcome": {"kind": "rejected", "reason": "run-sealed"}}),
        ),
    )
    .await;
    assert_eq!((status, v["code"].as_str()), (409, Some("bad-transition")));
    let (status, _) = call(
        &d,
        "message-outcome",
        with(
            base(GEN, "s1"),
            json!({"messageId": mid(1), "attempt": 1, "outcome": {"kind": "withdrawn"}}),
        ),
    )
    .await;
    assert_eq!(status, 400, "an agent has no word for withdrawn");
    assert_eq!(
        d.store
            .ledger()
            .get_message(RUN, &mid(1))
            .await
            .unwrap()
            .unwrap()
            .state,
        MessageState::Pending
    );
    let (status, v) = call(
        &d,
        "message-outcome",
        with(
            base(GEN, "s1"),
            json!({"messageId": mid(1), "attempt": 1, "outcome": {"kind": "submitted", "turn": 1}}),
        ),
    )
    .await;
    assert_eq!(
        (status, v["code"].as_str()),
        (409, Some("bad-transition")),
        "a pending message was never claimed"
    );
}

#[tokio::test]
async fn the_access_line_never_carries_a_message_text() {
    let d = deps(Fake::new());
    call(&d, "message-session", sess(GEN, "s1")).await;
    admit(&d, 1, "a secret the user typed", MessageMode::Next).await;
    let r = req("message-claim", arn("direct"), &base(GEN, "s1"));
    let (out, log) = handle_logged(r.clone(), &d).await;
    assert!(
        out.body.contains("a secret the user typed"),
        "agentd gets the text"
    );
    let lines = worker_api::lambda::log_lines(&r, out.status, &log);
    assert!(
        lines.iter().all(|l| !l.contains("a secret the user typed")),
        "{lines:?}"
    );
}

// ---- the review fixes of 2026-10-16: attempt, session number, seal that waits ---------------------------------------------

#[tokio::test]
async fn a_report_needs_its_number_and_an_outcome_needs_its_attempt() {
    let d = deps(Fake::new());
    let (status, v) = call(&d, "message-session", base(GEN, "s1")).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("bad_request")));
    let (status, _) = call(&d, "message-session", sess(GEN, "s0")).await;
    assert_eq!(status, 400, "sessions are numbered from 1");
    call(&d, "message-session", sess(GEN, "s1")).await;
    admit(&d, 1, "x", MessageMode::Next).await;
    call(&d, "message-claim", base(GEN, "s1")).await;
    let (status, v) = call(
        &d,
        "message-outcome",
        with(
            base(GEN, "s1"),
            json!({"messageId": mid(1), "outcome": {"kind": "released"}}),
        ),
    )
    .await;
    assert_eq!((status, v["code"].as_str()), (400, Some("bad_request")));
}

#[tokio::test]
async fn the_late_retry_of_an_earlier_attempt_is_409_stale_attempt_and_settles_nothing() {
    let d = deps(Fake::new());
    call(&d, "message-session", sess(GEN, "s1")).await;
    admit(&d, 1, "x", MessageMode::Next).await;
    call(&d, "message-claim", base(GEN, "s1")).await;
    let release = |attempt: u32| {
        with(
            base(GEN, "s1"),
            json!({"messageId": mid(1), "attempt": attempt, "outcome": {"kind": "released"}}),
        )
    };
    let (status, v) = call(&d, "message-outcome", release(1)).await;
    assert_eq!((status, v["state"].as_str()), (200, Some("pending")));
    let (status, v) = call(&d, "message-claim", base(GEN, "s1")).await;
    assert_eq!(status, 200);
    assert_eq!(v["messages"][0]["attempt"], 2, "{v}");
    // the retry of the first release arrives while the second claim is out
    let (status, v) = call(&d, "message-outcome", release(1)).await;
    assert_eq!((status, v["code"].as_str()), (409, Some("stale-attempt")));
    let (status, v) = call(&d, "message-outcome", release(2)).await;
    assert_eq!((status, v["state"].as_str()), (200, Some("pending")));
}

#[tokio::test]
async fn a_normal_seal_waits_for_the_outcome_of_a_claimed_message() {
    let d = deps(Fake::new());
    call(&d, "message-session", sess(GEN, "s1")).await;
    admit(&d, 1, "x", MessageMode::Next).await;
    call(&d, "message-claim", base(GEN, "s1")).await;
    let (status, v) = call(&d, "message-seal", base(GEN, "s1")).await;
    assert_eq!((status, v["code"].as_str()), (409, Some("bad-transition")));
    assert!(
        v["message"].as_str().unwrap_or("").contains("outcome"),
        "{v}"
    );
    let (status, v) = call(
        &d,
        "message-seal",
        with(base(GEN, "s1"), json!({"mode": "terminal"})),
    )
    .await;
    assert_eq!((status, v["outcome"].as_str()), (200, Some("sealed")));
    assert_eq!(v["deliveryUnknown"], 1, "{v}");
}

#[tokio::test]
async fn a_late_report_of_an_older_session_is_answered_and_changes_nothing() {
    let d = deps(Fake::new());
    call(&d, "message-session", sess(GEN, "s1")).await;
    call(&d, "message-session", sess(GEN, "s2")).await;
    let (status, ack) = call(&d, "message-session", sess(GEN, "s1")).await;
    assert_eq!((status, ack["epochNo"].as_u64()), (200, Some(2)));
    let (status, v) = call(
        &d,
        "message-session",
        with(base(GEN, "other"), json!({"number": 2})),
    )
    .await;
    assert_eq!((status, v["code"].as_str()), (409, Some("bad-transition")));
}
