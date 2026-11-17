//! The worker's call to `POST /internal/message-history`, the read of the run's delivered messages (`HttpMessageStore::delivered`).
//!
//! The transport is a fake that records every request and answers from a script, so no network is used. What each answer means to the
//! worker: an HTTP 200 with the answer is the history; an explicit answer that the route does not exist is "no such route" and says
//! nothing about the four message routes; every other answer is a failure, also an HTTP 200 whose body is not the answer. No failure
//! is an empty history.
#![allow(clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::direct::messages::earlier::Earlier;
use agentd::direct::messages::http::{HttpMessageStore, Transport};
use agentd::direct::messages::{HistoryError, MessageStore};
use harness_protocol::messages::HistoryRequest;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const GEN: &str = "01M5A0000000000000000000GE";
const PATH: &str = "/internal/message-history";

fn mid(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

/// A transport that answers from a script and keeps what it was asked.
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Result<(u16, String), String>>>,
    seen: Mutex<Vec<(String, Value)>>,
}

impl Script {
    fn reply(&self, status: u16, body: Value) {
        self.raw(status, &body.to_string());
    }

    fn raw(&self, status: u16, body: &str) {
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

#[tokio::test]
async fn b857_the_question_names_the_run_and_the_agent_and_the_answer_is_read_in_its_order() {
    let (t, s) = store();
    // An answer of a later service: members this worker does not know, on the answer and on a message.
    t.reply(
        200,
        json!({"messages": [
            {"messageId": mid(1), "seq": 1, "deliveredTurn": 2, "text": "use tabs, not spaces", "deliveredAt": "2026-11-17T10:00:00Z"},
            {"messageId": mid(3), "seq": 3, "text": "name the file REPORT.md"},
        ], "cut": 2, "asOf": 17}),
    );
    let got = s.delivered().await.unwrap();
    assert_eq!(
        got.messages
            .iter()
            .map(|m| (m.id.clone(), m.seq, m.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (mid(1), 1, "use tabs, not spaces"),
            (mid(3), 3, "name the file REPORT.md")
        ]
    );
    assert_eq!(got.cut, 2);
    let (path, body) = t.requests().remove(0);
    assert_eq!(path, PATH);
    assert_eq!(
        body,
        json!({"runId": RUN, "agentId": "direct"}),
        "no generation, no session and no text: the body is the run and the agent"
    );
    // the body is the type of the protocol, which takes no other member
    let typed: HistoryRequest = serde_json::from_value(body).unwrap();
    assert_eq!(
        (typed.run_id.as_str(), typed.agent_id.as_str()),
        (RUN, "direct")
    );
    // an answer with no message is an empty history
    t.reply(200, json!({"messages": [], "cut": 0}));
    assert!(s.delivered().await.unwrap().messages.is_empty());
}

#[tokio::test]
async fn b857_an_answer_that_the_route_does_not_exist_is_no_route_and_the_message_routes_stay_served()
 {
    let (t, s) = store();
    // worker-api of a release before the route, behind a gateway that has the resource
    t.reply(
        404,
        json!({"code": "not_found", "message": "no such internal route"}),
    );
    // a 404 that carries no error body of the service: something in front of worker-api does not know the path
    t.raw(404, "Not Found");
    // the gateway's answer to a resource it does not have: no error body of the service
    t.raw(403, r#"{"message":"Missing Authentication Token"}"#);
    // the same sentence under a code, as a stand-in of the gateway writes it
    t.reply(
        403,
        json!({"code": "forbidden", "message": "Missing Authentication Token"}),
    );
    // the path belongs to something that takes no POST there
    t.raw(405, "");
    for status in [404, 404, 403, 403, 405] {
        let err = s.delivered().await.unwrap_err();
        assert!(matches!(err, HistoryError::NoRoute(_)), "{status}: {err:?}");
        assert!(err.to_string().contains(&status.to_string()), "{err}");
    }
    assert!(
        !s.plane_has_no_routes() && !s.unavailable(),
        "a service from before the read serves the four message routes: the module stays in the run"
    );
}

#[tokio::test]
async fn b857_a_refusal_a_failure_and_a_malformed_answer_are_failures_and_never_an_empty_history() {
    let (t, s) = store();
    // refusals: the caller's credentials, another run's session, a fenced session, a body the service does not take
    t.reply(403, json!({"code": "not_worker_session", "message": "the caller is not a harness worker session"}));
    t.reply(
        403,
        json!({"code": "caller_mismatch", "message": "the request names another run or agent"}),
    );
    t.reply(409, json!({"code": "stale-generation", "message": "this session is not the one the run is in now"}));
    t.reply(
        400,
        json!({"code": "bad_request", "message": "invalid body"}),
    );
    // answers that look like "no such route" and are none: a 404 of the service for something else and a refusal that only holds
    // the gateway's sentence among other words
    t.reply(
        404,
        json!({"code": "unknown_agent", "message": "no agent item for the caller"}),
    );
    t.reply(
        403,
        json!({"code": "caller_mismatch", "message": "refused; this is not Missing Authentication Token"}),
    );
    // the store or the service failed, or throttled
    t.reply(
        503,
        json!({"code": "store_unavailable", "message": "the harness store is unavailable"}),
    );
    t.raw(500, "Internal Server Error");
    t.reply(429, json!({"message": "Too Many Requests"}));
    // nothing came back
    t.fail("connection reset");
    // HTTP 200 with a body that is not the answer
    t.raw(200, "");
    t.raw(200, "<html>ok</html>");
    t.reply(200, json!({}));
    t.reply(200, json!({"cut": 0}));
    t.reply(200, json!({"messages": "none"}));
    t.reply(200, json!({"messages": [{"messageId": mid(1), "seq": 1}]}));
    t.reply(200, json!({"messages": [{"seq": 1, "text": "words"}]}));
    for n in 0..17 {
        let err = s.delivered().await.unwrap_err();
        assert!(
            matches!(err, HistoryError::Failed(_)),
            "answer {n}: {err:?}"
        );
    }
    assert_eq!(t.requests().len(), 17);
    assert!(
        !s.unavailable(),
        "a failed read of the history takes the module out of nothing"
    );
}

/// The answer's messages that were handed to an earlier session with no report of their delivery are read, and the record a session
/// starts with holds every message once, by its id: an id the answer names twice is taken as delivered. Two messages with the same
/// words and ids of their own are two messages, and each is shown in its part.
#[tokio::test]
async fn b857_the_answers_unconfirmed_messages_are_read_and_no_message_is_taken_twice() {
    let (t, s) = store();
    t.reply(
        200,
        json!({"messages": [
            {"messageId": mid(1), "seq": 1, "deliveredTurn": 2, "text": "use tabs, not spaces"},
            {"messageId": mid(4), "seq": 4, "deliveredTurn": 3, "text": "skip the Windows build"},
        ], "cut": 0, "unconfirmed": [
            {"messageId": mid(1), "seq": 1, "text": "use tabs, not spaces"},
            {"messageId": mid(2), "seq": 2, "text": "name the file REPORT.md", "state": "delivery-unknown"},
            {"messageId": mid(3), "seq": 3, "text": "skip the Windows build"},
        ]}),
    );
    let plain = |t: &str| t.to_string();
    let e = Earlier::from_history(s.delivered().await.unwrap(), &plain);
    let block = e.block(&plain);
    for text in ["use tabs, not spaces", "name the file REPORT.md"] {
        assert_eq!(block.matches(text).count(), 1, "{text}: {block}");
    }
    // The user wrote these words in two messages: one was delivered, one has no report. Neither is dropped for the other.
    assert_eq!(
        block.matches("skip the Windows build").count(),
        2,
        "{block}"
    );
    assert_eq!(
        e.messages
            .iter()
            .map(|m| (m.id.clone().unwrap(), m.unconfirmed))
            .collect::<Vec<_>>(),
        [
            (mid(1), false),
            (mid(4), false),
            (mid(2), true),
            (mid(3), true)
        ],
        "the delivered messages in the answer's order, then the ones without a report"
    );
    // An answer of a service from before the member is the answer it was.
    t.reply(200, json!({"messages": [], "cut": 0}));
    assert!(Earlier::from_history(s.delivered().await.unwrap(), &plain).is_empty());
    // a message of the member without its text is not the answer
    t.reply(
        200,
        json!({"messages": [], "unconfirmed": [{"messageId": mid(2), "seq": 2}]}),
    );
    assert!(matches!(
        s.delivered().await.unwrap_err(),
        HistoryError::Failed(_)
    ));
}
