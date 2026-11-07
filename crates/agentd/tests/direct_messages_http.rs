//! The module's calls to the private internal API: the four routes, the bodies agentd
//! sends, and what each answer, error and fencing code means to the module. The transport is a fake that records every request and
//! answers from a script, so no network is used; the bodies are checked against the golden files of IM2c (`fixtures/direct-messages/`),
//! which hold the bytes the writers must produce and the readers must accept.
#![allow(clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::direct::messages::http::{HttpMessageStore, Transport};
use agentd::direct::messages::{
    ClaimOutcome, FailReason, MessageStore, Mode, Refusal, RejectReason, SealMode, SealOutcome,
    StoreError,
};
use harness_protocol::messages::{
    ClaimRequest, MessageOutcome, OutcomeRequest, SealRequest, SessionReport,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const MSG: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const GEN: &str = "01M5A0000000000000000000GE";

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/direct-messages")
        .join(name);
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

/// A transport that answers from a script and keeps what it was asked.
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Result<(u16, String), String>>>,
    seen: Mutex<Vec<(String, Value)>>,
}

impl Script {
    fn reply(&self, status: u16, body: Value) {
        self.replies
            .lock()
            .unwrap()
            .push_back(Ok((status, body.to_string())));
    }

    fn fail(&self, why: &str) {
        self.replies.lock().unwrap().push_back(Err(why.to_string()));
    }

    fn requests(&self) -> Vec<(String, Value)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Transport for Script {
    fn post<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<(u16, Vec<u8>), String>> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .push((path.to_string(), serde_json::from_slice(&body).unwrap()));
            match self.replies.lock().unwrap().pop_front() {
                Some(Ok((s, b))) => Ok((s, b.into_bytes())),
                Some(Err(e)) => Err(e),
                None => panic!("no reply scripted for {path}"),
            }
        })
    }
}

fn store() -> (Arc<Script>, HttpMessageStore) {
    let t = Arc::new(Script::default());
    let s = HttpMessageStore::new(t.clone(), RUN, "direct", GEN);
    (t, s)
}

// ---- the session ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_session_is_reported_to_the_message_session_route_with_the_generation() {
    let (t, s) = store();
    t.reply(200, fixture("session-ack.json"));
    s.report_epoch("sess-3", 3).await.unwrap();
    let (path, body) = t.requests().remove(0);
    assert_eq!(path, "/internal/message-session");
    assert_eq!(body, fixture("session-report.json"));
    // the body is the type of the protocol, field for field
    let typed: SessionReport = serde_json::from_value(body).unwrap();
    assert_eq!(
        (typed.run_id.as_str(), typed.generation.as_str()),
        (RUN, GEN)
    );
}

