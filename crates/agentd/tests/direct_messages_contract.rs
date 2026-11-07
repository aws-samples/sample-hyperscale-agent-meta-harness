//! What the control plane promises agentd about messages, asserted against the fake
//! plane the agentd tests use (`Plane`, answering from the server model) and against agentd's own store over it. A fake that accepts what the
//! real routes refuse proves nothing (the fakes-enforce-contracts rule), so each promise is checked here twice: the plane refuses what the
//! real route refuses, and agentd's store and module behave as the promise needs.
//!
//! 1. A normal or limit-reached seal gets 409 `bad-transition` while agentd still owes the outcome of a claimed message, also after the user's
//!    cancel; the text names the ids. A terminal seal does not wait.
//! 2. A claim asked with no id returns every message this incarnation holds in `dispatching`, `redelivered`, with the same attempt, also after
//!    the session changed.
//! 3. An unseen incarnation whose ULID is more than 60 s older than the current one is 409 `stale-generation`; one within 60 s replaces it; a
//!    replaced incarnation is stale on every route.
//! 4. A release that ended `rejected` because the run was closed is answered 200 again when repeated; a rejection of a pending message takes
//!    attempt 0 or the attempt of its last claim, any other attempt is 409 `stale-attempt`.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_messages_world;
mod direct_support;

use agentd::direct::messages::http::{HttpMessageStore, Transport};
use agentd::direct::messages::{MemoryStore, Message, MessageStore, Mode, RejectReason, State};
use direct_messages_world::{Plane, ulid_at};
use serde_json::{Value, json};
use std::sync::Arc;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const NOW_MS: u64 = 1_760_000_000_000;

fn gen_at(ms: u64) -> String {
    ulid_at(ms, "AAAAAAAAAAAAAAAA")
}

struct Rig {
    store: Arc<MemoryStore>,
    plane: Arc<Plane>,
}

impl Rig {
    fn new() -> Self {
        let store = MemoryStore::new();
        let plane = Arc::new(Plane::new(store.clone()));
        Self { store, plane }
    }

