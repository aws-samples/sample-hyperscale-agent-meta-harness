//! The answer flow of `ham say`, `new-session`, `try-model` and `finish`: finding the run's open
//! question from its events, checking the answer against what the question offers, one POST, the wait for Cline's acknowledgement,
//! the one resend, and the plain result. The control plane is a scripted one so the waits can be short and every refusal exact.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::commands::{
    Ack, AnswerError, AnswerOptions, Timing, Want, answer, answered_json, answered_text,
};
use ham::http::{ControlPlane, Response, TransportError};
use harness_protocol::direct::InstructAction;
use harness_protocol::envelope::Envelope;
use harness_protocol::store::event_shard;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn progress(n: u32, value: Value) -> Value {
    json!({"schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-15T13:41:07Z",
           "type": "turn.progress", "authority": "agent", "payload": {"kind": "inline", "value": value}})
}

fn needs_input(n: u32, extra: Value) -> Value {
    let mut v = json!({
        "turn": 1, "kind": "needs-input", "text": "Needs your input", "requestId": id(n), "n": 1, "reason": "engine_stop",
        "title": "Cline stopped its own prompt", "stopOutcome": "stopped_engine_cancelled",
        "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "runS": 412, "sessions": 1},
        "lastMessage": "I will keep polling the build.", "files": ["PROGRESS.md"], "filesTotal": 1, "blocking": true,
        "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
        "models": [{"id": "us.anthropic.claude-opus-5-5", "name": "Claude Opus 5.5", "contextTokens": 1000000},
                   {"id": "global.anthropic.claude-sonnet-5-5", "name": "Claude Sonnet 5.5", "contextTokens": 1000000}],
        "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2099-10-15T14:11:07Z"
    });
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    progress(n, v)
}

fn ack(n: u32, request: u32, kind: &str, extra: Value) -> Value {
    let mut v = json!({"turn": 1, "kind": kind, "text": "t", "requestId": id(request)});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    progress(n, v)
}

/// A control plane that serves event reads from a list and answers `POST .../input` from a queue, and can append an event after the
/// Nth POST (the harness side acknowledging).
#[derive(Default)]
struct Script {
    events: Mutex<Vec<Value>>,
    posts: Mutex<Vec<Value>>,
    replies: Mutex<VecDeque<(u16, Value)>>,
    after_post: Mutex<BTreeMap<usize, Vec<Value>>>,
    reads_after_last_post: Mutex<usize>,
    /// The next POSTs fail on the wire (a dropped connection) this many times.
    post_drops: Mutex<usize>,
}

impl Script {
    fn with(events: Vec<Value>) -> Arc<Self> {
        let s = Self::default();
        *s.events.lock().unwrap() = events;
        Arc::new(s)
    }

    fn reply(&self, status: u16, body: Value) {
        self.replies.lock().unwrap().push_back((status, body));
    }

    fn ack_after_post(&self, n: usize, events: Vec<Value>) {
        self.after_post.lock().unwrap().insert(n, events);
    }

    fn posts(&self) -> Vec<Value> {
        self.posts.lock().unwrap().clone()
    }
}