// ---- the claim -----------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_by_id_sends_the_golden_request_and_returns_the_text() {
    let (t, s) = store();
    t.reply(200, fixture("claim-response-claimed.json"));
    let out = s.claim(MSG, "sess-3").await.unwrap();
    let (path, body) = t.requests().remove(0);
    assert_eq!(path, "/internal/message-claim");
    assert_eq!(body, fixture("claim-request-by-id.json"));
    serde_json::from_value::<ClaimRequest>(body).unwrap();
    match out {
        ClaimOutcome::Claimed(m) => {
            assert_eq!(m.id, MSG);
            assert_eq!(
                m.text,
                "Please also add a README section on the retry rules."
            );
            assert_eq!(m.mode, Mode::Next);
            assert_eq!(m.epoch, "sess-3");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn every_refusal_of_a_claim_maps_to_its_meaning() {
    let (t, s) = store();
    t.reply(200, fixture("claim-response-not-pending.json"));
    assert_eq!(
        s.claim(MSG, "sess-3").await.unwrap(),
        ClaimOutcome::Refused(Refusal::NotPending("withdrawn".into()))
    );
    t.reply(200, fixture("claim-response-refused.json"));
    assert_eq!(
        s.claim(MSG, "sess-3").await.unwrap(),
        ClaimOutcome::Refused(Refusal::Cancelled)
    );
    t.reply(
        200,
        json!({"outcome": "refused", "reason": "admission-closed"}),
    );
    assert_eq!(
        s.claim(MSG, "sess-3").await.unwrap(),
        ClaimOutcome::Refused(Refusal::Sealed)
    );
    t.reply(200, fixture("claim-response-nothing.json"));
    assert!(matches!(
        s.claim(MSG, "sess-3").await.unwrap(),
        ClaimOutcome::Refused(Refusal::NotPending(_))
    ));
}

#[tokio::test]
async fn the_fencing_codes_are_refusals_that_say_stop_acting() {
    let (t, s) = store();
    t.reply(
        409,
        json!({"code": "stale-generation", "message": "replaced"}),
    );
    assert_eq!(
        s.claim(MSG, "sess-3").await.unwrap(),
        ClaimOutcome::Refused(Refusal::StaleGeneration)
    );
    t.reply(409, json!({"code": "stale-epoch", "message": "moved on"}));
    assert_eq!(
        s.claim(MSG, "sess-3").await.unwrap(),
        ClaimOutcome::Refused(Refusal::EpochMoved)
    );
    t.reply(
        409,
        json!({"code": "no-session", "message": "report a session first"}),
    );
    let e = s.claim(MSG, "sess-3").await.unwrap_err();
    assert!(!e.retryable && e.detail.contains("no-session"), "{e:?}");
}

#[tokio::test]
async fn a_server_error_and_a_transport_failure_are_retryable_and_a_missing_route_is_final() {
    let (t, s) = store();
    t.reply(503, json!({"code": "unavailable", "message": "try again"}));
    assert!(s.claim(MSG, "e").await.unwrap_err().retryable);
    t.fail("connection reset");
    assert!(s.claim(MSG, "e").await.unwrap_err().retryable);
    t.reply(429, json!({"code": "rate-limited", "message": "slow down"}));
    assert!(s.claim(MSG, "e").await.unwrap_err().retryable);
    // The credentials of the caller may pass; a 403 is not a missing route.
    t.reply(403, json!({"code": "denied", "message": "not allowed"}));
    assert!(!s.claim(MSG, "e").await.unwrap_err().retryable);
    assert!(!s.plane_has_no_routes() && !s.unavailable());
    t.reply(404, json!({"message": "no such route"}));
    let e: StoreError = s.claim(MSG, "e").await.unwrap_err();
    assert!(!e.retryable && e.detail.contains("no-route"), "{e:?}");
    assert!(
        s.plane_has_no_routes(),
        "a 404 on a message route marks the plane as one without message control"
    );
}

#[tokio::test]
async fn a_body_that_is_not_json_is_a_final_error_and_never_a_claim() {
    let (t, s) = store();
    t.replies
        .lock()
        .unwrap()
        .push_back(Ok((200, "<<garbage>>".into())));
    assert!(s.claim(MSG, "e").await.is_err());
}

// ---- outcomes ------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_outcomes_are_posted_with_the_epoch_of_the_claim_and_the_golden_bodies() {
    let (t, s) = store();
    t.reply(200, fixture("claim-response-claimed.json"));
    s.claim(MSG, "sess-3").await.unwrap();
    // the session moves on after the claim; the outcome still names the session the message was claimed in
    t.reply(200, fixture("session-ack.json"));
    s.report_epoch("sess-4", 4).await.unwrap();

    t.reply(200, fixture("outcome-response.json"));
    s.submitted(MSG, 4).await.unwrap();
    t.reply(200, fixture("outcome-response.json"));
    s.failed(MSG, FailReason::NotSent).await.unwrap();
    t.reply(200, fixture("outcome-response.json"));
    s.release(MSG).await.unwrap();
    t.reply(200, fixture("outcome-response.json"));
    s.reject(MSG, RejectReason::ContinuationLimit)
        .await
        .unwrap();

    let reqs = t.requests();
    let outcome =
        |i: usize| -> OutcomeRequest { serde_json::from_value(reqs[i].1.clone()).unwrap() };
    for r in &reqs[2..=5] {
        assert_eq!(r.0, "/internal/message-outcome");
    }
    assert_eq!(reqs[2].1, fixture("outcome-submitted.json"));
    assert_eq!(outcome(2).epoch, "sess-3");
    assert!(
        matches!(outcome(3).outcome, MessageOutcome::Failed { ref detail } if detail.contains("not-sent"))
    );
    assert_eq!(reqs[4].1, fixture("outcome-released.json"));
    assert_eq!(reqs[5].1, fixture("outcome-rejected.json"));
}

#[tokio::test]
async fn an_outcome_the_server_refuses_as_a_bad_transition_is_a_final_error() {
    let (t, s) = store();
    t.reply(
        409,
        json!({"code": "bad-transition", "message": "not dispatching"}),
    );
    let e = s.submitted(MSG, 2).await.unwrap_err();
    assert!(!e.retryable && e.detail.contains("bad-transition"), "{e:?}");
}

// ---- the seal ------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_seal_sends_its_mode_and_reads_pending_notices_or_the_sealed_answer() {
    let (t, s) = store();
    t.reply(200, fixture("seal-response-pending.json"));
    match s.seal("sess-3", SealMode::Normal).await.unwrap() {
        SealOutcome::Pending(v) => {
            assert_eq!(v.len(), 1);
            assert_eq!(
                (v[0].id.as_str(), v[0].bytes, v[0].mode),
                (MSG, 52, Mode::Next)
            );
            assert_eq!(v[0].epoch, "sess-3");
        }
        other => panic!("{other:?}"),
    }
    t.reply(200, fixture("seal-response-sealed.json"));
    assert_eq!(
        s.seal("sess-3", SealMode::Terminal).await.unwrap(),
        SealOutcome::Sealed { watermark: 3 }
    );
    let reqs = t.requests();
    assert_eq!(reqs[0].0, "/internal/message-seal");
    assert_eq!(reqs[0].1, fixture("seal-request.json"));
    assert_eq!(reqs[1].1, fixture("seal-request-terminal.json"));
    let typed: SealRequest = serde_json::from_value(reqs[1].1.clone()).unwrap();
    assert_eq!(typed.generation, GEN);
    t.reply(200, fixture("seal-response-sealed.json"));
    s.seal("sess-3", SealMode::LimitReached).await.unwrap();
    assert_eq!(t.requests()[2].1["mode"], "limit-reached");
}

