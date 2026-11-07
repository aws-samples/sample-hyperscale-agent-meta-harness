//! `ham`'s own client calls (`Api::send_message`, `list_messages`, `get_message`, `withdraw_message`) go over HTTP with SigV4 to the real
//! `control_api::handler` (the stand-in API of `tests/support`); a simulated agentd calls the real `worker_api::handler` routes
//! (`message-session`, `message-claim`, `message-seal`, `message-outcome` and the events post that hands the notices out). Both sides use the
//! real `message-ledger` on one in-memory table, so a race is decided by the ledger's conditions exactly as it is on DynamoDB.
//!
//! Every row asserts its own trigger and prints `TRIGGER PRESENT <row>: <what>` first: a row whose trigger is absent fails, it never passes by
//! default. A row's result line is `DIRECT-<row> PASS <what was measured>`. When `HAMH_MESSAGES_EVIDENCE_OUT` names a file, the lines are
//! written there as JSON for the evidence folder.
//!
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, ApiError, Retry};
use ham::https::HttpsPlane;
use ham::message_routes::mint_message_id;
use ham::sigv4::Signer;
use ham_core::messages::{confirmation, explain_refusal, plain, status_line};
use harness_protocol::api::internal::{EventsBatchResponse, PendingControl, ProcessCredentials};
use harness_protocol::common::Usage;
use harness_protocol::envelope::{Envelope, EventType};
use harness_protocol::goal::GoalStatus;
use harness_protocol::messages::{
    Admission, ClaimResponse, MessageMode, MessageNotice, MessageOutcome, MessageState,
    RejectReason, SealMode, SealResponse, SendOutcome,
};
use harness_protocol::store::{AgentItem, TurnItem};
use message_ledger::mem::MemTable;
use message_ledger::{Ledger, LedgerError};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{Fake, REGION, creds};
use worker_api::creds::MintPlan;
use worker_api::handler::{
    CallbackError, Callbacks, Deps, InternalRequest, MintError, Minter, Store, StoreError,
    TokenContext, handle,
};

const ROLE: &str = "hamh-live-harness-worker-session";
const GEN: &str = "01M5G000000000000000000001";

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn say(line: String) {
    println!("{line}");
    LINES.lock().unwrap().push(line);
}

fn trigger(row: &str, what: &str) {
    say(format!("TRIGGER PRESENT {row}: {what}"));
}

fn pass(row: &str, what: &str) {
    say(format!("DIRECT-{row} PASS {what}"));
    if let Ok(path) = std::env::var("HAMH_MESSAGES_EVIDENCE_OUT") {
        let lines = LINES.lock().unwrap().clone();
        std::fs::write(
            path,
            serde_json::to_string_pretty(&json!({"lines": lines})).unwrap(),
        )
        .unwrap();
    }
}

// ---- the user's side: ham's own client over HTTP ---------------------------------------------------------------------------

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(5),
    })
}

async fn start(fake: &Fake) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": "Add a percent_change function", "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    let run = api(fake)
        .start_direct_run(&req, "k-messages")
        .await
        .unwrap()
        .run_id;
    fake.mem.add_agent(&run, "direct", "root", "running");
    fake.mem.enable_messages(&run);
    run
}

fn http_code(e: &ApiError) -> (u16, String) {
    match e {
        ApiError::Http { status, error } => (*status, error.code.clone()),
        other => panic!("not an HTTP answer: {other:?}"),
    }
}

// ---- agentd's side: the real worker-api over the same ledger ---------------------------------------------------------------

struct AgentStore {
    table: MemTable,
    mem: support::Mem,
}

impl AgentStore {
    fn ledger(&self) -> Ledger<MemTable> {
        Ledger::new(self.table.clone())
    }
}