impl ControlPlane for Script {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let json = |status: u16, v: Value| Response {
            status,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&v).unwrap(),
        };
        if method == "POST" && route.ends_with("/input") {
            let mut drops = self.post_drops.lock().unwrap();
            if *drops > 0 {
                *drops -= 1;
                return Err(TransportError::Network("connection reset".into()));
            }
            drop(drops);
            let mut posts = self.posts.lock().unwrap();
            posts.push(serde_json::from_slice(&body).unwrap());
            let n = posts.len();
            *self.reads_after_last_post.lock().unwrap() = 0;
            let next = self.replies.lock().unwrap().pop_front();
            let (status, reply) = next.unwrap_or_else(|| {
                (
                    202,
                    json!({"runId": RUN, "requestId": posts[0]["requestId"], "state": "queued"}),
                )
            });
            if (200..300).contains(&status)
                && let Some(more) = self.after_post.lock().unwrap().get(&n)
            {
                self.events.lock().unwrap().extend(more.iter().cloned());
            }
            return Ok(json(status, reply));
        }
        if method == "GET" && route.ends_with("/events") {
            *self.reads_after_last_post.lock().unwrap() += 1;
            let q: BTreeMap<&str, &str> = query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .collect();
            let shard: u8 = q["shard"].parse().unwrap();
            let after = q.get("after").copied();
            let mut mine: Vec<Value> = self
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| {
                    let env: Envelope = serde_json::from_value((*e).clone()).unwrap();
                    event_shard(env.authority, env.agent_id.as_deref()) == shard
                })
                .filter(|e| after.is_none_or(|a| e["eventId"].as_str().unwrap() > a))
                .cloned()
                .collect();
            mine.sort_by(|a, b| a["eventId"].as_str().cmp(&b["eventId"].as_str()));
            return Ok(json(200, json!({"events": mine})));
        }
        Ok(json(
            404,
            json!({"code": "not-found", "message": "no such route"}),
        ))
    }

    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Refused("no objects".into()))
    }
}

fn api(plane: &Arc<Script>) -> Api<Script> {
    Api::new(plane.clone()).with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(1),
    })
}

/// Short waits: 400 ms for the acknowledgement, a resend at 150 ms, a read every 20 ms.
fn opts() -> AnswerOptions {
    AnswerOptions {
        timing: Timing {
            ack_wait: Duration::from_millis(400),
            resend_after: Duration::from_millis(150),
            poll: Duration::from_millis(20),
        },
        ..AnswerOptions::default()
    }
}

fn say(text: &str) -> Want {
    Want::Say(text.to_string())
}

#[tokio::test]
async fn say_sends_the_message_to_the_open_question_and_reports_the_acknowledgement() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![ack(
            11,
            10,
            "input-received",
            json!({"action": "continue_with_prompt", "origin": "user", "textBytes": 12, "waitedS": 5}),
        )],
    );
    let a = answer(&api(&plane), RUN, &say("Poll the log."), &opts())
        .await
        .unwrap();
    assert_eq!(a.request_id, id(10));
    assert_eq!(a.action, InstructAction::ContinueWithPrompt);
    assert_eq!(a.ack, Ack::Received { text_bytes: 12 });
    assert_eq!(a.state, "queued");
    assert_eq!(
        plane.posts(),
        vec![
            json!({"requestId": id(10), "action": "continue_with_prompt", "text": "Poll the log."})
        ],
        "exactly the answer, with no model and no origin"
    );
    assert_eq!(
        answered_text(&a),
        ("Cline took your message (same session).".to_string(), 0)
    );
}

#[tokio::test]
async fn new_session_without_text_sends_no_text() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![ack(
            11,
            10,
            "input-received",
            json!({"action": "new_session_with_prompt", "textBytes": 0}),
        )],
    );
    let a = answer(&api(&plane), RUN, &Want::NewSession(None), &opts())
        .await
        .unwrap();
    assert_eq!(
        plane.posts(),
        vec![json!({"requestId": id(10), "action": "new_session_with_prompt"})]
    );
    assert_eq!(
        answered_text(&a),
        (
            "Cline took your request: a fresh session starts on the same files.".to_string(),
            0
        )
    );
    // A blank text is no text.
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![ack(11, 10, "input-received", json!({"textBytes": 0}))],
    );
    answer(
        &api(&plane),
        RUN,
        &Want::NewSession(Some("  ".into())),
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(plane.posts()[0].get("text"), None);
}

