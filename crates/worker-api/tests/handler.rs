//! The /internal handler with fake store and STS minter: routing, re-mint with nonce rotation,
//! reuse and race handling, events, heartbeat and turn-complete. No AWS calls.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use harness_protocol::api::ApiError;
use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::api::internal::{
    CredsRequest, CredsResponse, EventsBatchRequest, EventsBatchResponse, PendingControl,
    ProcessCredentials,
};
use harness_protocol::common::{Engine, Transport, Usage};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::{AgentItem, AgentStatus, CredsBinding, TurnItem};
use std::sync::Mutex;
use worker_api::creds::MintPlan;
use worker_api::handler::{
    CallbackError, Callbacks, Deps, InternalRequest, MintError, Minter, Store, StoreError,
    TokenContext, handle, handle_logged,
};

const ROLE: &str = "hamh-live-harness-worker-session";
const SESSION: &str = "R1-a1-session-000000000000000000000001";

#[derive(Default)]
struct FakeStore {
    agent: Mutex<Option<AgentItem>>,
    goal: Option<GoalStatus>,
    force_conflict: bool,
    events: Mutex<Vec<Envelope>>,
    heartbeats: Mutex<u32>,
    completes: Mutex<Vec<u32>>,
    /// (turn, usage) recorded on Turn items.
    usages: Mutex<Vec<(u32, Usage)>>,
    fail_usage: bool,
    /// (turn, summary) recorded on Turn items by the transcript writer's turn-complete.
    transcripts: Mutex<Vec<(u32, harness_protocol::transcript::TranscriptSummary)>>,
    fail_transcript: bool,
    /// (agent, control, delivered)
    controls: Mutex<Vec<(String, PendingControl, bool)>>,
    fail_controls: bool,
    /// The open turn's item (a Step Functions dispatch), if any.
    turn: Mutex<Option<TurnItem>>,
}

impl Store for FakeStore {
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
            t.ended_at = Some("2026-09-25T21:00:00Z".into());
        }
        Ok(())
    }
    async fn get_agent(&self, _run: &str, _agent: &str) -> Result<Option<AgentItem>, StoreError> {
        Ok(self.agent.lock().unwrap().clone())
    }
    async fn get_goal_status(&self, _run: &str) -> Result<Option<GoalStatus>, StoreError> {
        Ok(self.goal)
    }
    async fn rotate_nonce(
        &self,
        _run: &str,
        _agent: &str,
        old: &str,
        new: &str,
    ) -> Result<(), StoreError> {
        if self.force_conflict {
            return Err(StoreError::Conflict);
        }
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
        turn: u32,
        usage: &Usage,
    ) -> Result<(), StoreError> {
        if self.fail_usage {
            return Err(StoreError::Unavailable("turn item throttled".into()));
        }
        self.usages.lock().unwrap().push((turn, *usage));
        Ok(())
    }
    async fn record_turn_transcript(
        &self,
        _run: &str,
        _agent: &str,
        turn: u32,
        summary: &harness_protocol::transcript::TranscriptSummary,
    ) -> Result<(), StoreError> {
        if self.fail_transcript {
            return Err(StoreError::Unavailable("turn item throttled".into()));
        }
        self.transcripts
            .lock()
            .unwrap()
            .push((turn, summary.clone()));
        Ok(())
    }
    async fn take_controls(
        &self,
        _run: &str,
        agent: &str,
    ) -> Result<Vec<PendingControl>, StoreError> {
        if self.fail_controls {
            return Err(StoreError::Unavailable("down".into()));
        }
        let mut g = self.controls.lock().unwrap();
        Ok(g.iter_mut()
            .filter(|(a, _, delivered)| a == agent && !*delivered)
            .map(|(_, c, delivered)| {
                *delivered = true;
                c.clone()
            })
            .collect())
    }
}

/// The key ID the fake minter returns: the ASIA prefix and a made-up rest, joined here so no committed line holds a key-ID-shaped literal.
fn minted_key_id() -> String {
    ["ASIA", "MINTED0000000001"].concat()
}

#[derive(Default)]
struct FakeMinter {
    plans: Mutex<Vec<MintPlan>>,
}

impl Minter for FakeMinter {
    async fn mint(&self, plan: &MintPlan) -> Result<ProcessCredentials, MintError> {
        self.plans.lock().unwrap().push(plan.clone());
        Ok(ProcessCredentials {
            version: 1,
            access_key_id: minted_key_id(),
            secret_access_key: "minted-secret".into(),
            session_token: "minted-token".into(),
            expiration: "2026-09-25T06:00:00Z".into(),
        })
    }
}

/// Step Functions and KMS as the handler uses them: (ciphertext, context, output) per call.
#[derive(Default)]
struct FakeCallbacks {
    done: Mutex<Vec<(String, TokenContext, String)>>,
    alive: Mutex<Vec<(String, TokenContext)>>,
    /// Answer every call with this error instead.
    fail: Option<CallbackError>,
}

impl Callbacks for FakeCallbacks {
    async fn turn_done(
        &self,
        ciphertext: &str,
        ctx: &TokenContext,
        output: &str,
    ) -> Result<(), CallbackError> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        self.done
            .lock()
            .unwrap()
            .push((ciphertext.into(), ctx.clone(), output.into()));
        Ok(())
    }
    async fn turn_alive(&self, ciphertext: &str, ctx: &TokenContext) -> Result<(), CallbackError> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        self.alive
            .lock()
            .unwrap()
            .push((ciphertext.into(), ctx.clone()));
        Ok(())
    }
}

fn open_turn(n: u32, ciphertext: &str) -> TurnItem {
    TurnItem {
        run_id: "R1".into(),
        agent_id: "a1".into(),
        n,
        task_token_ciphertext: ciphertext.into(),
        started_at: "2026-09-25T20:00:00Z".into(),
        invoked_at: None,
        ended_at: None,
        last_commit_sha: None,
        bundle_uri: None,
        snapshot_uri: None,
        gate_result_uri: None,
        turn_class: None,
        fingerprint: None,
        usage: None,
        rules_digest: String::new(),
        role: None,
        rules_manifest: None,
        version: 1,
        session_generation: None,
        runtime_session_id: None,
        runtime_version: None,
        runtime_endpoint: None,
        base_sha: None,
        resume: None,
        resume_reason: None,
        resume_head: None,
        engine: None,
        model_id: None,
        lost_at: None,
        attempt: None,
        ladder_version: None,
        max_silent_gap_s: None,
        max_output_tokens: None,
        region: None,
        route_level: None,
        context_tokens: None,
        transport: None,
        engine_policy_version: None,
        started_at_ms: None,
    }
}