impl Store for AgentStore {
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
    async fn cancel_queued(&self, run: &str, _: &str) -> Result<bool, StoreError> {
        Ok(self
            .mem
            .controls_of(run)
            .iter()
            .any(|c| c.control == PendingControl::Cancel && c.delivered_at.is_none()))
    }
    async fn message_session(
        &self,
        run: &str,
        r: &harness_protocol::messages::SessionReport,
    ) -> Result<harness_protocol::messages::SessionAck, LedgerError> {
        self.ledger()
            .report_session(run, &r.generation, &r.epoch, r.number)
            .await
    }
    async fn message_claim(
        &self,
        r: &harness_protocol::messages::ClaimRequest,
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
    async fn message_seal(
        &self,
        r: &harness_protocol::messages::SealRequest,
    ) -> Result<SealResponse, LedgerError> {
        self.ledger()
            .seal(&r.run_id, &r.generation, &r.epoch, r.mode)
            .await
    }
    async fn message_outcome(
        &self,
        r: &harness_protocol::messages::OutcomeRequest,
    ) -> Result<harness_protocol::messages::OutcomeResponse, LedgerError> {
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

struct NoMint;
impl Minter for NoMint {
    async fn mint(&self, _: &MintPlan) -> Result<ProcessCredentials, MintError> {
        Err(MintError("unused".into()))
    }
}
struct NoCallbacks;
impl Callbacks for NoCallbacks {
    async fn turn_done(&self, _: &str, _: &TokenContext, _: &str) -> Result<(), CallbackError> {
        Ok(())
    }
    async fn turn_alive(&self, _: &str, _: &TokenContext) -> Result<(), CallbackError> {
        Ok(())
    }
}

/// A simulated agentd of one run: an incarnation, a session, and the calls it makes.
struct Agentd {
    deps: Deps<AgentStore, NoMint, NoCallbacks>,
    run: String,
    generation: String,
    epoch: String,
    /// The attempt of each claim this agentd was given, by message id: an outcome repeats the attempt it answers.
    attempts: std::sync::Mutex<std::collections::BTreeMap<String, u32>>,
}

impl Agentd {
    fn new(fake: &Fake, run: &str, generation: &str, epoch: &str) -> Agentd {
        Agentd {
            deps: Deps {
                store: AgentStore {
                    table: fake.mem.msgs.clone(),
                    mem: fake.mem.clone(),
                },
                minter: NoMint,
                callbacks: NoCallbacks,
                role_name: ROLE.into(),
                bucket: "b".into(),
            },
            run: run.into(),
            generation: generation.into(),
            epoch: epoch.into(),
            attempts: std::sync::Mutex::default(),
        }
    }

    fn base(&self) -> Value {
        json!({"runId": self.run, "agentId": "direct", "generation": self.generation, "epoch": self.epoch})
    }

    async fn post(&self, op: &str, body: Value) -> (u16, Value) {
        let out = handle(
            InternalRequest {
                method: "POST".into(),
                path: format!("/live/internal/{op}"),
                caller_arn: Some(format!(
                    "arn:aws:sts::800000000022:assumed-role/{ROLE}/{}.direct",
                    self.run
                )),
                completion_key: None,
                body: body.to_string(),
            },
            &self.deps,
        )
        .await;
        let v: Value = serde_json::from_str(&out.body).unwrap_or(Value::Null);
        if op == "message-claim" {
            for m in v["messages"].as_array().into_iter().flatten() {
                if let (Some(id), Some(n)) = (m["messageId"].as_str(), m["attempt"].as_u64()) {
                    self.attempts
                        .lock()
                        .unwrap()
                        .insert(id.to_string(), u32::try_from(n).unwrap());
                }
            }
        }
        (out.status, v)
    }

    /// The session report: the number is the ordinal of the session in this incarnation, here the digits of its name ("sess-2").
    async fn session(&self) -> Value {
        let number: u32 = self
            .epoch
            .trim_start_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .unwrap_or(1);
        let mut body = self.base();
        body["number"] = json!(number);
        let (s, v) = self.post("message-session", body).await;
        assert_eq!(s, 200, "{v}");
        v
    }

    /// The events post agentd makes every couple of seconds; the notices ride in its answer.
    async fn events(&self) -> Vec<MessageNotice> {
        let (s, v) = self
            .post(
                "events",
                json!({"runId": self.run, "agentId": "direct", "events": []}),
            )
            .await;
        assert_eq!(s, 200, "{v}");
        let r: EventsBatchResponse = serde_json::from_value(v).unwrap();
        r.pending
            .into_iter()
            .filter_map(|c| match c {
                PendingControl::Message(n) => Some(n),
                _ => None,
            })
            .collect()
    }

    async fn claim(&self, limit: u8) -> (u16, Value) {
        let mut b = self.base();
        b["limit"] = json!(limit);
        self.post("message-claim", b).await
    }

    async fn seal(&self, mode: SealMode) -> SealResponse {
        let mut b = self.base();
        b["mode"] = serde_json::to_value(mode).unwrap();
        let (s, v) = self.post("message-seal", b).await;
        assert_eq!(s, 200, "{v}");
        serde_json::from_value(v).unwrap()
    }

    async fn outcome(&self, id: &str, o: MessageOutcome) -> (u16, Value) {
        let mut b = self.base();
        b["messageId"] = json!(id);
        b["attempt"] = json!(self.attempts.lock().unwrap().get(id).copied().unwrap_or(0));
        b["outcome"] = serde_json::to_value(o).unwrap();
        self.post("message-outcome", b).await
    }
}

fn claimed_ids(v: &Value) -> Vec<String> {
    v["messages"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| m["messageId"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ---- DIRECT-T51: a message to a working Cline is delivered after its turn ------------------------------------------------

#[tokio::test]
async fn direct_t51_a_message_sent_while_cline_works_is_delivered_at_the_end_of_the_turn_and_shown_as_delivered()
 {
    let fake = Fake::start().await;
    let run = start(&fake).await;
    let agentd = Agentd::new(&fake, &run, GEN, "sess-1");
    agentd.session().await;
    // Cline is working: agentd has a prompt open and has asked nothing; the run has no question
    let id = mint_message_id();
    let sent = api(&fake)
        .send_message(
            &run,
            &plain(
                id.clone(),
                "Also add a README section on the retry rules.",
                MessageMode::Next,
            ),
        )
        .await
        .unwrap();
    trigger(
        "T51",
        "the run is active with a live direct agent, no question is open, and the server holds the message pending",
    );
    assert_eq!(
        (sent.outcome, sent.message.state),
        (SendOutcome::Accepted, MessageState::Pending)
    );
    assert!(
        fake.mem
            .inner
            .lock()
            .unwrap()
            .runs
            .get(&run)
            .unwrap()
            .status
            == GoalStatus::Active
    );
    // agentd is told within its next events post, with no text
    let notices = agentd.events().await;
    trigger(
        "T51",
        "agentd's events response carries the notice while the message is pending",
    );
    assert_eq!(
        notices
            .iter()
            .map(|n| n.message_id.clone())
            .collect::<Vec<_>>(),
        std::slice::from_ref(&id)
    );
    // the user sees it as waiting, in words
    let page = api(&fake).list_messages(&run).await.unwrap();
    assert_eq!(
        status_line(&page.messages[0], page.epoch_no),
        "waiting for Cline to finish its turn"
    );
    // the turn ends: agentd claims the message, sends it, and says so
    let (s, claim) = agentd.claim(3).await;
    assert_eq!((s, claim["outcome"].as_str()), (200, Some("claimed")));
    assert_eq!(
        claim["messages"][0]["text"],
        "Also add a README section on the retry rules."
    );
    let (s, o) = agentd
        .outcome(&id, MessageOutcome::Submitted { turn: 2 })
        .await;
    assert_eq!((s, o["state"].as_str()), (200, Some("delivered")));
    // Cline answers in its next turn (the stand-in plays the engine's message event)
    fake.mem.agent(
        &run,
        "direct",
        EventType::TurnProgress,
        json!({"turn": 2, "kind": "message", "text": "Added the retry section."}),
    );
    let page = api(&fake).list_messages(&run).await.unwrap();
    assert_eq!(page.messages[0].state, MessageState::Delivered);
    assert_eq!(
        status_line(&page.messages[0], page.epoch_no),
        "delivered in turn 2"
    );
    assert!(
        agentd.events().await.is_empty(),
        "a claimed message is handed out no more"
    );
    pass(
        "T51",
        "send 202 pending; notice handed out; claimed once; submitted in turn 2; the page reads 'delivered in turn 2'; no further hand-out",
    );
}

#[tokio::test]
async fn direct_t51_an_uncertain_send_is_reconciled_by_its_id_and_a_retry_makes_no_second_message()
{
    let fake = Fake::start().await;
    let run = start(&fake).await;
    let id = mint_message_id();
    let req = plain(id.clone(), "Please use pytest fixtures.", MessageMode::Next);
    let first = api(&fake).send_message(&run, &req).await.unwrap();
    // the client never saw the 202: it asks for the message by its id, then sends the same request again
    trigger(
        "T51",
        "the first send was stored and its answer is treated as lost",
    );
    let found = api(&fake).get_message(&run, &id).await.unwrap();
    assert_eq!(found.message.state, MessageState::Pending);
    let again = api(&fake).send_message(&run, &req).await.unwrap();
    assert_eq!(
        (first.outcome, again.outcome),
        (SendOutcome::Accepted, SendOutcome::AlreadyAccepted)
    );
    assert_eq!(
        api(&fake).list_messages(&run).await.unwrap().messages.len(),
        1
    );
    // the same id with other text is refused and says why
    let e = api(&fake)
        .send_message(
            &run,
            &plain(id, "Something else entirely.", MessageMode::Next),
        )
        .await
        .unwrap_err();
    assert_eq!(http_code(&e), (409, "idempotency-conflict".into()));
    pass(
        "T51",
        "an uncertain send read back by id; the retry answered already-accepted; one message; a changed payload refused",
    );
}

// ---- DIRECT-T62: the pending lifecycle -----------------------------------------------------------------------------------

#[tokio::test]
async fn direct_t62_a_withdrawn_message_is_never_delivered() {
    let fake = Fake::start().await;
    let run = start(&fake).await;
    let agentd = Agentd::new(&fake, &run, GEN, "sess-1");
    agentd.session().await;
    let id = mint_message_id();
    api(&fake)
        .send_message(
            &run,
            &plain(id.clone(), "never mind this one", MessageMode::Next),
        )
        .await
        .unwrap();
    assert_eq!(agentd.events().await.len(), 1);
    trigger(
        "T62",
        "the message is pending and agentd has been told about it",
    );
    let w = api(&fake).withdraw_message(&run, &id).await.unwrap();
    assert_eq!(w.message.state, MessageState::Withdrawn);
    assert!(
        agentd.events().await.is_empty(),
        "a withdrawn message is not handed out again"
    );
    let (s, claim) = agentd.claim(3).await;
    assert_eq!((s, claim["outcome"].as_str()), (200, Some("nothing")));
    let (s, by_id) = agentd
        .post("message-claim", {
            let mut b = agentd.base();
            b["messageId"] = json!(id);
            b
        })
        .await;
    assert_eq!(
        (s, by_id["outcome"].as_str(), by_id["state"].as_str()),
        (200, Some("not-pending"), Some("withdrawn"))
    );
    assert_eq!(
        api(&fake)
            .get_message(&run, &id)
            .await
            .unwrap()
            .message
            .state,
        MessageState::Withdrawn
    );
    pass(
        "T62",
        "withdrawn while pending; not handed out; a claim by id answers not-pending withdrawn; the message stays withdrawn",
    );
}

#[tokio::test]
async fn direct_t62_a_message_after_the_seal_is_rejected_with_the_reason_and_stores_nothing() {
    let fake = Fake::start().await;
    let run = start(&fake).await;
    let agentd = Agentd::new(&fake, &run, GEN, "sess-1");
    agentd.session().await;
    let sealed = agentd.seal(SealMode::Normal).await;
    trigger(
        "T62",
        "agentd's seal found nothing pending and closed admission",
    );
    assert_eq!(
        sealed,
        SealResponse::Sealed {
            watermark: 0,
            rejected: 0,
            delivery_unknown: 0
        }
    );
    let e = api(&fake)
        .send_message(
            &run,
            &plain(mint_message_id(), "one more thing", MessageMode::Next),
        )
        .await
        .unwrap_err();
    let (status, code) = http_code(&e);
    assert_eq!((status, code.as_str()), (409, "run-sealed"));
    let words = explain_refusal(status, &code, "");
    assert!(words.definitive && words.restore_text, "{words:?}");
    assert!(words.text.contains("ending"), "{}", words.text);
    let page = api(&fake).list_messages(&run).await.unwrap();
    assert_eq!(
        (page.messages.len(), page.admission),
        (0, Admission::Closed)
    );
    pass(
        "T62",
        "409 run-sealed after the seal; the box text is restored; nothing stored; the page reports admission closed",
    );
}

#[tokio::test]
async fn direct_t62_a_message_that_arrives_at_the_last_moment_is_delivered_not_lost() {
    let fake = Fake::start().await;
    let run = start(&fake).await;
    let agentd = Agentd::new(&fake, &run, GEN, "sess-1");
    agentd.session().await;
    // Cline's last turn ended; agentd would declare the run done. The user's message lands before agentd asks the seal.
    let id = mint_message_id();
    api(&fake)
        .send_message(
            &run,
            &plain(
                id.clone(),
                "last thing: run the formatter",
                MessageMode::Next,
            ),
        )
        .await
        .unwrap();
    let first = agentd.seal(SealMode::Normal).await;
    trigger(
        "T62",
        "the seal found a pending message and did not close admission",
    );
    match first {
        SealResponse::Pending { messages } => assert_eq!(messages[0].message_id, id),
        other => panic!("the seal must return the late message: {other:?}"),
    }
    assert_eq!(
        api(&fake).list_messages(&run).await.unwrap().admission,
        Admission::Open
    );
    let (_, claim) = agentd.claim(3).await;
    assert_eq!(claimed_ids(&claim), std::slice::from_ref(&id));
    agentd
        .outcome(&id, MessageOutcome::Submitted { turn: 3 })
        .await;
    assert_eq!(
        agentd.seal(SealMode::Normal).await,
        SealResponse::Sealed {
            watermark: 1,
            rejected: 0,
            delivery_unknown: 0
        }
    );
    assert_eq!(
        api(&fake)
            .get_message(&run, &id)
            .await
            .unwrap()
            .message
            .state,
        MessageState::Delivered
    );
    pass(
        "T62",
        "the seal returned the late message; it was claimed and delivered in turn 3; the second seal closed admission with watermark 1",
    );
}

#[tokio::test]
async fn direct_t62_a_held_message_after_a_session_change_is_confirmed_once_and_only_the_copy_is_delivered()
 {
    let fake = Fake::start().await;
    let run = start(&fake).await;
    let one = Agentd::new(&fake, &run, GEN, "sess-1");
    one.session().await;
    let id = mint_message_id();
    api(&fake)
        .send_message(
            &run,
            &plain(id.clone(), "keep the public API stable", MessageMode::Next),
        )
        .await
        .unwrap();
    // the session is replaced (a fresh session started): the message was written to session 1
    let two = Agentd::new(&fake, &run, GEN, "sess-2");
    let ack = two.session().await;
    assert_eq!(ack["held"], 1);
    let page = api(&fake).list_messages(&run).await.unwrap();
    trigger(
        "T62",
        "the message is held after the session changed from 1 to 2",
    );
    assert_eq!(page.messages[0].state, MessageState::Held);
    assert_eq!(
        status_line(&page.messages[0], page.epoch_no),
        "held: written to session 1, now session 2; confirm to send it to the new session"
    );
    assert!(
        two.events().await.is_empty(),
        "a held message is not handed out"
    );
    // the user confirms against the session they were shown
    let copy = confirmation(
        mint_message_id(),
        &page.messages[0],
        page.epoch.as_deref().unwrap(),
    )
    .unwrap();
    let sent = api(&fake).send_message(&run, &copy).await.unwrap();
    assert_eq!(
        (sent.message.state, sent.message.confirms.as_deref()),
        (MessageState::Pending, Some(id.as_str()))
    );
    let after = api(&fake).get_message(&run, &id).await.unwrap().message;
    assert_eq!(
        (after.state, after.superseded_by.as_deref()),
        (MessageState::Superseded, Some(copy.message_id.as_str()))
    );
    // a confirmation made against a session that has changed again fails and sends nothing
    let three = Agentd::new(&fake, &run, GEN, "sess-3");
    three.session().await;
    // the copy is now held too (it was written to session 2); confirming it against the session the user was shown before is refused
    let held_copy = api(&fake)
        .get_message(&run, &copy.message_id)
        .await
        .unwrap()
        .message;
    assert_eq!(held_copy.state, MessageState::Held);
    let stale = confirmation(mint_message_id(), &held_copy, "sess-2").unwrap();
    let e = api(&fake).send_message(&run, &stale).await.unwrap_err();
    assert_eq!(http_code(&e), (409, "epoch-changed".into()));
    // confirm against the current session and deliver exactly that copy
    let again = confirmation(mint_message_id(), &held_copy, "sess-3").unwrap();
    api(&fake).send_message(&run, &again).await.unwrap();
    let (_, claim) = three.claim(3).await;
    assert_eq!(claimed_ids(&claim), std::slice::from_ref(&again.message_id));
    pass(
        "T62",
        "held after the session change; confirmed against the session shown; original superseded; a stale confirmation refused with epoch-changed; exactly one copy claimed",
    );
}

#[tokio::test]
async fn direct_t62_a_held_message_does_not_stop_the_run_from_completing() {
    let fake = Fake::start().await;
    let run = start(&fake).await;
    let one = Agentd::new(&fake, &run, GEN, "sess-1");
    one.session().await;
    let id = mint_message_id();
    api(&fake)
        .send_message(
            &run,
            &plain(id.clone(), "this one will be held", MessageMode::Next),
        )
        .await
        .unwrap();
    let two = Agentd::new(&fake, &run, GEN, "sess-2");
    two.session().await;
    assert_eq!(
        api(&fake)
            .get_message(&run, &id)
            .await
            .unwrap()
            .message
            .state,
        MessageState::Held
    );
    trigger("T62", "a held message exists when agentd asks the seal");
    let sealed = two.seal(SealMode::Normal).await;
    assert_eq!(
        sealed,
        SealResponse::Sealed {
            watermark: 1,
            rejected: 1,
            delivery_unknown: 0
        },
        "the seal does not wait for the user"
    );
    let m = api(&fake).get_message(&run, &id).await.unwrap().message;
    assert_eq!(
        (m.state, m.reason),
        (MessageState::Rejected, Some(RejectReason::RunSealed))
    );
    assert_eq!(status_line(&m, 2), "not sent: the run was ending");
    pass(
        "T62",
        "the seal answered at once with the held message rejected run-sealed; completion was not held for a confirmation",
    );
}

#[tokio::test]
async fn direct_t62_a_stop_by_the_user_wins_over_a_message_and_the_message_is_rejected_as_cancelled()
 {
    let fake = Fake::start().await;
    let run = start(&fake).await;
    let agentd = Agentd::new(&fake, &run, GEN, "sess-1");
    agentd.session().await;
    let id = mint_message_id();
    api(&fake)
        .send_message(
            &run,
            &plain(
                id.clone(),
                "pending when the user stops the run",
                MessageMode::Next,
            ),
        )
        .await
        .unwrap();
    api(&fake).cancel_run(&run).await.unwrap();
    trigger(
        "T62",
        "a Cancel is queued for agentd and the user's message was pending",
    );
    assert!(
        fake.mem
            .controls_of(&run)
            .iter()
            .any(|c| c.control == PendingControl::Cancel)
    );
    let m = api(&fake).get_message(&run, &id).await.unwrap().message;
    assert_eq!(
        (m.state, m.reason),
        (MessageState::Rejected, Some(RejectReason::RunCancelled))
    );
    let (_, claim) = agentd.claim(3).await;
    assert_eq!(claim["outcome"], "refused");
    let e = api(&fake)
        .send_message(
            &run,
            &plain(mint_message_id(), "too late", MessageMode::Next),
        )
        .await
        .unwrap_err();
    assert_eq!(http_code(&e), (409, "run-sealed".into()));
    pass(
        "T62",
        "cancel queued; the pending message ended rejected run-cancelled; the claim was refused; a new message is refused",
    );
}

#[tokio::test]
async fn direct_t62_a_run_on_an_older_image_refuses_a_message_and_keeps_the_text() {
    let fake = Fake::start().await;
    let run = start(&fake).await;
    {
        let mut g = fake.mem.inner.lock().unwrap();
        for a in g.agent_items.iter_mut().filter(|a| a.run_id == run) {
            a.harness_controls.clear();
        }
    }
    trigger("T62", "the run's Agent registered no message control");
    let e = api(&fake)
        .send_message(
            &run,
            &plain(mint_message_id(), "can you hear me", MessageMode::Next),
        )
        .await
        .unwrap_err();
    let (status, code) = http_code(&e);
    assert_eq!((status, code.as_str()), (409, "no-message-control"));
    let words = explain_refusal(status, &code, "");
    assert_eq!(
        words.text,
        "Cline is not waiting for an answer; /cancel stops the run and keeps the work"
    );
    assert!(words.restore_text);
    assert_eq!(
        api(&fake).list_messages(&run).await.unwrap().messages.len(),
        0
    );
    pass(
        "T62",
        "409 no-message-control with the box words; the text stays in the box; nothing stored",
    );
}

// ---- DIRECT-T65: /now ------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn direct_t65_the_server_side_of_now_hands_the_interrupt_out_first_and_a_released_claim_returns_as_next()
 {
    let fake = Fake::start().await;
    let run = start(&fake).await;
    let agentd = Agentd::new(&fake, &run, GEN, "sess-1");
    agentd.session().await;
    let queued = mint_message_id();
    api(&fake)
        .send_message(
            &run,
            &plain(
                queued.clone(),
                "queued for the end of the turn",
                MessageMode::Next,
            ),
        )
        .await
        .unwrap();
    let now_id = mint_message_id();
    let sent = api(&fake)
        .send_message(
            &run,
            &plain(
                now_id.clone(),
                "stop that and run the tests instead",
                MessageMode::Now,
            ),
        )
        .await
        .unwrap();
    trigger(
        "T65",
        "an interrupt message is pending behind a next message",
    );
    assert_eq!(sent.message.mode, MessageMode::Now);
    let notices = agentd.events().await;
    assert_eq!(
        notices
            .iter()
            .map(|n| n.message_id.clone())
            .collect::<Vec<_>>(),
        [now_id.clone(), queued.clone()]
    );
    assert_eq!(notices[0].mode, MessageMode::Now);
    // agentd claims the interrupt by id, then cannot cancel the turn in time and gives the claim back
    let mut b = agentd.base();
    b["messageId"] = json!(now_id);
    let (s, claim) = agentd.post("message-claim", b).await;
    assert_eq!((s, claim["outcome"].as_str()), (200, Some("claimed")));
    assert_eq!(
        api(&fake)
            .get_message(&run, &now_id)
            .await
            .unwrap()
            .message
            .state,
        MessageState::Dispatching
    );
    let (s, o) = agentd.outcome(&now_id, MessageOutcome::Released).await;
    assert_eq!((s, o["state"].as_str()), (200, Some("pending")));
    let back = api(&fake).get_message(&run, &now_id).await.unwrap().message;
    assert_eq!(
        (back.state, back.mode),
        (MessageState::Pending, MessageMode::Next)
    );
    assert_eq!(
        status_line(&back, 1),
        "waiting for Cline to finish its turn"
    );
    pass(
        "T65",
        "stand-in contract only: the interrupt is handed out first; claimed by id; released and demoted to next. The live row (a shell tool in progress, the engine's cancel) is NOT-RUN here",
    );
}