    fn msg(&self, id: &str, epoch: &str) {
        self.store
            .accept(Message {
                id: id.into(),
                text: format!("text of {id}"),
                mode: Mode::Next,
                epoch: epoch.into(),
                seq: 0,
            })
            .unwrap();
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let (status, bytes) = self
            .plane
            .post(path, serde_json::to_vec(&body).unwrap())
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn session(&self, generation: &str, epoch: &str, number: u32) -> (u16, Value) {
        self.post(
            "/internal/message-session",
            json!({"runId": RUN, "agentId": "direct", "generation": generation, "epoch": epoch, "number": number}),
        )
        .await
    }

    async fn claim(&self, generation: &str, epoch: &str, id: Option<&str>) -> (u16, Value) {
        let mut body =
            json!({"runId": RUN, "agentId": "direct", "generation": generation, "epoch": epoch});
        if let Some(id) = id {
            body["messageId"] = json!(id);
        }
        self.post("/internal/message-claim", body).await
    }

    async fn outcome(
        &self,
        generation: &str,
        epoch: &str,
        id: &str,
        attempt: u32,
        outcome: Value,
    ) -> (u16, Value) {
        self.post(
            "/internal/message-outcome",
            json!({"runId": RUN, "agentId": "direct", "generation": generation, "epoch": epoch,
                   "messageId": id, "attempt": attempt, "outcome": outcome}),
        )
        .await
    }

    async fn seal(&self, generation: &str, epoch: &str, mode: &str) -> (u16, Value) {
        self.post(
            "/internal/message-seal",
            json!({"runId": RUN, "agentId": "direct", "generation": generation, "epoch": epoch, "mode": mode}),
        )
        .await
    }
}

// ---- 1. the seal waits for the outcomes agentd owes ------------------------------------------------------------------------------------

#[tokio::test]
async fn a_normal_or_limit_seal_is_409_bad_transition_naming_the_messages_agentd_still_owes_and_a_terminal_seal_is_not()
 {
    let r = Rig::new();
    let g = gen_at(NOW_MS);
    assert_eq!(r.session(&g, "s1", 1).await.0, 200);
    r.msg("m1", "s1");
    r.msg("m2", "s1");
    assert_eq!(r.claim(&g, "s1", Some("m1")).await.0, 200);
    assert_eq!(r.claim(&g, "s1", Some("m2")).await.0, 200);
    for mode in ["normal", "limit-reached"] {
        let (status, body) = r.seal(&g, "s1", mode).await;
        assert_eq!(status, 409, "{mode}: {body}");
        assert_eq!(body["code"], "bad-transition");
        let text = body["message"].as_str().unwrap();
        assert!(
            text.contains("m1") && text.contains("m2"),
            "the refusal names the ids: {text}"
        );
    }
    // the outcomes first, then the seal
    assert_eq!(
        r.outcome(&g, "s1", "m1", 1, json!({"kind": "submitted", "turn": 2}))
            .await
            .0,
        200
    );
    assert_eq!(r.seal(&g, "s1", "normal").await.0, 409, "m2 is still owed");
    assert_eq!(
        r.outcome(&g, "s1", "m2", 1, json!({"kind": "submitted", "turn": 3}))
            .await
            .0,
        200
    );
    let (status, body) = r.seal(&g, "s1", "normal").await;
    assert_eq!(
        (status, body["outcome"].as_str()),
        (200, Some("sealed")),
        "{body}"
    );
}

#[tokio::test]
async fn after_the_users_cancel_a_normal_seal_still_waits_and_a_terminal_seal_turns_the_message_into_delivery_unknown()
 {
    let r = Rig::new();
    let g = gen_at(NOW_MS);
    r.session(&g, "s1", 1).await;
    r.msg("m1", "s1");
    r.claim(&g, "s1", Some("m1")).await;
    r.store.queue_cancel();
    let (status, body) = r.seal(&g, "s1", "normal").await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "bad-transition");
    let (status, body) = r.seal(&g, "s1", "terminal").await;
    assert_eq!(
        (status, body["outcome"].as_str()),
        (200, Some("sealed")),
        "{body}"
    );
    assert_eq!(r.store.state("m1"), Some(State::DeliveryUnknown));
}

// ---- 2. a claim without an id returns what this incarnation holds ----------------------------------------------------------------------