// ---- the controls ----------------------------------------------------------------------------------------------------------------------

#[test]
fn a_message_notice_in_the_pending_list_becomes_a_queue_entry_without_text() {
    use agentd::direct::messages::http::notice_of;
    use harness_protocol::api::internal::PendingControl;
    let c: PendingControl =
        serde_json::from_value(fixture("pending-control-message.json")).unwrap();
    let n = notice_of(&c).unwrap();
    assert_eq!(
        (n.id.as_str(), n.seq, n.mode, n.epoch.as_str(), n.bytes),
        (MSG, 1, Mode::Next, "sess-3", 52)
    );
    assert!(notice_of(&PendingControl::Cancel).is_none());
}

#[tokio::test]
async fn a_405_and_the_gateways_unknown_resource_answer_are_a_plane_without_the_routes() {
    for (status, body) in [
        (
            405,
            json!({"code": "method-not-allowed", "message": "nothing here"}),
        ),
        (
            403,
            json!({"code": "forbidden", "message": "Missing Authentication Token"}),
        ),
    ] {
        let (t, s) = store();
        t.reply(status, body);
        let e = s.report_epoch("sess-3", 3).await.unwrap_err();
        assert!(
            !e.retryable && e.detail.contains("no-route"),
            "{status}: {e:?}"
        );
        assert!(s.unavailable(), "{status}");
    }
}

// ---- protocol 1.38.0 additions: the attempt of a claim, and the number of a session --------------------------------------------------------

#[tokio::test]
async fn an_outcome_repeats_the_attempt_of_its_claim_and_a_new_claim_after_a_release_has_the_next_one()
 {
    let (t, s) = store();
    t.reply(200, fixture("claim-response-claimed.json"));
    s.claim(MSG, "sess-3").await.unwrap();
    t.reply(200, fixture("outcome-response.json"));
    s.release(MSG).await.unwrap();
    // the claim is given back and taken again: attempt 2
    let mut again = fixture("claim-response-claimed.json");
    again["messages"][0]["attempt"] = json!(2);
    t.reply(200, again);
    s.claim(MSG, "sess-3").await.unwrap();
    t.reply(200, fixture("outcome-response.json"));
    s.submitted(MSG, 5).await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs[1].1["attempt"], 1, "{:?}", reqs[1]);
    assert_eq!(reqs[3].1["attempt"], 2, "{:?}", reqs[3]);
}