#[tokio::test]
async fn try_model_names_the_offered_route_of_the_alias_and_says_what_cline_received() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![ack(
            11,
            10,
            "input-received",
            json!({"action": "try_model", "textBytes": 0}),
        )],
    );
    let want = Want::TryModel {
        model: "opus".into(),
        text: None,
    };
    let a = answer(&api(&plane), RUN, &want, &opts()).await.unwrap();
    assert_eq!(
        plane.posts(),
        vec![
            json!({"requestId": id(10), "action": "try_model", "model": "us.anthropic.claude-opus-5-5"})
        ]
    );
    assert_eq!(
        answered_text(&a),
        (
            "Switched to Claude Opus 5.5. Cline took the fixed continue message.".to_string(),
            0
        )
    );
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![ack(
            11,
            10,
            "input-received",
            json!({"action": "try_model", "textBytes": 9}),
        )],
    );
    let want = Want::TryModel {
        model: "sonnet".into(),
        text: Some("Carry on.".into()),
    };
    let a = answer(&api(&plane), RUN, &want, &opts()).await.unwrap();
    assert_eq!(plane.posts()[0]["text"], "Carry on.");
    assert_eq!(
        answered_text(&a).0,
        "Switched to Claude Sonnet 5.5. Cline took your message."
    );
}

#[tokio::test]
async fn finish_sends_neither_text_nor_model() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![
            ack(
                11,
                10,
                "input-received",
                json!({"action": "finish", "textBytes": 0}),
            ),
            ack(12, 10, "input-closed", json!({"how": "finish"})),
        ],
    );
    let a = answer(&api(&plane), RUN, &Want::Finish, &opts())
        .await
        .unwrap();
    assert_eq!(
        plane.posts(),
        vec![json!({"requestId": id(10), "action": "finish"})]
    );
    assert_eq!(
        a.ack,
        Ack::Received { text_bytes: 0 },
        "the first acknowledgement counts, the close after it does not undo it"
    );
    assert_eq!(
        answered_text(&a),
        ("The run will finish with what exists.".to_string(), 0)
    );
}

#[tokio::test]
async fn a_run_that_asked_nothing_is_not_waiting_and_nothing_is_sent() {
    let plane = Script::with(vec![]);
    let e = answer(&api(&plane), RUN, &say("hi"), &opts())
        .await
        .unwrap_err();
    assert!(
        matches!(&e, AnswerError::NotWaiting(m) if m == &format!("run {RUN} is not waiting for you. Cline has not asked it a question.")),
        "{e}"
    );
    assert!(plane.posts().is_empty());
}

#[tokio::test]
async fn a_closed_question_is_named_by_how_it_ended() {
    let plane = Script::with(vec![
        needs_input(10, json!({})),
        ack(11, 10, "input-received", json!({"action": "finish"})),
        ack(12, 10, "input-closed", json!({"how": "finish"})),
    ]);
    let e = answer(&api(&plane), RUN, &say("again"), &opts())
        .await
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        format!("run {RUN} is not waiting for you. Its last question was answered with finish.")
    );
    assert!(plane.posts().is_empty());
    let plane = Script::with(vec![
        needs_input(10, json!({})),
        ack(11, 10, "input-closed", json!({"how": "expired"})),
    ]);
    let e = answer(&api(&plane), RUN, &Want::Finish, &opts())
        .await
        .unwrap_err();
    assert!(
        e.to_string()
            .ends_with("Its last question was closed when nobody answered in time."),
        "{e}"
    );
}

#[tokio::test]
async fn the_outage_notice_is_not_a_question_to_answer() {
    let plane = Script::with(vec![needs_input(
        10,
        json!({"reason": "provider_outage", "blocking": false, "options": [], "models": [], "canContinue": false}),
    )]);
    let e = answer(&api(&plane), RUN, &Want::Finish, &opts())
        .await
        .unwrap_err();
    assert!(matches!(e, AnswerError::NotWaiting(_)), "{e}");
    assert!(plane.posts().is_empty());
}

