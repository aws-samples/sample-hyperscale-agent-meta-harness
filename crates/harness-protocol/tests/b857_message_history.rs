//! The user's earlier messages from the service (protocol 1.63.0): the wire of `POST /internal/message-history`.
//!
//! At the start of a session the worker asks the service for the messages of the run that were delivered to an engine in the sessions
//! before it. The route is a route of its own beside the four of message control: it reads and changes nothing, the invocation of a
//! session is not touched, and a worker that does not know the route never calls it.
#![allow(clippy::unwrap_used)]

use harness_protocol::api::INTERNAL_ROUTES;
use harness_protocol::direct::{DirectSlice, MAX_SLICE_BRIEF_BYTES};
use harness_protocol::export::all_schemas;
use harness_protocol::messages::{
    DeliveredMessage, HistoryRequest, HistoryResponse, MAX_HISTORY_TEXT_BYTES, UnconfirmedMessage,
    bound_history, bound_history_with,
};
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn mid(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn delivered(n: u32, text: &str) -> DeliveredMessage {
    DeliveredMessage {
        message_id: mid(n),
        seq: n,
        delivered_turn: Some(n + 1),
        text: text.into(),
    }
}

#[test]
fn b857_the_history_route_is_listed_beside_the_four_message_routes() {
    let r = INTERNAL_ROUTES
        .iter()
        .find(|r| r.operation_id == "listDeliveredMessages")
        .expect("the route is an /internal route");
    assert_eq!((r.method, r.path), ("POST", "/internal/message-history"));
    // after the four routes of message control and before the research routes, so no route before it moved
    let paths: Vec<&str> = INTERNAL_ROUTES.iter().map(|r| r.path).collect();
    let at = paths
        .iter()
        .position(|p| *p == "/internal/message-history")
        .unwrap();
    assert_eq!(paths[at - 1], "/internal/message-outcome");
    assert_eq!(paths[at + 1], "/internal/research/search");
}

#[test]
fn b857_the_request_names_the_run_and_the_agent_and_takes_no_other_member() {
    let req: HistoryRequest =
        serde_json::from_value(json!({"runId": RUN, "agentId": "direct"})).unwrap();
    assert_eq!(
        (req.run_id.as_str(), req.agent_id.as_str()),
        (RUN, "direct")
    );
    assert_eq!(
        serde_json::to_value(&req).unwrap(),
        json!({"runId": RUN, "agentId": "direct"})
    );
    // like the requests of the four message routes it is read strictly: a member the service does not know is refused, never guessed
    let err = serde_json::from_value::<HistoryRequest>(
        json!({"runId": RUN, "agentId": "direct", "text": "words"}),
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn b857_the_answer_carries_ids_order_and_text_and_a_reader_ignores_members_it_does_not_know() {
    let answer = HistoryResponse {
        messages: vec![delivered(1, "use tabs"), delivered(3, "name it REPORT.md")],
        cut: 2,
        ..HistoryResponse::default()
    };
    let wire = serde_json::to_value(&answer).unwrap();
    assert_eq!(
        wire,
        json!({"messages": [
            {"messageId": mid(1), "seq": 1, "deliveredTurn": 2, "text": "use tabs"},
            {"messageId": mid(3), "seq": 3, "deliveredTurn": 4, "text": "name it REPORT.md"},
        ], "cut": 2})
    );
    // An answer of a later service: members this reader does not know, on the answer and on a message, are ignored.
    let later = json!({"messages": [
        {"messageId": mid(1), "seq": 1, "text": "use tabs", "deliveredAt": "2026-11-17T10:00:00Z", "epochNo": 1},
    ], "cut": 0, "asOf": 12, "complete": true});
    let read: HistoryResponse = serde_json::from_value(later).unwrap();
    assert_eq!(read.messages.len(), 1);
    assert_eq!(read.messages[0].text, "use tabs");
    assert_eq!(read.messages[0].delivered_turn, None);
    // `cut` may be left out; a message without an id, an order or a text is not this answer
    let bare: HistoryResponse = serde_json::from_value(json!({"messages": []})).unwrap();
    assert_eq!(bare, HistoryResponse::default());
    assert!(serde_json::from_value::<HistoryResponse>(json!({"cut": 1})).is_err());
    assert!(
        serde_json::from_value::<HistoryResponse>(
            json!({"messages": [{"messageId": mid(1), "seq": 1}]})
        )
        .is_err(),
        "a message without its text"
    );
}

#[test]
fn b857_the_bound_of_the_answer_is_the_bound_of_what_one_session_hands_the_next() {
    assert_eq!(MAX_HISTORY_TEXT_BYTES, 16 * 1024);
    assert_eq!(MAX_HISTORY_TEXT_BYTES, MAX_SLICE_BRIEF_BYTES);
}

#[test]
fn b857_the_bound_cuts_whole_messages_the_oldest_first_and_counts_them() {
    // 7 messages of 3,000 bytes: 5 fit (15,000 bytes), the two oldest are cut
    let all: Vec<DeliveredMessage> = (1..=7)
        .map(|n| delivered(n, &format!("{n}").repeat(3000)))
        .collect();
    let kept = bound_history(all.clone(), MAX_HISTORY_TEXT_BYTES);
    assert_eq!(kept.cut, 2);
    assert_eq!(
        kept.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [3, 4, 5, 6, 7],
        "the newest stay, in their order"
    );
    assert!(
        kept.messages.iter().all(|m| m.text.len() == 3000),
        "whole messages"
    );
    // everything fits: nothing is cut
    let few = bound_history(all[..3].to_vec(), MAX_HISTORY_TEXT_BYTES);
    assert_eq!((few.messages.len(), few.cut), (3, 0));
    // none
    assert_eq!(
        bound_history(Vec::new(), MAX_HISTORY_TEXT_BYTES),
        HistoryResponse::default()
    );
}

#[test]
fn b857_the_bound_counts_bytes_and_never_cuts_inside_a_message() {
    // Multibyte text: 2,000 characters of 3 bytes each are 6,000 bytes. Three messages are 18,000 bytes: the oldest is cut, whole.
    let text = "\u{20ac}".repeat(2000);
    assert_eq!(text.len(), 6000);
    let all: Vec<DeliveredMessage> = (1..=3).map(|n| delivered(n, &text)).collect();
    let kept = bound_history(all, MAX_HISTORY_TEXT_BYTES);
    assert_eq!(kept.cut, 1);
    assert_eq!(kept.messages.len(), 2);
    for m in &kept.messages {
        assert_eq!(m.text, text, "a kept message is the whole message");
    }
    // The newest message alone is longer than the bound: it stays whole (the worker shows its start and its end), the others are cut.
    let long = "x".repeat(MAX_HISTORY_TEXT_BYTES + 1);
    let kept = bound_history(
        vec![delivered(1, "old"), delivered(2, &long)],
        MAX_HISTORY_TEXT_BYTES,
    );
    assert_eq!(kept.cut, 1);
    assert_eq!(kept.messages.len(), 1);
    assert_eq!(kept.messages[0].text.len(), MAX_HISTORY_TEXT_BYTES + 1);
}

fn schema(name: &str) -> Value {
    all_schemas()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v)
        .unwrap()
}

#[test]
fn b857_the_internal_api_document_describes_the_route_and_its_answer() {
    let doc = schema("openapi-internal.json");
    let op = &doc["paths"]["/internal/message-history"]["post"];
    assert_eq!(op["operationId"], "listDeliveredMessages");
    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["HistoryRequest"]["additionalProperties"], false,
        "the request is strict"
    );
    assert_eq!(
        schemas["HistoryRequest"]["required"],
        json!(["runId", "agentId"])
    );
    let message = &schemas["DeliveredMessage"];
    assert_eq!(message["required"], json!(["messageId", "seq", "text"]));
    assert!(
        message.get("additionalProperties").is_none(),
        "a reader takes an answer with members it does not know"
    );
    assert!(
        schemas["HistoryResponse"]
            .get("additionalProperties")
            .is_none()
    );
}

/// The contract of the invocation: the slice member of a session's invocation is what it was in the release before. Nothing about the
/// user's messages rides in it, so a worker image of that release reads the invocation of this service.
#[test]
fn b857_the_slice_member_of_the_invocation_is_unchanged() {
    let runtime = schema("runtime.schema.json");
    let slice = &runtime["$defs"]["DirectSlice"];
    let mut members: Vec<&str> = slice["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    members.sort_unstable();
    assert_eq!(
        members,
        ["baseSha", "brief", "last", "restoreHead", "sessionsBefore"]
    );
    assert_eq!(slice["additionalProperties"], false);
    assert_eq!(slice["required"], json!(["last"]));
    // the slice of the release before reads, and a slice with one more member is refused, as it always was
    let before = json!({"last": false, "baseSha": "a".repeat(40), "restoreHead": "b".repeat(40), "sessionsBefore": 1, "brief": "done so far"});
    let read: DirectSlice = serde_json::from_value(before.clone()).unwrap();
    assert_eq!(serde_json::to_value(&read).unwrap(), before);
    let mut more = before;
    more["userMessages"] = json!(["name it REPORT.md"]);
    assert!(serde_json::from_value::<DirectSlice>(more).is_err());
}

// ---- messages handed to an earlier session with no report of their delivery ------------------------------------------------------

/// The answer as a worker read it before the member `unconfirmed` existed: the two members it knew.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnswerBefore {
    messages: Vec<DeliveredMessage>,
    #[serde(default)]
    cut: u32,
}

/// An answer with one delivered message and one that was handed to an earlier session with no report of its delivery.
fn with_unconfirmed() -> Value {
    json!({"messages": [
        {"messageId": mid(1), "seq": 1, "deliveredTurn": 2, "text": "use tabs"},
    ], "cut": 0, "unconfirmed": [
        {"messageId": mid(2), "seq": 2, "text": "name it REPORT.md"},
    ]})
}

#[test]
fn b857_the_answer_names_the_messages_without_a_delivery_report_in_a_member_of_their_own() {
    let wire = with_unconfirmed();
    let read: HistoryResponse = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(
        read.messages.len(),
        1,
        "a message without a report of its delivery is not among the delivered ones"
    );
    assert_eq!(
        serde_json::to_value(&read).unwrap(),
        wire,
        "the member is read and written: the id, the order and the stored text of each message, and no turn"
    );
    // An answer with no such message leaves the member out: it is the answer it was.
    let none: HistoryResponse = serde_json::from_value(json!({"messages": [], "cut": 0})).unwrap();
    assert_eq!(
        serde_json::to_value(&none).unwrap(),
        json!({"messages": [], "cut": 0})
    );
    // a message of the member without its text is not this answer
    assert!(
        serde_json::from_value::<HistoryResponse>(
            json!({"messages": [], "unconfirmed": [{"messageId": mid(2), "seq": 2}]})
        )
        .is_err()
    );
}

/// A worker that does not know the member reads the answer it always read: the delivered messages and the count of the cut ones.
#[test]
fn b857_a_worker_that_does_not_know_the_member_reads_the_answer_as_it_did() {
    let read: AnswerBefore = serde_json::from_value(with_unconfirmed()).unwrap();
    assert_eq!(read.messages.len(), 1);
    assert_eq!(read.messages[0].text, "use tabs");
    assert_eq!(read.cut, 0);
}

#[test]
fn b857_the_internal_api_document_describes_the_member_of_the_unconfirmed_messages() {
    let doc = schema("openapi-internal.json");
    let schemas = &doc["components"]["schemas"];
    let member = &schemas["HistoryResponse"]["properties"]["unconfirmed"];
    assert_eq!(member["type"], "array", "{member}");
    assert_eq!(
        member["items"]["$ref"], "#/components/schemas/UnconfirmedMessage",
        "{member}"
    );
    assert_eq!(
        schemas["HistoryResponse"]["required"],
        json!(["messages"]),
        "an answer without the member is an answer"
    );
    let message = &schemas["UnconfirmedMessage"];
    assert_eq!(message["required"], json!(["messageId", "seq", "text"]));
    assert!(
        message.get("additionalProperties").is_none(),
        "a reader takes an answer with members it does not know"
    );
}

fn unconfirmed(n: u32, text: &str) -> UnconfirmedMessage {
    UnconfirmedMessage {
        message_id: mid(n),
        seq: n,
        text: text.into(),
    }
}

/// One bound for both kinds: the oldest messages are cut first, whichever kind they are, and a kept message is whole.
#[test]
fn b857_one_bound_holds_both_kinds_and_the_oldest_of_either_is_cut_first() {
    // Multibyte text: 2,000 characters of 3 bytes each are 6,000 bytes a message. Four messages are 24,000 bytes: two fit.
    let text = "\u{20ac}".repeat(2000);
    // admitted in the order 1 (delivered), 2 (no report), 3 (delivered), 4 (no report)
    let kept = bound_history_with(
        vec![delivered(1, &text), delivered(3, &text)],
        vec![unconfirmed(2, &text), unconfirmed(4, &text)],
        MAX_HISTORY_TEXT_BYTES,
    );
    assert_eq!(kept.cut, 2, "the two oldest: one of each kind");
    assert_eq!(kept.messages.iter().map(|m| m.seq).collect::<Vec<_>>(), [3]);
    assert_eq!(
        kept.unconfirmed.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [4]
    );
    assert!(
        kept.messages.iter().all(|m| m.text == text)
            && kept.unconfirmed.iter().all(|m| m.text == text),
        "a kept message is the whole message"
    );
    // The newest message is an unconfirmed one that alone is longer than the bound: it stays whole and everything else is cut.
    let long = "x".repeat(MAX_HISTORY_TEXT_BYTES + 1);
    let kept = bound_history_with(
        vec![delivered(1, "old"), delivered(2, "older")],
        vec![unconfirmed(3, &long)],
        MAX_HISTORY_TEXT_BYTES,
    );
    assert_eq!((kept.cut, kept.messages.len()), (2, 0));
    assert_eq!(kept.unconfirmed[0].text.len(), MAX_HISTORY_TEXT_BYTES + 1);
    // everything fits: nothing is cut, and each list keeps its order
    let few = bound_history_with(
        vec![delivered(1, "a"), delivered(4, "d")],
        vec![unconfirmed(2, "b"), unconfirmed(3, "c")],
        MAX_HISTORY_TEXT_BYTES,
    );
    assert_eq!(few.cut, 0);
    assert_eq!(
        few.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [1, 4]
    );
    assert_eq!(
        few.unconfirmed.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [2, 3]
    );
    // with no unconfirmed message the answer is the one of the delivered messages alone
    let all: Vec<DeliveredMessage> = (1..=7)
        .map(|n| delivered(n, &format!("{n}").repeat(3000)))
        .collect();
    assert_eq!(
        bound_history_with(all.clone(), Vec::new(), MAX_HISTORY_TEXT_BYTES),
        bound_history(all, MAX_HISTORY_TEXT_BYTES)
    );
}