#[tokio::test]
async fn a_claim_without_an_id_returns_every_dispatching_message_redelivered_with_the_same_attempt_also_after_the_session_changed()
 {
    let r = Rig::new();
    let g = gen_at(NOW_MS);
    r.session(&g, "s1", 1).await;
    r.msg("m1", "s1");
    r.msg("m2", "s1");
    r.claim(&g, "s1", Some("m1")).await;
    r.claim(&g, "s1", Some("m2")).await;
    // the answers were lost: the module asks again, without an id
    let (status, body) = r.claim(&g, "s1", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["outcome"], "claimed");
    assert_eq!(body["redelivered"], true);
    let held: Vec<(&str, u64)> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["messageId"].as_str().unwrap(),
                m["attempt"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(held, [("m1", 1), ("m2", 1)]);
    // the session changes; the same question still finds them
    assert_eq!(r.session(&g, "s2", 2).await.0, 200);
    let (status, body) = r.claim(&g, "s2", None).await;
    assert_eq!(
        (status, body["redelivered"].clone()),
        (200, json!(true)),
        "{body}"
    );
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    // and a by-id claim of a held message is the same claim, also in the new session
    let (status, body) = r.claim(&g, "s2", Some("m1")).await;
    assert_eq!(
        (status, body["outcome"].as_str()),
        (200, Some("claimed")),
        "{body}"
    );
    assert_eq!(body["messages"][0]["attempt"], 1);
}

#[tokio::test]
async fn agentds_store_reads_what_it_holds_and_knows_which_are_of_the_current_session() {
    let r = Rig::new();
    let g = gen_at(NOW_MS);
    let store = HttpMessageStore::new(r.plane.clone(), RUN, "direct", &g);
    store.report_epoch("s1", 1).await.unwrap();
    r.msg("m1", "s1");
    r.msg("m2", "s1");
    store.claim("m1", "s1").await.unwrap();
    store.claim("m2", "s1").await.unwrap();
    let held = store.held("s1").await.unwrap();
    assert_eq!(
        held.iter()
            .map(|h| h.message.id.as_str())
            .collect::<Vec<_>>(),
        ["m1", "m2"]
    );
    assert!(
        held.iter().all(|h| h.redelivered && h.current_session),
        "{held:?}"
    );
    assert_eq!(held[0].message.text, "text of m1");
    // the session changes: the same messages are found, and they are not of the current session
    store.report_epoch("s2", 2).await.unwrap();
    let held = store.held("s2").await.unwrap();
    assert_eq!(held.len(), 2);
    assert!(
        held.iter().all(|h| h.redelivered && !h.current_session),
        "{held:?}"
    );
    // the request had no message id
    let seen = r.plane.seen.lock().unwrap().clone();
    let last = seen.last().unwrap();
    assert_eq!(last.0, "/internal/message-claim");
    assert!(last.1.get("messageId").is_none(), "{:?}", last.1);
    // the outcomes carry the attempts that were read
    store.release("m1").await.unwrap();
    assert_eq!(r.store.state("m1"), Some(State::Held));
}

// ---- 3. incarnations ------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unseen_incarnation_more_than_sixty_seconds_older_is_stale_and_one_within_sixty_seconds_replaces_the_current()
 {
    let r = Rig::new();
    let current = gen_at(NOW_MS);
    assert_eq!(r.session(&current, "s1", 1).await.0, 200);
    // a delayed first report of an incarnation that was replaced long ago
    let (status, body) = r.session(&gen_at(NOW_MS - 61_000), "s1", 1).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("stale-generation")),
        "{body}"
    );
    // a replacement microVM whose clock runs a little behind
    let slow = gen_at(NOW_MS - 30_000);
    assert_eq!(r.session(&slow, "s1", 1).await.0, 200);
    // the replaced incarnation is stale on every route from then on
    for (status, body) in [
        r.session(&current, "s1", 2).await,
        r.claim(&current, "s1", None).await,
        r.seal(&current, "s1", "normal").await,
        r.outcome(&current, "s1", "m1", 1, json!({"kind": "released"}))
            .await,
    ] {
        assert_eq!(
            (status, body["code"].as_str()),
            (409, Some("stale-generation")),
            "{body}"
        );
    }
}

#[tokio::test]
async fn a_call_before_any_session_report_is_409_no_session_and_a_replaced_incarnations_dispatching_messages_become_delivery_unknown()
 {
    let r = Rig::new();
    let first = gen_at(NOW_MS);
    let (status, body) = r.claim(&first, "s1", None).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("no-session")),
        "{body}"
    );
    r.session(&first, "s1", 1).await;
    r.msg("m1", "s1");
    r.claim(&first, "s1", Some("m1")).await;
    // a replacement microVM reports a session: what the first was sending is delivery-unknown, never sent again
    let second = gen_at(NOW_MS + 5_000);
    assert_eq!(r.session(&second, "s2", 1).await.0, 200);
    assert_eq!(r.store.state("m1"), Some(State::DeliveryUnknown));
}

// ---- 4. outcomes ----------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_release_that_ended_rejected_for_the_closed_run_is_answered_200_again_when_repeated() {
    let r = Rig::new();
    let g = gen_at(NOW_MS);
    r.session(&g, "s1", 1).await;
    r.msg("m1", "s1");
    r.claim(&g, "s1", Some("m1")).await;
    r.store.queue_cancel();
    for _ in 0..2 {
        let (status, body) = r
            .outcome(&g, "s1", "m1", 1, json!({"kind": "released"}))
            .await;
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(
        r.store.state("m1"),
        Some(State::Rejected(RejectReason::RunCancelled))
    );
}