#[tokio::test]
async fn an_action_the_question_does_not_offer_is_refused_before_anything_is_sent() {
    // A dead session: the message and the model switch are withdrawn.
    let plane = Script::with(vec![needs_input(
        10,
        json!({"reason": "engine_died", "options": ["new_session_with_prompt", "finish"], "canContinue": false}),
    )]);
    let e = answer(&api(&plane), RUN, &say("hi"), &opts())
        .await
        .unwrap_err();
    let text = e.to_string();
    assert!(matches!(e, AnswerError::Refused(_)));
    assert!(
        text.contains("cannot answer with a message to the same session")
            && text.contains("a fresh session")
            && text.contains("finish"),
        "{text}"
    );
    assert!(plane.posts().is_empty());
    let want = Want::TryModel {
        model: "opus".into(),
        text: None,
    };
    assert!(matches!(
        answer(&api(&plane), RUN, &want, &opts()).await.unwrap_err(),
        AnswerError::Refused(_)
    ));
}

#[tokio::test]
async fn a_model_the_question_does_not_offer_is_refused_with_the_choices() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    for bad in ["sol", "gpt-9", ""] {
        let want = Want::TryModel {
            model: bad.into(),
            text: None,
        };
        let e = answer(&api(&plane), RUN, &want, &opts()).await.unwrap_err();
        assert!(matches!(e, AnswerError::Refused(_)), "{bad}: {e}");
        assert!(
            e.to_string().contains("opus") && e.to_string().contains("sonnet"),
            "{bad}: {e}"
        );
    }
    assert!(plane.posts().is_empty());
}

#[tokio::test]
async fn a_bad_message_is_refused_locally() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    for bad in [String::new(), "  ".to_string(), "x".repeat(262_145)] {
        let e = answer(&api(&plane), RUN, &Want::Say(bad.clone()), &opts())
            .await
            .unwrap_err();
        assert!(
            matches!(e, AnswerError::Refused(_)),
            "{} chars: {e}",
            bad.len()
        );
    }
    assert!(plane.posts().is_empty());
}

#[tokio::test]
async fn the_servers_refusal_is_shown_in_its_own_words() {
    for (status, code, message) in [
        (
            409,
            "already-answered",
            "this question was already answered with finish",
        ),
        (
            410,
            "request-expired",
            "the answer window closed at 2026-10-15T14:11:07Z",
        ),
        (
            404,
            "no-such-request",
            "the run has no such question; read its events again",
        ),
        (
            409,
            "run-finished",
            "run 01M4ZHFBV5SDX7J92E6YRH4QTW is finished",
        ),
    ] {
        let plane = Script::with(vec![needs_input(10, json!({}))]);
        plane.reply(status, json!({"code": code, "message": message}));
        let e = answer(&api(&plane), RUN, &Want::Finish, &opts())
            .await
            .unwrap_err();
        assert!(matches!(e, AnswerError::Api(_)), "{e}");
        assert_eq!(e.to_string(), message);
        assert_eq!(plane.posts().len(), 1, "a 4xx is not retried");
    }
}

#[tokio::test]
async fn a_busy_service_is_retried_with_the_same_answer() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.reply(503, json!({"code": "unavailable", "message": "retry"}));
    plane.reply(429, json!({"code": "throttled", "message": "slow down"}));
    plane.ack_after_post(
        3,
        vec![ack(11, 10, "input-received", json!({"textBytes": 0}))],
    );
    let a = answer(&api(&plane), RUN, &Want::Finish, &opts())
        .await
        .unwrap();
    assert_eq!(a.ack, Ack::Received { text_bytes: 0 });
    let posts = plane.posts();
    assert_eq!(posts.len(), 3);
    assert!(
        posts.iter().all(|p| *p == posts[0]),
        "the same answer each time"
    );
}