fn agent(nonce: &str) -> AgentItem {
    AgentItem {
        run_id: "R1".into(),
        agent_id: "a1".into(),
        task_id: "t1".into(),
        attempt: 1,
        runtime_arn: "arn".into(),
        runtime_session_id: SESSION.into(),
        generation: 1,
        engine: Engine::OPENCODE,
        engine_version: "1.18.32".into(),
        transport: Transport::Acp,
        model_id: "m".into(),
        pool: "p".into(),
        status: AgentStatus::Running,
        heartbeat_at: None,
        creds: Some(CredsBinding {
            nonce_sha256: CredsBinding::hash_nonce(nonce),
            revoked: false,
            tenant: "t".into(),
            mode: "repo".into(),
            session_policy: "{}".into(),
            completion_key_sha256: None,
        }),
        version: 1,
        session_generation: None,
        generation_started_at: None,
        runtime_version: None,
        runtime_endpoint: None,
        engine_policy_version: None,
        harness_controls: vec![],
        features: vec![],
        capacity_provider_id: None,
        nonce_ciphertext: None,
        compute: None,
    }
}

fn deps(store: FakeStore) -> Deps<FakeStore, FakeMinter, FakeCallbacks> {
    deps_with(store, FakeCallbacks::default())
}

fn deps_with(
    store: FakeStore,
    callbacks: FakeCallbacks,
) -> Deps<FakeStore, FakeMinter, FakeCallbacks> {
    Deps {
        store,
        minter: FakeMinter::default(),
        callbacks,
        role_name: ROLE.into(),
        bucket: "b".into(),
    }
}

fn caller_arn() -> Option<String> {
    Some(format!(
        "arn:aws:sts::800000000022:assumed-role/{ROLE}/R1.a1"
    ))
}

fn creds_body(nonce: &str) -> String {
    serde_json::to_string(&CredsRequest {
        run_id: "R1".into(),
        agent_id: "a1".into(),
        runtime_session_id: SESSION.into(),
        nonce: nonce.into(),
    })
    .unwrap()
}

fn post(path: &str, body: String) -> InternalRequest {
    InternalRequest {
        method: "POST".into(),
        path: path.into(),
        caller_arn: caller_arn(),
        completion_key: None,
        body,
    }
}

#[tokio::test]
async fn creds_re_mints_and_rotates_the_nonce() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        ..Default::default()
    });
    let resp = handle(post("/internal/creds", creds_body("n0")), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body: CredsResponse = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(body.credentials.access_key_id, minted_key_id());
    assert_ne!(body.next_nonce, "n0");
    let stored = d
        .store
        .agent
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .creds
        .unwrap()
        .nonce_sha256;
    assert_eq!(stored, CredsBinding::hash_nonce(&body.next_nonce));
    let plans = d.minter.plans.lock().unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].role_session_name, "R1.a1");
    assert!(
        plans[0]
            .tags
            .contains(&("engine".into(), "opencode".into()))
    );
}

#[tokio::test]
async fn a_used_nonce_is_denied_after_rotation() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        ..Default::default()
    });
    assert_eq!(
        handle(post("/internal/creds", creds_body("n0")), &d)
            .await
            .status,
        200
    );
    let again = handle(post("/internal/creds", creds_body("n0")), &d).await;
    assert_eq!(again.status, 403);
    let err: ApiError = serde_json::from_str(&again.body).unwrap();
    assert_eq!(err.code, "bad_nonce");
    assert!(!again.body.contains("n0\""), "the nonce is never echoed");
    assert_eq!(d.minter.plans.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_lost_rotation_race_is_a_conflict_and_mints_nothing() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        force_conflict: true,
        ..Default::default()
    });
    let resp = handle(post("/internal/creds", creds_body("n0")), &d).await;
    assert_eq!(resp.status, 409);
    assert!(d.minter.plans.lock().unwrap().is_empty());
}

#[tokio::test]
async fn denials_are_403_without_secrets() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Blocked),
        ..Default::default()
    });
    let resp = handle(post("/internal/creds", creds_body("n0")), &d).await;
    assert_eq!(resp.status, 403);
    let err: ApiError = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(err.code, "goal_inactive");
    for leak in ["n0", "minted-secret", "minted-token"] {
        assert!(!resp.body.contains(leak));
    }
}

#[tokio::test]
async fn missing_or_foreign_callers_are_rejected() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        ..Default::default()
    });
    let mut r = post("/internal/creds", creds_body("n0"));
    r.caller_arn = None;
    assert_eq!(handle(r, &d).await.status, 403);
    let mut r = post("/internal/creds", creds_body("n0"));
    r.caller_arn = Some("arn:aws:sts::800000000022:assumed-role/Admin/testuser".into());
    assert_eq!(handle(r, &d).await.status, 403);
}

#[tokio::test]
async fn bad_json_is_400() {
    let d = deps(FakeStore::default());
    assert_eq!(
        handle(post("/internal/creds", "{not json".into()), &d)
            .await
            .status,
        400
    );
}

#[tokio::test]
async fn routing_is_exact_and_stage_prefixes_are_accepted() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        ..Default::default()
    });
    assert_eq!(
        handle(post("/internal/nope", "{}".into()), &d).await.status,
        404
    );
    let mut get = post("/internal/creds", creds_body("n0"));
    get.method = "GET".into();
    assert_eq!(handle(get, &d).await.status, 405);
    assert_eq!(
        handle(post("/live/internal/creds", creds_body("n0")), &d)
            .await
            .status,
        200
    );
}

