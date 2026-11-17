//! The fence of a session generation, and the worker's notice that its session is being ended.
//!
//! A session that was stopped from outside lives on for about a minute without its storage, and its worker keeps calling the service. Between
//! the stop and the start of the next session the Agent item still names the old generation, so nothing told the old worker's calls from the
//! live session's. The fence does: `fencedGeneration` on the Agent item, set by worker-api when the worker gives notice of its end or by
//! run-session before it stops a session that left no result. While it equals the generation the item is at, the session's credentials,
//! heartbeats, turn-complete and message calls are refused (409 `stale-generation`), and its events are stored and answered with no control
//! and no message notice: the worker's last sentences are evidence, and they move nothing.
//!
//! The notice is one heartbeat with `ending: true` and the completion key. With a valid key, the generation the run is in and an open turn,
//! worker-api fences the generation and ends the wait for the session with the output `{"sessionEnded": {...}}`, which is no result: the
//! session counts as lost, at once, where it was called lost only after 600 seconds of silence.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use harness_protocol::api::ApiError;
use harness_protocol::api::internal::{EventsBatchResponse, PendingControl};
use harness_protocol::common::{Usage, is_utc_timestamp};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::{AgentItem, CredsBinding, TurnItem};
use serde_json::{Value, json};
use std::sync::Mutex;
use worker_api::creds::MintPlan;
use worker_api::handler::{
    CallbackError, Callbacks, Deps, InternalRequest, InternalResponse, MintError, Minter, Store,
    StoreError, TokenContext, handle, handle_logged,
};

const ROLE: &str = "hamh-direct-harness-worker-session";
const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const NONCE: &str = "the-first-nonce-of-the-session";

fn key() -> String {
    CredsBinding::completion_key(NONCE)
}

#[derive(Default)]
struct World {
    agent: Mutex<Option<AgentItem>>,
    turn: Mutex<Option<TurnItem>>,
    events: Mutex<Vec<Envelope>>,
    heartbeats: Mutex<u32>,
    completes: Mutex<Vec<u32>>,
    controls_taken: Mutex<u32>,
    notices_read: Mutex<u32>,
    fences: Mutex<Vec<u32>>,
    queued: Mutex<Vec<PendingControl>>,
}