#[tokio::test]
async fn an_answer_nobody_acknowledges_is_sent_once_more_and_then_reported() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    let a = answer(&api(&plane), RUN, &say("hi"), &opts())
        .await
        .unwrap();
    assert_eq!(a.ack, Ack::Unacknowledged);
    let posts = plane.posts();
    assert_eq!(posts.len(), 2, "one send and one resend");
    assert_eq!(posts[0], posts[1]);
    let (text, code) = answered_text(&a);
    assert_eq!(code, 1);
    assert_eq!(
        text,
        format!("queued, not acknowledged yet: the run may have ended; run: ham watch {RUN}")
    );
}

#[tokio::test]
async fn an_acknowledgement_after_the_resend_counts() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.reply(
        202,
        json!({"runId": RUN, "requestId": id(10), "state": "queued"}),
    );
    plane.reply(
        202,
        json!({"runId": RUN, "requestId": id(10), "state": "re-queued"}),
    );
    plane.ack_after_post(
        2,
        vec![ack(11, 10, "input-received", json!({"textBytes": 3}))],
    );
    let a = answer(&api(&plane), RUN, &say("hi"), &opts())
        .await
        .unwrap();
    assert_eq!(a.ack, Ack::Received { text_bytes: 3 });
    assert_eq!(plane.posts().len(), 2);
}

#[tokio::test]
async fn a_refused_resend_does_not_hide_a_late_acknowledgement() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.reply(
        202,
        json!({"runId": RUN, "requestId": id(10), "state": "queued"}),
    );
    plane.reply(410, json!({"code": "request-expired", "message": "closed"}));
    // The harness acknowledges after the resend (sent at 150 ms) was refused; the events decide.
    let late = ack(11, 10, "input-received", json!({"textBytes": 0}));
    let writer_plane = plane.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        writer_plane.events.lock().unwrap().push(late);
    });
    let a = answer(&api(&plane), RUN, &say("hi"), &opts())
        .await
        .unwrap();
    writer.await.unwrap();
    assert_eq!(a.ack, Ack::Received { text_bytes: 0 });
    assert_eq!(plane.posts().len(), 2);
}

/// The finish was taken, so the command is a success whichever of the two ids is smaller.
#[tokio::test]
async fn a_finish_whose_close_sorts_before_its_receipt_is_still_received() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![
            // Emitted second, sorts first.
            ack(11, 10, "input-closed", json!({"how": "finish"})),
            // Emitted first, sorts second.
            ack(
                12,
                10,
                "input-received",
                json!({"action": "finish", "textBytes": 0}),
            ),
        ],
    );
    let a = answer(&api(&plane), RUN, &Want::Finish, &opts())
        .await
        .unwrap();
    assert_eq!(
        a.ack,
        Ack::Received { text_bytes: 0 },
        "the receipt is in the page; the close of a finish does not undo it"
    );
    assert_eq!(
        answered_text(&a),
        ("The run will finish with what exists.".to_string(), 0)
    );
    assert_eq!(answered_json(&a)["result"], "received");
}

/// Every ordering of `items`.
fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut all = Vec::new();
    for i in 0..items.len() {
        let mut rest = items.to_vec();
        let first = rest.remove(i);
        for mut tail in permutations(&rest) {
            tail.insert(0, first.clone());
            all.push(tail);
        }
    }
    all
}

/// The four commands with the action each one sends.
fn verbs() -> Vec<(Want, &'static str)> {
    vec![
        (say("Poll the log."), "continue_with_prompt"),
        (Want::NewSession(None), "new_session_with_prompt"),
        (
            Want::TryModel {
                model: "opus".into(),
                text: None,
            },
            "try_model",
        ),
        (Want::Finish, "finish"),
    ]
}