fn event(authority: Authority, t: EventType, agent_id: &str) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: "01M3B8X7YK000000000000001W".into(),
        run_id: "R1".into(),
        task_id: Some("t1".into()),
        agent_id: Some(agent_id.into()),
        seq: 1,
        ts: "2026-09-25T04:00:00Z".into(),
        event_type: t,
        authority,
        payload: Payload::Inline {
            value: serde_json::json!({"text": "hi"}),
        },
    }
}

#[tokio::test]
async fn events_from_the_caller_are_stored() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        ..Default::default()
    });
    let batch = EventsBatchRequest {
        run_id: "R1".into(),
        agent_id: "a1".into(),
        events: vec![event(Authority::Agent, EventType::TurnProgress, "a1")],
        take_controls: None,
    };
    let resp = handle(
        post("/internal/events", serde_json::to_string(&batch).unwrap()),
        &d,
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body: EventsBatchResponse = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(body.accepted, 1);
    assert_eq!(d.store.events.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn events_for_another_agent_or_with_harness_authority_are_refused() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        ..Default::default()
    });
    let other = EventsBatchRequest {
        run_id: "R1".into(),
        agent_id: "a2".into(),
        events: vec![event(Authority::Agent, EventType::TurnProgress, "a2")],
        take_controls: None,
    };
    assert_eq!(
        handle(
            post("/internal/events", serde_json::to_string(&other).unwrap()),
            &d
        )
        .await
        .status,
        403
    );
    let claim = EventsBatchRequest {
        run_id: "R1".into(),
        agent_id: "a1".into(),
        events: vec![event(Authority::Harness, EventType::GoalStatus, "a1")],
        take_controls: None,
    };
    let resp = handle(
        post("/internal/events", serde_json::to_string(&claim).unwrap()),
        &d,
    )
    .await;
    assert_eq!(resp.status, 400);
    assert!(d.store.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn heartbeat_and_turn_complete_are_recorded_for_the_caller() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        ..Default::default()
    });
    let hb = serde_json::json!({"runId": "R1", "agentId": "a1", "turn": 2});
    assert_eq!(
        handle(post("/internal/heartbeat", hb.to_string()), &d)
            .await
            .status,
        200
    );
    assert_eq!(*d.store.heartbeats.lock().unwrap(), 1);
    let tc = serde_json::json!({"runId": "R1", "agentId": "a1", "turn": 2, "finishReason": "end_turn",
        "usage": {"inputTokens": 1, "outputTokens": 1, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0}});
    let resp = handle(post("/internal/turn-complete", tc.to_string()), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(*d.store.completes.lock().unwrap(), vec![2]);
    // The engine-reported usage lands on the Turn item.
    assert_eq!(
        *d.store.usages.lock().unwrap(),
        vec![(
            2,
            Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0
            }
        )]
    );
    let foreign = serde_json::json!({"runId": "R9", "agentId": "a1", "turn": 2});
    assert_eq!(
        handle(post("/internal/heartbeat", foreign.to_string()), &d)
            .await
            .status,
        403
    );
}

fn queued(controls: Vec<(&str, PendingControl)>) -> Mutex<Vec<(String, PendingControl, bool)>> {
    Mutex::new(
        controls
            .into_iter()
            .map(|(a, c)| (a.to_string(), c, false))
            .collect(),
    )
}

async fn post_events(
    d: &Deps<FakeStore, FakeMinter, FakeCallbacks>,
    events: Vec<Envelope>,
) -> EventsBatchResponse {
    post_batch(d, events, None).await
}

async fn post_batch(
    d: &Deps<FakeStore, FakeMinter, FakeCallbacks>,
    events: Vec<Envelope>,
    take_controls: Option<bool>,
) -> EventsBatchResponse {
    let batch = EventsBatchRequest {
        run_id: "R1".into(),
        agent_id: "a1".into(),
        events,
        take_controls,
    };
    let resp = handle(
        post("/internal/events", serde_json::to_string(&batch).unwrap()),
        d,
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    serde_json::from_str(&resp.body).unwrap()
}

#[tokio::test]
async fn pending_controls_ride_the_events_response_once() {
    let steer = PendingControl::Steer {
        text: "use pytest".into(),
        now: true,
    };
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        controls: queued(vec![
            ("a1", steer.clone()),
            ("a2", PendingControl::Cancel),
            ("a1", PendingControl::Cancel),
        ]),
        ..Default::default()
    });
    let first = post_events(
        &d,
        vec![event(Authority::Agent, EventType::TurnProgress, "a1")],
    )
    .await;
    assert_eq!(first.accepted, 1);
    assert_eq!(first.pending, vec![steer, PendingControl::Cancel]);
    let second = post_events(
        &d,
        vec![event(Authority::Agent, EventType::TurnProgress, "a1")],
    )
    .await;
    assert!(second.pending.is_empty(), "{:?}", second.pending);
    // Another agent's control stays queued for that agent.
    assert!(!d.store.controls.lock().unwrap()[1].2);
}

#[tokio::test]
async fn an_empty_batch_polls_for_controls() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        controls: queued(vec![("a1", PendingControl::Cancel)]),
        ..Default::default()
    });
    let resp = post_events(&d, vec![]).await;
    assert_eq!(resp.accepted, 0);
    assert_eq!(resp.pending, vec![PendingControl::Cancel]);
    assert!(d.store.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_control_store_failure_is_reported_in_the_log() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        fail_controls: true,
        ..Default::default()
    });
    let batch = EventsBatchRequest {
        run_id: "R1".into(),
        agent_id: "a1".into(),
        events: vec![],
        take_controls: None,
    };
    let (resp, lines) = worker_api::handler::handle_logged(
        post("/internal/events", serde_json::to_string(&batch).unwrap()),
        &d,
    )
    .await;
    assert_eq!(resp.status, 200);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("controls for R1/a1 not read") && l.contains("down")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn a_control_store_failure_still_accepts_the_events() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        fail_controls: true,
        ..Default::default()
    });
    let resp = post_events(
        &d,
        vec![event(Authority::Agent, EventType::TurnProgress, "a1")],
    )
    .await;
    assert_eq!(resp.accepted, 1);
    assert!(resp.pending.is_empty());
    assert_eq!(d.store.events.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_final_flush_leaves_controls_for_the_next_turn() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        controls: queued(vec![("a1", PendingControl::Cancel)]),
        ..Default::default()
    });
    let last = post_batch(
        &d,
        vec![event(Authority::Agent, EventType::TurnEnded, "a1")],
        Some(false),
    )
    .await;
    assert_eq!(last.accepted, 1);
    assert!(last.pending.is_empty());
    assert!(!d.store.controls.lock().unwrap()[0].2, "still queued");
    let next = post_batch(&d, vec![], Some(true)).await;
    assert_eq!(next.pending, vec![PendingControl::Cancel]);
}

