//! The typed calls of `ham::message_routes` against a scripted plane: the exact request each makes (method, path, headers, body), the
//! retry of a send with the same body, the mapping of an error answer, and the id the client mints.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, ApiError, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham::message_routes::mint_message_id;
use ham_core::messages::{plain, ulid_from};
use harness_protocol::common::is_ulid;
use harness_protocol::messages::{MessageMode, MessageState, SendOutcome};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const MSG: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

type Seen = (String, String, Vec<(String, String)>, Vec<u8>);

#[derive(Default)]
struct Scripted {
    replies: Mutex<VecDeque<Result<(u16, Value), TransportError>>>,
    seen: Mutex<Vec<Seen>>,
}

impl Scripted {
    fn with(replies: Vec<Result<(u16, Value), TransportError>>) -> Arc<Scripted> {
        Arc::new(Scripted {
            replies: Mutex::new(replies.into()),
            seen: Mutex::default(),
        })
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.seen
            .lock()
            .unwrap()
            .push((method.into(), path.into(), headers.to_vec(), body));
        let r = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("an unscripted call");
        r.map(|(status, v)| Response {
            status,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&v).unwrap(),
        })
    }
    async fn fetch(
        &self,
        _: &str,
        _: Option<(u64, u64)>,
        _: Option<&str>,
    ) -> Result<Response, TransportError> {
        unreachable!("messages never fetch an object")
    }
}

fn api(p: &Arc<Scripted>) -> Api<Scripted> {
    Api::new(p.clone()).with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(1),
    })
}

fn view(state: &str) -> Value {
    json!({"messageId": MSG, "seq": 1, "mode": "next", "state": state, "text": "hello", "epochNo": 1,
           "acceptedAt": "2026-10-16T14:02:11Z", "updatedAt": "2026-10-16T14:02:11Z"})
}

#[tokio::test]
async fn a_send_posts_the_message_as_typed_with_json_headers_and_no_idempotency_header() {
    let p = Scripted::with(vec![Ok((
        202,
        json!({"outcome": "accepted", "message": view("pending")}),
    ))]);
    let req = plain(MSG.into(), "  hello\n", MessageMode::Now);
    let r = api(&p).send_message(RUN, &req).await.unwrap();
    assert_eq!(
        (r.outcome, r.message.state),
        (SendOutcome::Accepted, MessageState::Pending)
    );
    let seen = p.seen();
    assert_eq!(seen.len(), 1);
    let (method, path, headers, body) = &seen[0];
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", format!("/v1/runs/{RUN}/messages").as_str())
    );
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/json")
    );
    assert!(
        !headers.iter().any(|(k, _)| k == "idempotency-key"),
        "the message id is the key"
    );
    let b: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(
        b,
        json!({"messageId": MSG, "text": "  hello\n", "mode": "now"})
    );
}

#[tokio::test]
async fn a_send_that_met_a_5xx_is_sent_again_with_the_same_body_and_the_stored_message_comes_back()
{
    let p = Scripted::with(vec![
        Ok((503, json!({"code": "unavailable", "message": "retry"}))),
        Err(TransportError::Network("connection reset".into())),
        Ok((
            200,
            json!({"outcome": "already-accepted", "message": view("dispatching")}),
        )),
    ]);
    let r = api(&p)
        .send_message(RUN, &plain(MSG.into(), "hello", MessageMode::Next))
        .await
        .unwrap();
    assert_eq!(
        (r.outcome, r.message.state),
        (SendOutcome::AlreadyAccepted, MessageState::Dispatching)
    );
    let seen = p.seen();
    assert_eq!(seen.len(), 3);
    assert!(
        seen.iter().all(|s| s.3 == seen[0].3),
        "every try carries the same bytes"
    );
}

#[tokio::test]
async fn a_definitive_refusal_is_not_retried_and_keeps_its_code() {
    let p = Scripted::with(vec![Ok((
        409,
        json!({"code": "run-sealed", "message": "the run is ending"}),
    ))]);
    let e = api(&p)
        .send_message(RUN, &plain(MSG.into(), "hello", MessageMode::Next))
        .await
        .unwrap_err();
    match e {
        ApiError::Http { status, error } => {
            assert_eq!((status, error.code.as_str()), (409, "run-sealed"))
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(p.seen().len(), 1);
}

#[tokio::test]
async fn the_reads_and_the_withdrawal_use_their_own_paths() {
    let page = json!({"runId": RUN, "messages": [view("pending")], "admission": "open", "epoch": "sess-1", "epochNo": 1, "watermark": 1});
    let p = Scripted::with(vec![
        Ok((200, page)),
        Ok((200, json!({"message": view("pending")}))),
        Ok((200, json!({"message": view("withdrawn")}))),
    ]);
    let a = api(&p);
    let list = a.list_messages(RUN).await.unwrap();
    assert_eq!(
        (list.messages.len(), list.epoch.as_deref(), list.watermark),
        (1, Some("sess-1"), 1)
    );
    assert_eq!(
        a.get_message(RUN, MSG).await.unwrap().message.state,
        MessageState::Pending
    );
    assert_eq!(
        a.withdraw_message(RUN, MSG).await.unwrap().message.state,
        MessageState::Withdrawn
    );
    let seen = p.seen();
    let paths: Vec<(&str, String)> = seen.iter().map(|s| (s.0.as_str(), s.1.clone())).collect();
    assert_eq!(
        paths,
        [
            ("GET", format!("/v1/runs/{RUN}/messages")),
            ("GET", format!("/v1/runs/{RUN}/messages/{MSG}")),
            ("POST", format!("/v1/runs/{RUN}/messages/{MSG}/withdraw")),
        ]
    );
    assert_eq!(seen[2].3, b"{}".to_vec());
}

#[test]
fn a_minted_id_is_a_fresh_ulid_that_sorts_with_the_clock() {
    let a = mint_message_id();
    let b = mint_message_id();
    assert!(is_ulid(&a) && is_ulid(&b));
    assert_ne!(a, b, "ten random bytes differ");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    // the time part of the id is the clock to within a few seconds
    assert!(a[..10] >= ulid_from(now_ms - 5_000, [0; 10])[..10]);
    assert!(a[..10] <= ulid_from(now_ms + 5_000, [0; 10])[..10]);
}