/// The events one acknowledgement test appends after the answer, in the order `order` names them, with ids that grow in that order:
/// the question's receipt, its close, a close of another question, and an event that acknowledges nothing.
fn after_the_answer(order: &[&str], action: &str, how: &str) -> Vec<Value> {
    order
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            let n = 20 + u32::try_from(i).unwrap();
            match *kind {
                "received" => ack(
                    n,
                    10,
                    "input-received",
                    json!({"action": action, "textBytes": 0}),
                ),
                "ignored" => ack(n, 10, "input-ignored", json!({"why": "bad-model"})),
                "closed" => ack(n, 10, "input-closed", json!({"how": how})),
                "other-closed" => ack(n, 9, "input-closed", json!({"how": "expired"})),
                _ => progress(n, json!({"turn": 1, "kind": "message", "text": "working"})),
            }
        })
        .collect()
}

/// The ULIDs of two events made in one millisecond sort in either order, so the page may list the close of a question before its
/// receipt. For every command and every order of the events, the receipt is the answer.
#[tokio::test]
async fn the_receipt_wins_in_every_order_of_the_page_for_every_command() {
    for (want, action) in verbs() {
        let how = if action == "finish" {
            "finish"
        } else {
            "expired"
        };
        for order in permutations(&["received", "closed", "other-closed", "unrelated"]) {
            let plane = Script::with(vec![needs_input(10, json!({}))]);
            plane.ack_after_post(1, after_the_answer(&order, action, how));
            let a = answer(&api(&plane), RUN, &want, &opts()).await.unwrap();
            assert_eq!(a.ack, Ack::Received { text_bytes: 0 }, "{action} {order:?}");
            assert_eq!(answered_text(&a).1, 0, "{action} {order:?}");
            assert_eq!(
                answered_json(&a)["result"],
                "received",
                "{action} {order:?}"
            );
        }
    }
}

/// A close with no receipt is a lost answer, also for a finish: exit 1 and the close's own words.
#[tokio::test]
async fn a_close_without_a_receipt_stays_an_error_for_every_command() {
    for (want, action) in verbs() {
        let how = if action == "finish" {
            "finish"
        } else {
            "expired"
        };
        for order in permutations(&["closed", "other-closed", "unrelated"]) {
            let plane = Script::with(vec![needs_input(10, json!({}))]);
            plane.ack_after_post(1, after_the_answer(&order, action, how));
            let a = answer(&api(&plane), RUN, &want, &opts()).await.unwrap();
            assert_eq!(a.ack, Ack::Closed { how: how.into() }, "{action} {order:?}");
            assert_eq!(answered_text(&a).1, 1, "{action} {order:?}");
            assert_eq!(answered_json(&a)["result"], "closed", "{action} {order:?}");
        }
    }
}

/// An `input-ignored` of the question says why Cline did not take the answer, and wins over the close that follows it; a later receipt
/// (the user's corrected answer) wins over both.
#[tokio::test]
async fn ignored_wins_over_closed_and_received_wins_over_both_in_every_order() {
    for (want, action) in verbs() {
        let how = if action == "finish" {
            "finish"
        } else {
            "expired"
        };
        for order in permutations(&["ignored", "closed"]) {
            let plane = Script::with(vec![needs_input(10, json!({}))]);
            plane.ack_after_post(1, after_the_answer(&order, action, how));
            let a = answer(&api(&plane), RUN, &want, &opts()).await.unwrap();
            assert_eq!(
                a.ack,
                Ack::Ignored {
                    why: "bad-model".into()
                },
                "{action} {order:?}"
            );
            assert_eq!(answered_text(&a).1, 1, "{action} {order:?}");
        }
        for order in permutations(&["ignored", "closed", "received"]) {
            let plane = Script::with(vec![needs_input(10, json!({}))]);
            plane.ack_after_post(1, after_the_answer(&order, action, how));
            let a = answer(&api(&plane), RUN, &want, &opts()).await.unwrap();
            assert_eq!(a.ack, Ack::Received { text_bytes: 0 }, "{action} {order:?}");
        }
    }
}