fn turn_complete_body(turn: u32) -> String {
    serde_json::json!({"runId": "R1", "agentId": "a1", "turn": turn, "finishReason": "end_turn",
        "lastCommitSha": "c".repeat(40), "bundleUri": "s3://b/runs/R1/agents/a1/turns/2/bundle",
        "usage": null, "outcome": "completed"})
    .to_string()
}

#[tokio::test]
async fn a_turn_without_usage_records_none_and_a_failed_usage_write_is_a_503() {
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        ..Default::default()
    });
    let resp = handle(post("/internal/turn-complete", turn_complete_body(2)), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert!(d.store.usages.lock().unwrap().is_empty());
    let d = deps(FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        fail_usage: true,
        ..Default::default()
    });
    let tc = serde_json::json!({"runId": "R1", "agentId": "a1", "turn": 2, "finishReason": "end_turn",
        "usage": {"inputTokens": 5, "outputTokens": 1, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0}});
    let resp = handle(post("/internal/turn-complete", tc.to_string()), &d).await;
    assert_eq!(resp.status, 503, "{}", resp.body);
}

fn store_with_agent() -> FakeStore {
    FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_transcript_summary_of_a_turn_complete_lands_on_the_turn_item() {
    let d = deps(store_with_agent());
    let tc = serde_json::json!({"runId": "R1", "agentId": "a1", "turn": 1, "finishReason": "end_turn",
        "transcript": {"lastTranscriptSeq": 7, "entries": [{"kind": "failed", "count": 3}, {"kind": "conflict", "fromSeq": 2, "toSeq": 2, "count": 1}]}});
    let resp = handle(post("/internal/turn-complete", tc.to_string()), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let got = d.store.transcripts.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, 1);
    assert_eq!(got[0].1.last_transcript_seq, Some(7));
    assert_eq!(got[0].1.entries.len(), 2);
}

#[tokio::test]
async fn a_turn_without_a_summary_records_none() {
    let d = deps(store_with_agent());
    let tc =
        serde_json::json!({"runId": "R1", "agentId": "a1", "turn": 1, "finishReason": "end_turn"});
    let resp = handle(post("/internal/turn-complete", tc.to_string()), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(
        *d.store.completes.lock().unwrap(),
        vec![1],
        "the turn is reported"
    );
    assert!(d.store.transcripts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_summary_that_is_not_well_formed_is_not_stored_and_the_turn_still_completes() {
    let d = deps(store_with_agent());
    let tc = serde_json::json!({"runId": "R1", "agentId": "a1", "turn": 1, "finishReason": "end_turn",
        "transcript": {"lastTranscriptSeq": 1, "entries": [{"kind": "shed", "fromSeq": 5, "toSeq": 2}]}});
    let resp = handle(post("/internal/turn-complete", tc.to_string()), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert!(
        d.store.transcripts.lock().unwrap().is_empty(),
        "a range that runs backwards is not kept"
    );
    assert_eq!(
        *d.store.completes.lock().unwrap(),
        vec![1],
        "the turn is still reported"
    );
}

#[tokio::test]
async fn a_failed_summary_write_never_fails_the_turn() {
    let d = deps(FakeStore {
        fail_transcript: true,
        ..store_with_agent()
    });
    let tc = serde_json::json!({"runId": "R1", "agentId": "a1", "turn": 1, "finishReason": "end_turn",
        "transcript": {"lastTranscriptSeq": 1, "entries": []}});
    let resp = handle(post("/internal/turn-complete", tc.to_string()), &d).await;
    assert_eq!(resp.status, 200, "the summary is advisory: {}", resp.body);
    assert_eq!(*d.store.completes.lock().unwrap(), vec![1]);
}

fn with_turn(t: TurnItem) -> FakeStore {
    FakeStore {
        agent: Mutex::new(Some(agent("n0"))),
        goal: Some(GoalStatus::Active),
        turn: Mutex::new(Some(t)),
        ..Default::default()
    }
}

#[tokio::test]
async fn turn_complete_reports_the_open_turn_to_step_functions_once() {
    // worker-api matches the caller's <run>.<agent> to the open turn and calls
    // SendTaskSuccess; the output is the request, which carries no verdict.
    let d = deps(with_turn(open_turn(2, "ct-1")));
    let resp = handle(post("/internal/turn-complete", turn_complete_body(2)), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let done = d.callbacks.done.lock().unwrap().clone();
    assert_eq!(done.len(), 1);
    let (ct, ctx, output) = &done[0];
    assert_eq!(ct, "ct-1");
    assert_eq!(
        ctx,
        &TokenContext {
            run_id: "R1".into(),
            agent_id: "a1".into(),
            turn: 2
        }
    );
    let sent: TurnCompleteRequest = serde_json::from_str(output).unwrap();
    assert_eq!(
        (sent.run_id.as_str(), sent.agent_id.as_str(), sent.turn),
        ("R1", "a1", 2)
    );
    assert_eq!(
        sent.last_commit_sha.as_deref(),
        Some("c".repeat(40).as_str())
    );
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
    // agentd may repeat the call; the turn is closed and nothing is sent again.
    let again = handle(post("/internal/turn-complete", turn_complete_body(2)), &d).await;
    assert_eq!(again.status, 200);
    assert_eq!(d.callbacks.done.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_turn_without_a_task_token_is_only_recorded() {
    // The k-suite dispatch path has no Step Functions state: no Turn item, or one with no token.
    for store in [
        FakeStore {
            agent: Mutex::new(Some(agent("n0"))),
            goal: Some(GoalStatus::Active),
            ..Default::default()
        },
        with_turn(open_turn(2, "")),
        with_turn(open_turn(3, "ct-3")),
    ] {
        let d = deps(store);
        let resp = handle(post("/internal/turn-complete", turn_complete_body(2)), &d).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(*d.store.completes.lock().unwrap(), vec![2]);
        assert!(d.callbacks.done.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_closed_task_is_accepted_and_logged_and_an_outage_leaves_the_turn_open() {
    let closed = FakeCallbacks {
        fail: Some(CallbackError::Closed("TaskTimedOut".into())),
        ..Default::default()
    };
    let d = deps_with(with_turn(open_turn(2, "ct-1")), closed);
    let (resp, log) =
        handle_logged(post("/internal/turn-complete", turn_complete_body(2)), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert!(log.iter().any(|l| l.contains("TaskTimedOut")), "{log:?}");
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

    let down = FakeCallbacks {
        fail: Some(CallbackError::Unavailable("throttled".into())),
        ..Default::default()
    };
    let d = deps_with(with_turn(open_turn(2, "ct-1")), down);
    let (resp, log) =
        handle_logged(post("/internal/turn-complete", turn_complete_body(2)), &d).await;
    assert_eq!(resp.status, 503, "{}", resp.body);
    assert!(!resp.body.contains("throttled"));
    assert!(log.iter().any(|l| l.contains("throttled")), "{log:?}");
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
async fn a_heartbeat_extends_the_open_turn_only() {
    let hb =
        |turn: u32| serde_json::json!({"runId": "R1", "agentId": "a1", "turn": turn}).to_string();
    let d = deps(with_turn(open_turn(2, "ct-1")));
    assert_eq!(
        handle(post("/internal/heartbeat", hb(2)), &d).await.status,
        200
    );
    let alive = d.callbacks.alive.lock().unwrap().clone();
    assert_eq!(
        alive,
        vec![(
            "ct-1".to_string(),
            TokenContext {
                run_id: "R1".into(),
                agent_id: "a1".into(),
                turn: 2
            }
        )]
    );
    // Another turn number, or an ended turn, sends nothing.
    assert_eq!(
        handle(post("/internal/heartbeat", hb(3)), &d).await.status,
        200
    );
    d.store.turn.lock().unwrap().as_mut().unwrap().ended_at = Some("x".into());
    assert_eq!(
        handle(post("/internal/heartbeat", hb(2)), &d).await.status,
        200
    );
    assert_eq!(d.callbacks.alive.lock().unwrap().len(), 1);
    // A closed task still answers the heartbeat and logs why.
    let closed = FakeCallbacks {
        fail: Some(CallbackError::Closed("TaskDoesNotExist".into())),
        ..Default::default()
    };
    let d = deps_with(with_turn(open_turn(2, "ct-1")), closed);
    let (resp, log) = handle_logged(post("/internal/heartbeat", hb(2)), &d).await;
    assert_eq!(resp.status, 200);
    assert!(
        log.iter().any(|l| l.contains("TaskDoesNotExist")),
        "{log:?}"
    );
}

// ---- the output locations of a turn-complete (sweep SW-67) ----------------------------------------------

/// A turn-complete of the caller R1.a1 for turn 2 that names `bundle` (and `snapshot`) as its output.
fn turn_complete_naming(bundle: Option<&str>, snapshot: Option<&str>) -> String {
    let mut v = serde_json::json!({"runId": "R1", "agentId": "a1", "turn": 2, "finishReason": "end_turn",
        "lastCommitSha": "c".repeat(40), "usage": null, "outcome": "completed"});
    if let Some(b) = bundle {
        v["bundleUri"] = b.into();
    }
    if let Some(s) = snapshot {
        v["snapshotUri"] = s.into();
    }
    v.to_string()
}

#[tokio::test]
async fn turn_complete_refuses_a_bundle_that_is_not_the_callers_own_turn_bundle() {
    // Sweep SW-67: the engine holds the session's credentials, so it can post turn-complete itself. A bundle it names in
    // another agent's prefix or another turn would become the run's head and reach delivery. Only agentd's own output passes:
    // s3://<harness bucket>/runs/<run>/agents/<caller>/turns/<this turn>/bundle (agentd turn.rs builds exactly that).
    for foreign in [
        "s3://b/runs/R1/agents/hidden/turns/1/bundle",
        "s3://b/runs/R1/agents/a2/turns/2/bundle",
        "s3://b/runs/R1/agents/merge/turns/3/bundle",
        "s3://b/runs/R2/agents/a1/turns/2/bundle",
        "s3://other-bucket/runs/R1/agents/a1/turns/2/bundle",
        "s3://b/runs/R1/agents/a1/turns/1/bundle",
        "s3://b/runs/R1/agents/a1/turns/02/bundle",
        "s3://b/runs/R1/agents/a1/turns/2/bundle.lock.json",
        "s3://b/runs/R1/agents/a1/turns/2/../../hidden/turns/1/bundle",
        "s3://b/runs/R1/agents/a1//turns/2/bundle",
        "s3://b/runs/R1/agents/a1/turns/2/bundle?versionId=1",
        "s3://b/runs/R1/agents/a1/turns/2/bundle#x",
        "s3://b/runs/R1/agents/%61%31/turns/2/bundle",
        "S3://b/runs/R1/agents/a1/turns/2/bundle",
        "s3://b/runs/R1/input/repo.bundle",
        "file:///work/turns/2/bundle",
        "",
    ] {
        let d = deps(with_turn(open_turn(2, "ct-1")));
        let resp = handle(
            post(
                "/internal/turn-complete",
                turn_complete_naming(Some(foreign), None),
            ),
            &d,
        )
        .await;
        assert_eq!(resp.status, 403, "{foreign}: {}", resp.body);
        let err: ApiError = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(err.code, "foreign_bundle", "{foreign}");
        assert!(
            !resp.body.contains("hidden"),
            "the denial echoes nothing: {}",
            resp.body
        );
        // Nothing is recorded and nothing reaches Step Functions.
        assert!(d.callbacks.done.lock().unwrap().is_empty(), "{foreign}");
        assert!(d.store.completes.lock().unwrap().is_empty(), "{foreign}");
    }
}

#[tokio::test]
async fn turn_complete_refuses_any_snapshot_uri_while_no_producer_writes_one() {
    for snapshot in [
        "s3://b/runs/R1/agents/a1/turns/2/snapshot",
        "s3://b/runs/R1/agents/hidden/turns/1/bundle",
    ] {
        let d = deps(with_turn(open_turn(2, "ct-1")));
        let resp = handle(
            post(
                "/internal/turn-complete",
                turn_complete_naming(
                    Some("s3://b/runs/R1/agents/a1/turns/2/bundle"),
                    Some(snapshot),
                ),
            ),
            &d,
        )
        .await;
        assert_eq!(resp.status, 403, "{snapshot}: {}", resp.body);
        let err: ApiError = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(err.code, "foreign_bundle");
        assert!(d.callbacks.done.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn turn_complete_accepts_the_callers_own_turn_bundle_or_none() {
    for bundle in [Some("s3://b/runs/R1/agents/a1/turns/2/bundle"), None] {
        let d = deps(with_turn(open_turn(2, "ct-1")));
        let resp = handle(
            post(
                "/internal/turn-complete",
                turn_complete_naming(bundle, None),
            ),
            &d,
        )
        .await;
        assert_eq!(resp.status, 200, "{bundle:?}: {}", resp.body);
        let done = d.callbacks.done.lock().unwrap().clone();
        assert_eq!(done.len(), 1, "{bundle:?}");
        let sent: TurnCompleteRequest = serde_json::from_str(&done[0].2).unwrap();
        assert_eq!(sent.bundle_uri.as_deref(), bundle);
    }
}

// ---- the direct agent: one agent `direct`, one turn, the run machine's StartSession waits on its token ----

fn direct_post(path: &str, body: String) -> InternalRequest {
    InternalRequest {
        method: "POST".into(),
        path: path.into(),
        caller_arn: Some(format!(
            "arn:aws:sts::700000000011:assumed-role/{ROLE}/R1.direct"
        )),
        completion_key: None,
        body,
    }
}

fn direct_turn(ciphertext: &str) -> TurnItem {
    let mut t = open_turn(1, ciphertext);
    t.agent_id = "direct".into();
    t
}

fn direct_complete_body() -> String {
    serde_json::json!({
        "summary": "Added pct(a, b).", "runId": "R1", "agentId": "direct", "turn": 1,
        "finishReason": "end_turn", "lastCommitSha": "c".repeat(40),
        "bundleUri": "s3://b/runs/R1/agents/direct/turns/1/bundle", "outcome": "completed"
    })
    .to_string()
}

#[tokio::test]
async fn the_direct_agents_heartbeat_keeps_the_state_alive_with_the_token_the_turn_item_holds() {
    let d = deps(with_turn(direct_turn("ct-first")));
    let resp = handle(
        direct_post(
            "/internal/heartbeat",
            r#"{"runId":"R1","agentId":"direct","turn":1}"#.into(),
        ),
        &d,
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(d.callbacks.alive.lock().unwrap().len(), 1);
    assert_eq!(d.callbacks.alive.lock().unwrap()[0].0, "ct-first");
    // a retried waitForTaskToken state: start-session replaced the ciphertext; heartbeats and the result follow it
    d.store
        .turn
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .task_token_ciphertext = "ct-newest".into();
    handle(
        direct_post(
            "/internal/heartbeat",
            r#"{"runId":"R1","agentId":"direct","turn":1}"#.into(),
        ),
        &d,
    )
    .await;
    assert_eq!(d.callbacks.alive.lock().unwrap()[1].0, "ct-newest");
    let done = handle(
        direct_post("/internal/turn-complete", direct_complete_body()),
        &d,
    )
    .await;
    assert_eq!(done.status, 200, "{}", done.body);
    let calls = d.callbacks.done.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "ct-newest");
    assert_eq!(
        calls[0].1,
        TokenContext {
            run_id: "R1".into(),
            agent_id: "direct".into(),
            turn: 1
        }
    );
}

#[tokio::test]
async fn the_direct_turn_result_is_the_whole_request_inside_the_task_output_limit() {
    let d = deps(with_turn(direct_turn("ct-1")));
    // agentd's summary is the last 8,000 characters of the message: well inside SendTaskSuccess's 256 KiB
    let big = "x".repeat(8_000);
    let body = serde_json::json!({
        "summary": big, "runId": "R1", "agentId": "direct", "turn": 1, "finishReason": "end_turn",
        "lastCommitSha": "c".repeat(40), "bundleUri": "s3://b/runs/R1/agents/direct/turns/1/bundle",
        "outcome": "completed", "error": "none"
    });
    handle(direct_post("/internal/turn-complete", body.to_string()), &d).await;
    let output = d.callbacks.done.lock().unwrap()[0].2.clone();
    assert!(output.len() < 256 * 1024);
    let sent: TurnCompleteRequest = serde_json::from_str(&output).unwrap();
    assert_eq!(sent.summary.as_deref(), Some(big.as_str()));
    assert_eq!(
        sent.bundle_uri.as_deref(),
        Some("s3://b/runs/R1/agents/direct/turns/1/bundle")
    );
    assert_eq!(
        sent.last_commit_sha.as_deref(),
        Some("c".repeat(40).as_str())
    );
}

#[tokio::test]
async fn the_direct_agent_may_name_only_its_own_bundle_and_no_snapshot() {
    for (field, value) in [
        ("bundleUri", "s3://b/runs/R1/agents/other/turns/1/bundle"),
        (
            "bundleUri",
            "s3://b/runs/R1/agents/direct/turns/1/snapshot.bundle",
        ),
        ("bundleUri", "s3://b/runs/R1/agents/direct/turns/2/bundle"),
        (
            "bundleUri",
            "s3://elsewhere/runs/R1/agents/direct/turns/1/bundle",
        ),
        (
            "snapshotUri",
            "s3://b/runs/R1/agents/direct/turns/1/snapshot.bundle",
        ),
    ] {
        let d = deps(with_turn(direct_turn("ct-1")));
        let mut body: serde_json::Value = serde_json::from_str(&direct_complete_body()).unwrap();
        body[field] = serde_json::json!(value);
        let resp = handle(direct_post("/internal/turn-complete", body.to_string()), &d).await;
        assert_eq!(resp.status, 403, "{field}={value}: {}", resp.body);
        assert!(d.callbacks.done.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn an_old_token_that_step_functions_closed_is_logged_and_the_turn_is_still_accepted() {
    // A retried StartSession abandons its first task; a result posted for that token cannot complete anything, and the
    // machine's heartbeat or the replaced token carries the run on.
    let d = deps_with(
        with_turn(direct_turn("ct-old")),
        FakeCallbacks {
            fail: Some(CallbackError::Closed("TaskTimedOut".into())),
            ..Default::default()
        },
    );
    let resp = handle(
        direct_post("/internal/turn-complete", direct_complete_body()),
        &d,
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(*d.store.completes.lock().unwrap(), vec![1]);
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
}

// ---- message control on a store that keeps no ledger (the defaults of the Store trait) ---------------------------------------

#[tokio::test]
async fn a_store_that_keeps_no_ledger_answers_the_four_message_routes_with_503() {
    let d = deps(FakeStore::default());
    let base = serde_json::json!({
        "runId": "R1", "agentId": "direct", "generation": "01M5G000000000000000000001", "epoch": "s1"
    });
    let with = |extra: serde_json::Value| {
        let mut b = base.clone();
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    };
    for (op, body) in [
        ("message-session", with(serde_json::json!({"number": 1}))),
        ("message-claim", base.clone()),
        ("message-seal", base.clone()),
        (
            "message-outcome",
            with(serde_json::json!({
                "messageId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "attempt": 1, "outcome": {"kind": "released"}
            })),
        ),
    ] {
        let resp = handle(
            direct_post(&format!("/internal/{op}"), body.to_string()),
            &d,
        )
        .await;
        assert_eq!(resp.status, 503, "{op}: {}", resp.body);
        assert!(
            resp.body.contains("store_unavailable"),
            "{op}: {}",
            resp.body
        );
    }
}

#[tokio::test]
async fn a_store_that_keeps_no_ledger_hands_out_no_notice_to_the_direct_agent() {
    let mut direct = agent("n0");
    direct.agent_id = "direct".into();
    let d = deps(FakeStore {
        agent: Mutex::new(Some(direct)),
        goal: Some(GoalStatus::Active),
        ..Default::default()
    });
    let batch = EventsBatchRequest {
        run_id: "R1".into(),
        agent_id: "direct".into(),
        events: vec![event(Authority::Agent, EventType::TurnProgress, "direct")],
        take_controls: None,
    };
    let resp = handle(
        direct_post("/internal/events", serde_json::to_string(&batch).unwrap()),
        &d,
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body: EventsBatchResponse = serde_json::from_str(&resp.body).unwrap();
    assert!(body.pending.is_empty(), "{:?}", body.pending);
}

// ----: a run is a list of slices; only the slice the Agent item names may act for the run ----

/// A store whose Agent item names slice `generation`, with the Turn item of slice `turn` open.
fn at_slice(generation: Option<u32>, turn: u32) -> FakeStore {
    let mut a = agent("n0");
    a.session_generation = generation;
    let mut t = direct_turn("ct-slice");
    t.n = turn;
    FakeStore {
        agent: Mutex::new(Some(a)),
        goal: Some(GoalStatus::Active),
        turn: Mutex::new(Some(t)),
        ..Default::default()
    }
}

fn direct_heartbeat(turn: u32) -> InternalRequest {
    direct_post(
        "/internal/heartbeat",
        format!(r#"{{"runId":"R1","agentId":"direct","turn":{turn}}}"#),
    )
}

#[tokio::test]
async fn b688_a_heartbeat_from_a_slice_the_run_has_left_is_refused_and_records_nothing() {
    let d = deps(at_slice(Some(2), 1));
    let resp = handle(direct_heartbeat(1), &d).await;
    assert_eq!(resp.status, 409, "{}", resp.body);
    let err: ApiError = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(err.code, "stale-generation");
    assert_eq!(*d.store.heartbeats.lock().unwrap(), 0);
    assert!(d.callbacks.alive.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b688_a_turn_complete_from_a_slice_the_run_has_left_changes_nothing() {
    let d = deps(at_slice(Some(2), 1));
    let resp = handle(
        direct_post("/internal/turn-complete", direct_complete_body()),
        &d,
    )
    .await;
    assert_eq!(resp.status, 409, "{}", resp.body);
    let err: ApiError = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(err.code, "stale-generation");
    assert!(d.store.completes.lock().unwrap().is_empty());
    assert!(d.store.usages.lock().unwrap().is_empty());
    assert!(d.callbacks.done.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b688_the_slice_the_agent_item_names_passes_the_fence() {
    let d = deps(at_slice(Some(2), 2));
    let resp = handle(direct_heartbeat(2), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(*d.store.heartbeats.lock().unwrap(), 1);
    assert_eq!(d.callbacks.alive.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn b688_an_agent_item_written_before_slices_passes_every_turn() {
    // An item with no session generation cannot tell slices apart, so it keeps today's behaviour.
    let d = deps(at_slice(None, 1));
    let resp = handle(direct_heartbeat(1), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
}

// ----: the engine holds the same session credentials as agentd; events and turn-complete need the key only agentd holds ----

const FIRST_NONCE: &str = "first-nonce-of-the-run-held-by-run-session-and-agentd";

fn run_key() -> String {
    CredsBinding::completion_key(FIRST_NONCE)
}

/// The store of a run whose Agent item names the hash of the run's key, with the direct agent's open turn.
fn keyed_store() -> FakeStore {
    let store = with_turn(direct_turn("ct-1"));
    store
        .agent
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|a| a.creds.as_mut())
        .unwrap()
        .completion_key_sha256 = Some(CredsBinding::hash_completion_key(&run_key()));
    store
}

fn with_key(mut req: InternalRequest, key: Option<&str>) -> InternalRequest {
    req.completion_key = key.map(str::to_string);
    req
}

fn direct_events_body() -> String {
    serde_json::to_string(&EventsBatchRequest {
        run_id: "R1".into(),
        agent_id: "direct".into(),
        events: vec![event(Authority::Agent, EventType::TurnProgress, "direct")],
        take_controls: None,
    })
    .unwrap()
}

fn nothing_happened(d: &Deps<FakeStore, FakeMinter, FakeCallbacks>) {
    assert!(
        d.callbacks.done.lock().unwrap().is_empty(),
        "no SendTaskSuccess"
    );
    assert!(
        d.store.completes.lock().unwrap().is_empty(),
        "no turn recorded"
    );
    assert!(
        d.store.events.lock().unwrap().is_empty(),
        "no event written"
    );
    assert_eq!(
        d.store.turn.lock().unwrap().as_ref().unwrap().ended_at,
        None,
        "the turn stays open"
    );
}

#[tokio::test]
async fn b593_an_engine_that_posts_turn_complete_itself_without_the_key_is_refused_and_nothing_happens()
 {
    let d = deps(keyed_store());
    // The engine planted a bundle at the one key worker-api accepts, then posts the completion with the shared session credentials.
    let resp = handle(
        with_key(
            direct_post("/internal/turn-complete", direct_complete_body()),
            None,
        ),
        &d,
    )
    .await;
    assert_eq!(resp.status, 403, "{}", resp.body);
    let e: ApiError = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(e.code, "completion_key");
    nothing_happened(&d);
}

#[tokio::test]
async fn b593_a_wrong_key_is_refused_on_both_routes_and_the_error_never_echoes_a_key() {
    let d = deps(keyed_store());
    let other = CredsBinding::completion_key("the-nonce-of-another-run");
    for key in [
        other.as_str(),
        "x",
        &CredsBinding::hash_completion_key(&run_key()),
    ] {
        for (path, body) in [
            ("/internal/turn-complete", direct_complete_body()),
            ("/internal/events", direct_events_body()),
        ] {
            let resp = handle(with_key(direct_post(path, body), Some(key)), &d).await;
            assert_eq!(resp.status, 403, "{path}: {}", resp.body);
            assert!(
                !resp.body.contains(key) && !resp.body.contains(&run_key()),
                "{}",
                resp.body
            );
        }
    }
    nothing_happened(&d);
}

#[tokio::test]
async fn b593_the_runs_key_completes_the_turn_and_writes_events() {
    let d = deps(keyed_store());
    let key = run_key();
    let ev = handle(
        with_key(
            direct_post("/internal/events", direct_events_body()),
            Some(&key),
        ),
        &d,
    )
    .await;
    assert_eq!(ev.status, 200, "{}", ev.body);
    assert_eq!(d.store.events.lock().unwrap().len(), 1);
    let done = handle(
        with_key(
            direct_post("/internal/turn-complete", direct_complete_body()),
            Some(&key),
        ),
        &d,
    )
    .await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(d.callbacks.done.lock().unwrap().len(), 1);
    // An identical retry (the answer was lost) is accepted again and sends nothing a second time.
    let again = handle(
        with_key(
            direct_post("/internal/turn-complete", direct_complete_body()),
            Some(&key),
        ),
        &d,
    )
    .await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(d.callbacks.done.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn b593_a_run_whose_item_names_no_key_is_accepted_as_before() {
    // An Agent item written before the key existed: nothing names a key, and the engine cannot write an Agent item.
    let d = deps(with_turn(direct_turn("ct-1")));
    for (path, body) in [
        ("/internal/events", direct_events_body()),
        ("/internal/turn-complete", direct_complete_body()),
    ] {
        let resp = handle(direct_post(path, body), &d).await;
        assert_eq!(resp.status, 200, "{path}: {}", resp.body);
    }
    // A request with no Agent item at all keeps the answer it had.
    let none = deps(FakeStore::default());
    let resp = handle(direct_post("/internal/events", direct_events_body()), &none).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
}

#[tokio::test]
async fn b593_a_stored_hash_that_is_malformed_refuses_every_request() {
    let d = deps(keyed_store());
    d.store
        .agent
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|a| a.creds.as_mut())
        .unwrap()
        .completion_key_sha256 = Some("not-a-hash".into());
    for key in [None, Some(run_key())] {
        let resp = handle(
            with_key(
                direct_post("/internal/events", direct_events_body()),
                key.as_deref(),
            ),
            &d,
        )
        .await;
        assert_eq!(resp.status, 403, "{}", resp.body);
    }
}

#[tokio::test]
async fn b593_the_heartbeat_and_the_credential_refresh_do_not_need_the_key() {
    let d = deps(keyed_store());
    let hb = handle(
        direct_post(
            "/internal/heartbeat",
            r#"{"runId":"R1","agentId":"direct","turn":1}"#.into(),
        ),
        &d,
    )
    .await;
    assert_eq!(hb.status, 200, "{}", hb.body);
}

#[tokio::test]
async fn b776_a_long_summary_is_cut_to_a_preview_before_step_functions_gets_it() {
    use harness_protocol::api::internal::MAX_TURN_SUMMARY_BYTES;
    use harness_protocol::direct::json_len;
    let d = deps(with_turn(open_turn(2, "ct-1")));
    let long: String = (0..60_000).map(|i| format!("row {i} \u{e9}\n")).collect();
    let body = serde_json::json!({"runId": "R1", "agentId": "a1", "turn": 2, "finishReason": "end_turn",
        "lastCommitSha": "c".repeat(40), "bundleUri": "s3://b/runs/R1/agents/a1/turns/2/bundle",
        "usage": null, "outcome": "completed", "summary": long});
    let resp = handle(post("/internal/turn-complete", body.to_string()), &d).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let done = d.callbacks.done.lock().unwrap().clone();
    let sent: TurnCompleteRequest = serde_json::from_str(&done[0].2).unwrap();
    let summary = sent.summary.unwrap();
    assert!(
        json_len(&summary) <= MAX_TURN_SUMMARY_BYTES,
        "{}",
        json_len(&summary)
    );
    assert!(summary.starts_with("row 0 "));
    assert!(summary.ends_with("row 59999 \u{e9}\n"), "the end is kept");
    assert!(summary.contains("characters omitted"));
    assert!(
        done[0].2.len() < 64 * 1024,
        "the whole output stays small: {}",
        done[0].2.len()
    );
}