#[tokio::test]
async fn a_rejection_of_a_pending_message_takes_attempt_zero_or_its_last_claims_attempt_and_no_other()
 {
    let r = Rig::new();
    let g = gen_at(NOW_MS);
    r.session(&g, "s1", 1).await;
    for id in ["m1", "m2", "m3"] {
        r.msg(id, "s1");
    }
    let reject = json!({"kind": "rejected", "reason": "continuation-limit"});
    // never claimed: attempt 0
    assert_eq!(r.outcome(&g, "s1", "m1", 0, reject.clone()).await.0, 200);
    // claimed once, released: the attempt of its last claim, 1
    r.claim(&g, "s1", Some("m2")).await;
    assert_eq!(
        r.outcome(&g, "s1", "m2", 1, json!({"kind": "released"}))
            .await
            .0,
        200
    );
    let (status, body) = r.outcome(&g, "s1", "m2", 7, reject.clone()).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("stale-attempt")),
        "{body}"
    );
    assert_eq!(r.outcome(&g, "s1", "m2", 1, reject.clone()).await.0, 200);
    // claimed twice (released between): the last claim's attempt is 2, and 1 is stale
    r.claim(&g, "s1", Some("m3")).await;
    r.outcome(&g, "s1", "m3", 1, json!({"kind": "released"}))
        .await;
    let (_, again) = r.claim(&g, "s1", Some("m3")).await;
    assert_eq!(again["messages"][0]["attempt"], 2);
    r.outcome(&g, "s1", "m3", 2, json!({"kind": "released"}))
        .await;
    let (status, body) = r.outcome(&g, "s1", "m3", 1, reject.clone()).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("stale-attempt")),
        "{body}"
    );
    assert_eq!(r.outcome(&g, "s1", "m3", 2, reject).await.0, 200);
    // any other outcome needs the exact attempt of the claim, and 0 is not one
    r.msg("m4", "s1");
    r.claim(&g, "s1", Some("m4")).await;
    let (status, body) = r
        .outcome(&g, "s1", "m4", 0, json!({"kind": "submitted", "turn": 2}))
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("stale-attempt")),
        "{body}"
    );
}

#[tokio::test]
async fn agentds_store_rejects_with_the_attempt_of_the_last_claim_and_a_replaced_agentd_is_fenced_for_good()
 {
    let r = Rig::new();
    let current = gen_at(NOW_MS);
    let mine = HttpMessageStore::new(r.plane.clone(), RUN, "direct", &current);
    mine.report_epoch("s1", 1).await.unwrap();
    r.msg("m1", "s1");
    mine.claim("m1", "s1").await.unwrap();
    mine.release("m1").await.unwrap();
    // pending again after the release: the rejection names the last claim's attempt and the plane accepts it
    mine.reject("m1", RejectReason::ContinuationLimit)
        .await
        .unwrap();
    assert_eq!(
        r.store.state("m1"),
        Some(State::Rejected(RejectReason::ContinuationLimit))
    );
    // an incarnation of agentd that started 61 s earlier than the current one is refused on its first report and is unavailable from then on
    let old = HttpMessageStore::new(r.plane.clone(), RUN, "direct", &gen_at(NOW_MS - 61_000));
    assert!(old.report_epoch("s1", 1).await.is_err());
    assert!(old.unavailable(), "a refused generation stays out");
}

#[test]
fn every_new_agentd_process_makes_a_new_generation_that_is_a_ulid_later_than_the_one_before() {
    let a = agentd::direct::messages::http::new_generation();
    let b = agentd::direct::messages::http::new_generation();
    assert_eq!(a.len(), 26, "{a}");
    assert_ne!(a, b);
    assert!(b > a, "later in time sorts later: {a} {b}");
    assert!(
        direct_messages_world::ulid_ms(&a) > 1_700_000_000_000,
        "{a} carries the clock"
    );
}