#[tokio::test]
async fn a_rejection_of_a_message_that_was_never_claimed_carries_attempt_zero() {
    let (t, s) = store();
    t.reply(200, fixture("outcome-response.json"));
    s.reject(MSG, RejectReason::ContinuationLimit)
        .await
        .unwrap();
    assert_eq!(t.requests()[0].1["attempt"], 0);
}

#[tokio::test]
async fn a_stale_attempt_is_over_and_is_not_retried() {
    let (t, s) = store();
    t.reply(200, fixture("claim-response-claimed.json"));
    s.claim(MSG, "sess-3").await.unwrap();
    t.reply(
        409,
        json!({"code": "stale-attempt", "message": "that attempt is over"}),
    );
    let e = s.submitted(MSG, 4).await.unwrap_err();
    assert!(!e.retryable && e.detail.contains("stale-attempt"), "{e:?}");
}

#[tokio::test]
async fn the_number_of_a_session_is_resent_unchanged_when_the_same_report_is_retried() {
    let (t, s) = store();
    t.fail("connection reset");
    assert!(s.report_epoch("sess-3", 3).await.is_err());
    t.reply(200, fixture("session-ack.json"));
    s.report_epoch("sess-3", 3).await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs[0].1["number"], 3);
    assert_eq!(reqs[0].1, reqs[1].1, "the same report, byte for byte");
}

#[tokio::test]
async fn a_claim_without_an_id_sends_no_message_id_and_reads_what_the_agent_holds() {
    let (t, s) = store();
    t.reply(
        200,
        json!({"epochNo": 3, "held": 0, "deliveryUnknown": 0, "admission": "open"}),
    );
    s.report_epoch("sess-3", 3).await.unwrap();
    let mut reply = fixture("claim-response-claimed.json");
    reply["redelivered"] = json!(true);
    reply["messages"][1]["epochNo"] = json!(2);
    t.reply(200, reply);
    let held = s.held("sess-3").await.unwrap();
    let (path, body) = t.requests().remove(1);
    assert_eq!(path, "/internal/message-claim");
    assert!(
        body.get("messageId").is_none() && body.get("limit").is_none(),
        "{body}"
    );
    serde_json::from_value::<ClaimRequest>(body).unwrap();
    assert_eq!(held.len(), 2);
    assert!(held.iter().all(|h| h.redelivered));
    assert_eq!(held[0].message.id, MSG);
    assert!(
        held[0].current_session,
        "its session number is the one reported"
    );
    assert!(
        !held[1].current_session,
        "a claim of an older session is not of the current one"
    );
    // the outcomes that follow carry the attempts that were read
    t.reply(200, fixture("outcome-response.json"));
    s.release(MSG).await.unwrap();
    assert_eq!(t.requests()[2].1["attempt"], 1);
}

#[tokio::test]
async fn nothing_held_and_a_refused_claim_without_an_id_are_an_empty_list() {
    let (t, s) = store();
    t.reply(200, fixture("claim-response-nothing.json"));
    assert!(s.held("sess-3").await.unwrap().is_empty());
    t.reply(200, fixture("claim-response-refused.json"));
    assert!(s.held("sess-3").await.unwrap().is_empty());
    t.reply(
        409,
        json!({"code": "stale-generation", "message": "replaced"}),
    );
    assert!(s.held("sess-3").await.is_err());
    assert!(s.unavailable(), "a refused generation stays out");
}

#[tokio::test]
async fn a_rejection_after_a_release_carries_the_attempt_of_its_last_claim() {
    let (t, s) = store();
    t.reply(200, fixture("claim-response-claimed.json"));
    s.claim(MSG, "sess-3").await.unwrap();
    t.reply(200, fixture("outcome-response.json"));
    s.release(MSG).await.unwrap();
    t.reply(200, fixture("outcome-response.json"));
    s.reject(MSG, RejectReason::ContinuationLimit)
        .await
        .unwrap();
    assert_eq!(t.requests()[2].1["attempt"], 1, "{:?}", t.requests()[2]);
}

#[test]
fn a_new_generation_is_a_fresh_ulid_every_time() {
    use agentd::direct::messages::http::new_generation;
    let (a, b) = (new_generation(), new_generation());
    assert_eq!((a.len(), b.len()), (26, 26));
    assert_ne!(a, b);
}