#[tokio::test]
async fn cline_not_taking_the_answer_is_reported_with_the_reason() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![ack(11, 10, "input-ignored", json!({"why": "bad-model"}))],
    );
    let want = Want::TryModel {
        model: "opus".into(),
        text: None,
    };
    let a = answer(&api(&plane), RUN, &want, &opts()).await.unwrap();
    assert_eq!(
        a.ack,
        Ack::Ignored {
            why: "bad-model".into()
        }
    );
    assert_eq!(
        answered_text(&a),
        (
            "Cline did not take another model: that model is not one of this run's routes."
                .to_string(),
            1
        )
    );
}

#[tokio::test]
async fn a_question_that_closes_without_taking_the_answer_is_reported() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![ack(
            11,
            10,
            "input-closed",
            json!({"how": "expired", "waitedS": 1800}),
        )],
    );
    let a = answer(&api(&plane), RUN, &say("late"), &opts())
        .await
        .unwrap();
    assert_eq!(
        a.ack,
        Ack::Closed {
            how: "expired".into()
        }
    );
    let (text, code) = answered_text(&a);
    assert_eq!(code, 1);
    assert_eq!(
        text,
        "The question closed before Cline took your answer (closed when nobody answered in time)."
    );
}

#[tokio::test]
async fn an_older_acknowledgement_of_the_same_question_is_not_the_answer_to_this_one() {
    // The user's earlier try was ignored (`bad-model`); the new, valid one is taken.
    let plane = Script::with(vec![
        needs_input(10, json!({})),
        ack(11, 10, "input-ignored", json!({"why": "bad-model"})),
    ]);
    plane.ack_after_post(
        1,
        vec![ack(12, 10, "input-received", json!({"textBytes": 0}))],
    );
    let a = answer(&api(&plane), RUN, &Want::Finish, &opts())
        .await
        .unwrap();
    assert_eq!(a.ack, Ack::Received { text_bytes: 0 });
}

#[tokio::test]
async fn an_acknowledgement_of_another_question_is_not_this_ones() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![ack(11, 99, "input-received", json!({"textBytes": 0}))],
    );
    let a = answer(&api(&plane), RUN, &Want::Finish, &opts())
        .await
        .unwrap();
    assert_eq!(a.ack, Ack::Unacknowledged);
}

#[tokio::test]
async fn no_wait_returns_when_the_answer_is_queued() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    let o = AnswerOptions {
        no_wait: true,
        ..opts()
    };
    let a = answer(&api(&plane), RUN, &Want::Finish, &o).await.unwrap();
    assert_eq!(a.ack, Ack::NotWaited);
    assert_eq!(
        *plane.reads_after_last_post.lock().unwrap(),
        0,
        "no event read after the POST"
    );
    assert_eq!(plane.posts().len(), 1);
    let (text, code) = answered_text(&a);
    assert_eq!(code, 0);
    assert!(
        text.starts_with("Queued finish for Cline;") && text.contains(&format!("ham watch {RUN}")),
        "{text}"
    );
}

#[tokio::test]
async fn an_explicit_request_id_is_answered_as_given() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    plane.ack_after_post(
        1,
        vec![ack(21, 20, "input-received", json!({"textBytes": 0}))],
    );
    let o = AnswerOptions {
        request_id: Some(id(20)),
        ..opts()
    };
    let a = answer(&api(&plane), RUN, &Want::Finish, &o).await.unwrap();
    assert_eq!(a.request_id, id(20));
    assert_eq!(plane.posts()[0]["requestId"], id(20));
    assert_eq!(a.ack, Ack::Received { text_bytes: 0 });
}

#[tokio::test]
async fn an_explicit_request_id_needs_no_open_question() {
    let plane = Script::with(vec![]);
    plane.ack_after_post(
        1,
        vec![ack(
            21,
            20,
            "input-ignored",
            json!({"why": "unknown-request"}),
        )],
    );
    let o = AnswerOptions {
        request_id: Some(id(20)),
        ..opts()
    };
    let a = answer(&api(&plane), RUN, &say("x"), &o).await.unwrap();
    assert_eq!(
        a.ack,
        Ack::Ignored {
            why: "unknown-request".into()
        }
    );
}