impl Store for World {
    async fn get_agent(&self, _run: &str, _agent: &str) -> Result<Option<AgentItem>, StoreError> {
        Ok(self.agent.lock().unwrap().clone())
    }
    async fn get_goal_status(&self, _run: &str) -> Result<Option<GoalStatus>, StoreError> {
        Ok(Some(GoalStatus::Active))
    }
    async fn rotate_nonce(
        &self,
        _run: &str,
        _agent: &str,
        old: &str,
        new: &str,
    ) -> Result<(), StoreError> {
        let mut g = self.agent.lock().unwrap();
        let b = g
            .as_mut()
            .and_then(|a| a.creds.as_mut())
            .ok_or(StoreError::Conflict)?;
        if b.nonce_sha256 != old {
            return Err(StoreError::Conflict);
        }
        b.nonce_sha256 = new.to_string();
        Ok(())
    }
    async fn put_events(
        &self,
        _run: &str,
        _agent: &str,
        events: &[Envelope],
    ) -> Result<(), StoreError> {
        self.events.lock().unwrap().extend_from_slice(events);
        Ok(())
    }
    async fn record_heartbeat(
        &self,
        _run: &str,
        _agent: &str,
        _turn: u32,
    ) -> Result<(), StoreError> {
        *self.heartbeats.lock().unwrap() += 1;
        Ok(())
    }
    async fn record_turn_complete(
        &self,
        _run: &str,
        _agent: &str,
        turn: u32,
    ) -> Result<(), StoreError> {
        self.completes.lock().unwrap().push(turn);
        Ok(())
    }
    async fn record_turn_usage(
        &self,
        _run: &str,
        _agent: &str,
        _turn: u32,
        _usage: &Usage,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    async fn record_turn_transcript(
        &self,
        _run: &str,
        _agent: &str,
        _turn: u32,
        _summary: &harness_protocol::transcript::TranscriptSummary,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    async fn get_turn(
        &self,
        _run: &str,
        _agent: &str,
        n: u32,
    ) -> Result<Option<TurnItem>, StoreError> {
        Ok(self.turn.lock().unwrap().clone().filter(|t| t.n == n))
    }
    async fn mark_turn_ended(&self, _run: &str, _agent: &str, n: u32) -> Result<(), StoreError> {
        if let Some(t) = self.turn.lock().unwrap().as_mut().filter(|t| t.n == n) {
            t.ended_at = Some("2026-11-16T14:03:08Z".into());
        }
        Ok(())
    }
    async fn take_controls(
        &self,
        _run: &str,
        _agent: &str,
    ) -> Result<Vec<PendingControl>, StoreError> {
        *self.controls_taken.lock().unwrap() += 1;
        Ok(std::mem::take(&mut *self.queued.lock().unwrap()))
    }
    async fn message_notices(
        &self,
        _run: &str,
    ) -> Result<Vec<harness_protocol::messages::MessageNotice>, message_ledger::LedgerError> {
        *self.notices_read.lock().unwrap() += 1;
        Ok(Vec::new())
    }
    async fn fence_agent(
        &self,
        _run: &str,
        _agent: &str,
        generation: u32,
    ) -> Result<bool, StoreError> {
        let mut g = self.agent.lock().unwrap();
        let Some(a) = g.as_mut() else {
            return Ok(false);
        };
        if a.session_generation != Some(generation) {
            return Ok(false);
        }
        let Some(creds) = a.creds.as_mut() else {
            return Ok(false);
        };
        creds.revoked = true;
        a.fenced_generation = Some(generation);
        a.version += 1;
        self.fences.lock().unwrap().push(generation);
        Ok(true)
    }
}

#[derive(Default)]
struct NoMint;

impl Minter for NoMint {
    async fn mint(
        &self,
        _plan: &MintPlan,
    ) -> Result<harness_protocol::api::internal::ProcessCredentials, MintError> {
        Err(MintError("no mint in this test".into()))
    }
}

#[derive(Default)]
struct Calls {
    done: Mutex<Vec<(TokenContext, String)>>,
    alive: Mutex<u32>,
    /// What `turn_done` answers, in order; with none left it succeeds.
    script: Mutex<Vec<CallbackError>>,
}

impl Callbacks for Calls {
    async fn turn_done(
        &self,
        _ciphertext: &str,
        ctx: &TokenContext,
        output: &str,
    ) -> Result<(), CallbackError> {
        let next = {
            let mut s = self.script.lock().unwrap();
            if s.is_empty() {
                None
            } else {
                Some(s.remove(0))
            }
        };
        if let Some(e) = next {
            return Err(e);
        }
        self.done
            .lock()
            .unwrap()
            .push((ctx.clone(), output.to_string()));
        Ok(())
    }
    async fn turn_alive(
        &self,
        _ciphertext: &str,
        _ctx: &TokenContext,
    ) -> Result<(), CallbackError> {
        *self.alive.lock().unwrap() += 1;
        Ok(())
    }
}

/// The Agent item of a run in session `generation`, with the completion key of [`NONCE`].
fn agent(generation: u32) -> AgentItem {
    serde_json::from_value(json!({
        "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:111122223333:runtime/hamh_direct_worker-AbCdEf",
        "runtimeSessionId": format!("{RUN}-direct-g{generation}"), "generation": generation, "engine": "cline", "engineVersion": "3.0.71",
        "transport": "acp", "modelId": "us.anthropic.claude-sonnet-5-5", "pool": "direct", "status": "running",
        "sessionGeneration": generation, "version": 4,
        "creds": {"nonceSha256": CredsBinding::hash_nonce(NONCE), "revoked": false, "tenant": "t", "mode": "s3-only", "sessionPolicy": "{}",
                  "completionKeySha256": CredsBinding::hash_completion_key(&key())}
    }))
    .unwrap()
}

fn open_turn(n: u32) -> TurnItem {
    serde_json::from_value(json!({
        "runId": RUN, "agentId": "direct", "n": n, "taskTokenCiphertext": "Y3Q=", "startedAt": "2026-11-16T14:00:00Z",
        "invokedAt": "2026-11-16T14:00:05Z", "rulesDigest": "", "version": 1
    }))
    .unwrap()
}

fn world(generation: u32) -> Deps<World, NoMint, Calls> {
    Deps {
        store: World {
            agent: Mutex::new(Some(agent(generation))),
            turn: Mutex::new(Some(open_turn(generation))),
            ..Default::default()
        },
        minter: NoMint,
        callbacks: Calls::default(),
        role_name: ROLE.into(),
        bucket: "hamh-direct-harness-111122223333".into(),
    }
}

fn post(route: &str, body: Value, with_key: Option<&str>) -> InternalRequest {
    InternalRequest {
        method: "POST".into(),
        path: format!("/internal/{route}"),
        caller_arn: Some(format!(
            "arn:aws:sts::111122223333:assumed-role/{ROLE}/{RUN}.direct"
        )),
        completion_key: with_key.map(str::to_string),
        body: body.to_string(),
    }
}

fn heartbeat(turn: u32) -> Value {
    json!({"runId": RUN, "agentId": "direct", "turn": turn})
}

fn ending(turn: u32) -> Value {
    json!({"runId": RUN, "agentId": "direct", "turn": turn, "ending": true})
}

fn batch() -> Value {
    let event = Envelope {
        schema_version: 1,
        event_id: "01M3B8X7YK000000000000001W".into(),
        run_id: RUN.into(),
        task_id: Some("root".into()),
        agent_id: Some("direct".into()),
        seq: 7,
        ts: "2026-11-16T14:03:07Z".into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline {
            value: json!({"text": "The session was asked to end, and a copy of the work could not be saved."}),
        },
    };
    json!({"runId": RUN, "agentId": "direct", "events": [event]})
}

fn turn_complete(turn: u32) -> Value {
    json!({"summary": "done", "runId": RUN, "agentId": "direct", "turn": turn, "finishReason": "end_turn", "outcome": "completed"})
}

fn code(resp: &InternalResponse) -> String {
    serde_json::from_str::<ApiError>(&resp.body)
        .map(|e| e.code)
        .unwrap_or_default()
}

fn fence(d: &Deps<World, NoMint, Calls>, generation: u32) {
    let mut g = d.store.agent.lock().unwrap();
    let a = g.as_mut().unwrap();
    a.fenced_generation = Some(generation);
    a.creds.as_mut().unwrap().revoked = true;
}

#[tokio::test]
async fn b854_a_fenced_generation_is_refused_on_every_route() {
    // Trigger: before the fence the routes of the session work.
    let d = world(2);
    d.store.queued.lock().unwrap().push(PendingControl::Cancel);
    assert_eq!(
        handle(post("heartbeat", heartbeat(2), None), &d)
            .await
            .status,
        200
    );
    let r = handle(post("events", batch(), Some(&key())), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let b: EventsBatchResponse = serde_json::from_str(&r.body).unwrap();
    assert_eq!(
        b.pending,
        [PendingControl::Cancel],
        "TRIGGER PRESENT: an unfenced session is handed its controls"
    );

    // Fenced: eight routes are refused, each with 409 stale-generation, before anything is recorded.
    let d = world(2);
    fence(&d, 2);
    d.store.queued.lock().unwrap().push(PendingControl::Cancel);
    let key = key();
    for (route, body, with_key) in [
        (
            "creds",
            json!({"runId": RUN, "agentId": "direct", "runtimeSessionId": format!("{RUN}-direct-g2"), "nonce": NONCE}),
            None,
        ),
        ("heartbeat", heartbeat(2), None),
        ("turn-complete", turn_complete(2), Some(key.as_str())),
        ("message-session", json!({}), None),
        ("message-claim", json!({}), None),
        ("message-seal", json!({}), None),
        ("message-outcome", json!({}), None),
        ("message-history", json!({}), None),
    ] {
        let r = handle(post(route, body, with_key), &d).await;
        assert_eq!(
            (r.status, code(&r).as_str()),
            (409, "stale-generation"),
            "{route}: {}",
            r.body
        );
        assert!(
            r.body
                .contains("this session is not the one the run is in now"),
            "{route}"
        );
    }
    assert_eq!(*d.store.heartbeats.lock().unwrap(), 0);
    assert!(d.store.completes.lock().unwrap().is_empty());
    assert!(
        d.callbacks.done.lock().unwrap().is_empty(),
        "a fenced session cannot end the wait with a result"
    );
    assert_eq!(
        *d.callbacks.alive.lock().unwrap(),
        0,
        "a fenced session cannot keep the wait alive"
    );

    // Its events are kept: stored, answered 200, with no control and no message notice, and no control is taken for it.
    let r = handle(post("events", batch(), Some(&key)), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let b: EventsBatchResponse = serde_json::from_str(&r.body).unwrap();
    assert_eq!(b.accepted, 1);
    assert!(b.pending.is_empty(), "{:?}", b.pending);
    assert_eq!(
        d.store.events.lock().unwrap().len(),
        1,
        "the worker's last sentences are stored"
    );
    assert_eq!(
        *d.store.controls_taken.lock().unwrap(),
        0,
        "the user's cancel stays queued for the session the run goes on in"
    );
    assert_eq!(*d.store.notices_read.lock().unwrap(), 0);
    assert_eq!(d.store.queued.lock().unwrap().len(), 1);
    // The completion key still guards the route: the fence never cleared it, and a request without the key is refused as before.
    let r = handle(post("events", batch(), None), &d).await;
    assert_eq!(r.status, 403, "{}", r.body);
    let a = d.store.agent.lock().unwrap().clone().unwrap();
    assert!(a.creds.as_ref().unwrap().completion_key_sha256.is_some());
    assert!(
        a.creds.as_ref().unwrap().revoked,
        "the fence revokes the credential refresh"
    );
}

#[tokio::test]
async fn b854_an_ending_heartbeat_with_the_key_fences_and_completes_the_task_with_session_ended() {
    let d = world(2);
    let (r, log) = handle_logged(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(r.status, 200, "{} {log:?}", r.body);
    // The fence, in the same step as the notice.
    let a = d.store.agent.lock().unwrap().clone().unwrap();
    assert_eq!(a.fenced_generation, Some(2));
    assert!(a.creds.as_ref().unwrap().revoked);
    assert_eq!(*d.store.fences.lock().unwrap(), [2]);
    // The wait ends with an output that is no result of the session.
    let done = d.callbacks.done.lock().unwrap().clone();
    assert_eq!(done.len(), 1);
    assert_eq!(
        done[0].0,
        TokenContext {
            run_id: RUN.into(),
            agent_id: "direct".into(),
            turn: 2
        }
    );
    let out: Value = serde_json::from_str(&done[0].1).unwrap();
    assert_eq!(out.as_object().unwrap().len(), 1, "{out}");
    let ended = &out["sessionEnded"];
    assert_eq!(ended["session"], 2);
    assert_eq!(ended["cause"], "end-requested");
    assert!(is_utc_timestamp(ended["at"].as_str().unwrap()), "{ended}");
    assert!(
        out.get("runId").is_none(),
        "the machine's gate reads runId: a known end has none"
    );
    // The turn is ended, and the notice is no sign of life.
    assert!(
        d.store
            .turn
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .ended_at
            .is_some()
    );
    assert_eq!(*d.callbacks.alive.lock().unwrap(), 0);
    assert_eq!(*d.store.heartbeats.lock().unwrap(), 0);
}

#[tokio::test]
async fn b854_an_ending_heartbeat_without_the_key_is_a_plain_heartbeat() {
    // No key, and a key that is not the run's: a plain heartbeat, recorded as one, with one log line; nothing is fenced or completed.
    for presented in [None, Some("00".repeat(32))] {
        let d = world(2);
        let (r, log) = handle_logged(post("heartbeat", ending(2), presented.as_deref()), &d).await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(*d.store.heartbeats.lock().unwrap(), 1);
        assert_eq!(*d.callbacks.alive.lock().unwrap(), 1);
        assert!(d.callbacks.done.lock().unwrap().is_empty());
        assert!(d.store.fences.lock().unwrap().is_empty());
        assert_eq!(
            d.store
                .agent
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .fenced_generation,
            None
        );
        assert_eq!(
            log.iter().filter(|l| l.contains("ending")).count(),
            1,
            "{log:?}"
        );
    }
    // An Agent item that names no key at all (written before the key existed) accepts every request on the keyed routes. That is no proof
    // of the key: the notice is a plain heartbeat there too.
    let d = world(2);
    d.store
        .agent
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .creds
        .as_mut()
        .unwrap()
        .completion_key_sha256 = None;
    let r = handle(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(r.status, 200);
    assert!(d.callbacks.done.lock().unwrap().is_empty());
    assert!(d.store.fences.lock().unwrap().is_empty());
    assert_eq!(*d.store.heartbeats.lock().unwrap(), 1);
    // Trigger: the same request with the run's key on an item that names it does end the wait.
    let d = world(2);
    handle(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(d.callbacks.done.lock().unwrap().len(), 1, "TRIGGER PRESENT");
}

#[tokio::test]
async fn b854_a_run_that_switched_the_read_off_takes_an_ending_heartbeat_as_a_plain_one() {
    let d = world(2);
    d.store
        .agent
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .recover_after_stop = Some(false);
    let r = handle(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        d.callbacks.done.lock().unwrap().is_empty(),
        "off: the wait is not ended early; the 600 seconds decide"
    );
    assert!(d.store.fences.lock().unwrap().is_empty());
    assert_eq!(
        d.store
            .agent
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .fenced_generation,
        None
    );
    assert_eq!(*d.store.heartbeats.lock().unwrap(), 1);
    assert_eq!(*d.callbacks.alive.lock().unwrap(), 1);
    // On (absent, or true): the notice acts.
    for on in [None, Some(true)] {
        let d = world(2);
        d.store
            .agent
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .recover_after_stop = on;
        handle(post("heartbeat", ending(2), Some(&key())), &d).await;
        assert_eq!(d.callbacks.done.lock().unwrap().len(), 1, "{on:?}");
    }
}

/// A session on Instances compute has no storage of its own to read back, so nothing waits for the stopped machine's last copy after
/// a notice. The notice therefore changes nothing for such a run: the wait for the session ends as it did before, after the silence.
#[tokio::test]
async fn b854_a_run_on_instances_compute_takes_an_ending_heartbeat_as_a_plain_one() {
    let d = world(2);
    d.store.agent.lock().unwrap().as_mut().unwrap().compute = Some("instances".into());
    let r = handle(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        d.callbacks.done.lock().unwrap().is_empty(),
        "instances: the wait is not ended early; the 600 seconds decide"
    );
    assert!(d.store.fences.lock().unwrap().is_empty());
    assert_eq!(
        d.store
            .agent
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .fenced_generation,
        None
    );
    assert_eq!(*d.store.heartbeats.lock().unwrap(), 1);
    assert_eq!(*d.callbacks.alive.lock().unwrap(), 1);
    // Trigger: the same notice of a session on the microVM (named, or absent on an item written before) does end the wait.
    for compute in [None, Some("microvm")] {
        let d = world(2);
        d.store.agent.lock().unwrap().as_mut().unwrap().compute = compute.map(str::to_string);
        handle(post("heartbeat", ending(2), Some(&key())), &d).await;
        assert_eq!(
            d.callbacks.done.lock().unwrap().len(),
            1,
            "TRIGGER PRESENT {compute:?}"
        );
    }
}

#[tokio::test]
async fn b854_an_ending_heartbeat_after_a_turn_complete_changes_nothing() {
    let d = world(2);
    let r = handle(post("turn-complete", turn_complete(2), Some(&key())), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let result = d.callbacks.done.lock().unwrap().clone();
    assert_eq!(result.len(), 1);
    assert!(
        result[0].1.contains("\"runId\""),
        "the session ended with its result"
    );
    // The notice after it: 200, and neither a second output nor a fence.
    let r = handle(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(d.callbacks.done.lock().unwrap().clone(), result);
    assert!(d.store.fences.lock().unwrap().is_empty());
    assert_eq!(
        d.store
            .agent
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .fenced_generation,
        None
    );
}

#[tokio::test]
async fn b854_a_repeated_ending_heartbeat_is_harmless() {
    // A repeat after a notice that worked: the turn is ended, so nothing happens again.
    let d = world(2);
    assert_eq!(
        handle(post("heartbeat", ending(2), Some(&key())), &d)
            .await
            .status,
        200
    );
    let r = handle(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(d.callbacks.done.lock().unwrap().len(), 1);

    // The callback is unavailable after the fence: 503, and the fence stands. The repeat comes from a fenced generation and still gets
    // its callback tried again, which ends the wait.
    let d = world(2);
    d.callbacks
        .script
        .lock()
        .unwrap()
        .push(CallbackError::Unavailable("ThrottlingException".into()));
    let r = handle(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(
        (r.status, code(&r).as_str()),
        (503, "callback_unavailable"),
        "{}",
        r.body
    );
    assert_eq!(
        d.store
            .agent
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .fenced_generation,
        Some(2)
    );
    assert!(d.callbacks.done.lock().unwrap().is_empty());
    assert!(
        d.store
            .turn
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .ended_at
            .is_none(),
        "the turn stays open for the repeat"
    );
    let r = handle(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        d.callbacks.done.lock().unwrap().len(),
        1,
        "the repeat from the fenced generation ended the wait"
    );
    // A plain heartbeat of that fenced generation is refused all the same.
    let r = handle(post("heartbeat", heartbeat(2), None), &d).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "stale-generation"));

    // The task is closed (a turn-complete won the race, or the state timed out): logged, answered 200.
    let d = world(2);
    d.callbacks
        .script
        .lock()
        .unwrap()
        .push(CallbackError::Closed("TaskTimedOut".into()));
    let (r, log) = handle_logged(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(log.iter().any(|l| l.contains("task closed")), "{log:?}");
}

#[tokio::test]
async fn b854_an_ending_heartbeat_of_an_earlier_generation_is_stale() {
    // The run is in session 3; a notice of session 2 arrives late.
    let d = world(3);
    let r = handle(post("heartbeat", ending(2), Some(&key())), &d).await;
    assert_eq!(
        (r.status, code(&r).as_str()),
        (409, "stale-generation"),
        "{}",
        r.body
    );
    assert!(d.store.fences.lock().unwrap().is_empty());
    assert_eq!(
        d.store
            .agent
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .fenced_generation,
        None
    );
    assert!(
        d.callbacks.done.lock().unwrap().is_empty(),
        "the live session's wait is not ended by another session's notice"
    );
    assert!(
        d.store
            .turn
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .ended_at
            .is_none()
    );
}

#[tokio::test]
async fn b854_the_next_session_is_not_fenced() {
    // Generation 2 is fenced: its heartbeat is refused.
    let d = world(2);
    fence(&d, 2);
    let r = handle(post("heartbeat", heartbeat(2), None), &d).await;
    assert_eq!(
        (r.status, code(&r).as_str()),
        (409, "stale-generation"),
        "TRIGGER PRESENT: the fenced generation is refused"
    );
    // start-session moves the item to generation 3 and keeps the member: the fence names 2, the run is in 3.
    {
        let mut g = d.store.agent.lock().unwrap();
        let a = g.as_mut().unwrap();
        a.session_generation = Some(3);
        a.generation = 3;
        a.creds.as_mut().unwrap().revoked = false;
        assert_eq!(a.fenced_generation, Some(2));
    }
    *d.store.turn.lock().unwrap() = Some(open_turn(3));
    d.store.queued.lock().unwrap().push(PendingControl::Cancel);
    assert_eq!(
        handle(post("heartbeat", heartbeat(3), None), &d)
            .await
            .status,
        200
    );
    assert_eq!(*d.store.heartbeats.lock().unwrap(), 1);
    let r = handle(post("events", batch(), Some(&key())), &d).await;
    let b: EventsBatchResponse = serde_json::from_str(&r.body).unwrap();
    assert_eq!(
        b.pending,
        [PendingControl::Cancel],
        "the live session is handed its controls"
    );
    let r = handle(post("turn-complete", turn_complete(3), Some(&key())), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(d.callbacks.done.lock().unwrap().len(), 1);
    // And the old session's late calls are stale by the generation, as before.
    let r = handle(post("heartbeat", heartbeat(2), None), &d).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "stale-generation"));
}