#[tokio::test]
async fn the_json_line_names_the_result() {
    let a = ham::commands::Answered {
        engine: "cline".into(),
        run_id: RUN.into(),
        request_id: id(10),
        action: InstructAction::TryModel,
        model: Some("us.anthropic.claude-opus-5-5".into()),
        state: "queued".into(),
        ack: Ack::Received { text_bytes: 9 },
    };
    assert_eq!(
        answered_json(&a),
        json!({"runId": RUN, "requestId": id(10), "action": "try_model", "state": "queued", "result": "received",
               "textBytes": 9, "model": "us.anthropic.claude-opus-5-5"})
    );
    for (ack, result) in [
        (
            Ack::Ignored {
                why: "limit".into(),
            },
            "ignored",
        ),
        (
            Ack::Closed {
                how: "expired".into(),
            },
            "closed",
        ),
        (Ack::Unacknowledged, "unacknowledged"),
        (Ack::NotWaited, "queued"),
    ] {
        let j = answered_json(&ham::commands::Answered { ack, ..a.clone() });
        assert_eq!(j["result"], result);
    }
}

/// A control plane nobody answers.
struct Down;

impl ControlPlane for Down {
    async fn call(
        &self,
        _method: &str,
        _path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Network("connection refused".into()))
    }

    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Network("connection refused".into()))
    }
}

#[tokio::test]
async fn an_unreachable_service_is_a_read_error_and_nothing_is_sent() {
    let down = Api::new(Arc::new(Down)).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let e = answer(&down, RUN, &Want::Finish, &opts())
        .await
        .unwrap_err();
    assert!(matches!(e, AnswerError::Read(_)), "{e}");
    assert_eq!(e.to_string(), "reading events: network: connection refused");
}

#[tokio::test]
async fn a_dropped_connection_on_the_answer_is_reported_in_the_transports_words() {
    let plane = Script::with(vec![needs_input(10, json!({}))]);
    *plane.post_drops.lock().unwrap() = 1;
    let once = Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let e = answer(&once, RUN, &Want::Finish, &opts())
        .await
        .unwrap_err();
    assert!(matches!(e, AnswerError::Api(_)), "{e}");
    assert_eq!(e.to_string(), "network: connection reset");
    assert!(plane.posts().is_empty());
    // With retries the same answer goes through once the connection holds.
    *plane.post_drops.lock().unwrap() = 1;
    plane.ack_after_post(
        1,
        vec![ack(11, 10, "input-received", json!({"textBytes": 0}))],
    );
    let a = answer(&api(&plane), RUN, &Want::Finish, &opts())
        .await
        .unwrap();
    assert_eq!(a.ack, Ack::Received { text_bytes: 0 });
    assert_eq!(plane.posts().len(), 1);
}

#[tokio::test]
async fn an_action_missing_from_the_options_is_refused_even_when_the_session_is_alive() {
    // The question lists no model switch (one route only) while the session can take a message.
    let plane = Script::with(vec![needs_input(
        10,
        json!({"options": ["continue_with_prompt", "finish"], "models": []}),
    )]);
    let want = Want::TryModel {
        model: "opus".into(),
        text: None,
    };
    let e = answer(&api(&plane), RUN, &want, &opts()).await.unwrap_err();
    let text = e.to_string();
    assert!(matches!(e, AnswerError::Refused(_)));
    assert!(
        text.contains("this question does not offer it")
            && text.contains("a message to the same session")
            && !text.contains("another model;"),
        "{text}"
    );
    assert!(plane.posts().is_empty());
    // A question that offers nothing at all says so.
    let plane = Script::with(vec![needs_input(10, json!({"options": []}))]);
    let e = answer(&api(&plane), RUN, &Want::Finish, &opts())
        .await
        .unwrap_err();
    assert!(e.to_string().ends_with("It offers: nothing"), "{e}");
}
