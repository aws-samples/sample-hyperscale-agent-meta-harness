//! The events route read from the end: the `tail` and `before` parameters, the superset response `EventsView`
//! and the user's answer text. These types live in `ham-core`, outside the worker image key; `control-api` and `ham` share the
//! one parser of the query, so a client can never send what the server refuses (or the reverse).
#![allow(clippy::unwrap_used)]

use ham_core::events_view::{
    ANSWERS_BYTE_BUDGET, DEFAULT_TAIL, EVENTS_BYTE_BUDGET, EventsQuery, EventsRead, EventsView,
    MAX_ANSWERS, MAX_TAIL, UserAnswer,
};
use harness_protocol::api::v1::EventsPage;
use harness_protocol::direct_input::InstructAction;
use serde_json::json;
use std::collections::BTreeMap;

const ID_A: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const ID_B: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT2A";

fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn refusal(pairs: &[(&str, &str)]) -> String {
    EventsQuery::from_params(&params(pairs))
        .unwrap_err()
        .to_string()
}

#[test]
fn limits_are_the_documented_numbers() {
    assert_eq!(DEFAULT_TAIL, 50);
    assert_eq!(MAX_TAIL, 500);
    assert_eq!(MAX_ANSWERS, 200);
    // Under the 6 MB a Lambda may answer with, JSON escaping and the cursors included.
    assert_eq!(EVENTS_BYTE_BUDGET, 4_500_000);
    // Together with the events, still under 6 MB.
    assert_eq!(ANSWERS_BYTE_BUDGET, 1_000_000);
}

#[test]
fn a_plain_read_is_forward_from_the_start_or_from_a_cursor() {
    let q = EventsQuery::from_params(&params(&[("shard", "28")])).unwrap();
    assert_eq!(q.shard, 28);
    assert_eq!(q.read, EventsRead::Forward { after: None });
    let q = EventsQuery::from_params(&params(&[("shard", "0"), ("after", ID_A)])).unwrap();
    assert_eq!(
        q.read,
        EventsRead::Forward {
            after: Some(ID_A.into())
        }
    );
}

#[test]
fn tail_reads_the_newest_n_and_before_pages_back() {
    let q = EventsQuery::from_params(&params(&[("shard", "28"), ("tail", "50")])).unwrap();
    assert_eq!(
        q.read,
        EventsRead::Tail {
            count: 50,
            before: None
        }
    );
    let q = EventsQuery::from_params(&params(&[
        ("shard", "28"),
        ("tail", "500"),
        ("before", ID_B),
    ]))
    .unwrap();
    assert_eq!(
        q.read,
        EventsRead::Tail {
            count: 500,
            before: Some(ID_B.into())
        }
    );
    // `before` alone asks for the default page.
    let q = EventsQuery::from_params(&params(&[("shard", "28"), ("before", ID_B)])).unwrap();
    assert_eq!(
        q.read,
        EventsRead::Tail {
            count: DEFAULT_TAIL,
            before: Some(ID_B.into())
        }
    );
}

#[test]
fn the_server_refuses_what_the_contract_refuses() {
    assert!(refusal(&[]).contains("shard"));
    assert!(refusal(&[("shard", "32")]).contains("shard"));
    assert!(refusal(&[("shard", "-1")]).contains("shard"));
    assert!(refusal(&[("shard", "")]).contains("shard"));
    assert!(refusal(&[("shard", "0"), ("after", "nope")]).contains("after"));
    for bad in ["0", "501", "5000", "-3", "+5", "abc", "", "1.5", " 5", "٥"] {
        let m = refusal(&[("shard", "0"), ("tail", bad)]);
        assert!(m.contains("tail"), "tail={bad:?}: {m}");
    }
    assert!(refusal(&[("shard", "0"), ("before", "nope")]).contains("before"));
    assert!(refusal(&[("shard", "0"), ("before", "")]).contains("before"));
    // A forward cursor and a backward one are different reads.
    assert!(refusal(&[("shard", "0"), ("after", ID_A), ("tail", "5")]).contains("after"));
    assert!(refusal(&[("shard", "0"), ("after", ID_A), ("before", ID_B)]).contains("after"));
}

#[test]
fn a_query_survives_the_trip_through_its_own_text() {
    let all = [
        EventsQuery::forward(0, None),
        EventsQuery::forward(31, Some(ID_A)),
        EventsQuery::tail(28, 50),
        EventsQuery::tail(28, MAX_TAIL),
        EventsQuery::older(28, ID_B, 20),
    ];
    for q in all {
        let text = q.to_path_query();
        let map: BTreeMap<String, String> = text
            .split('&')
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap();
                (k.to_string(), v.to_string())
            })
            .collect();
        assert_eq!(EventsQuery::from_params(&map).unwrap(), q, "{text}");
    }
    assert_eq!(EventsQuery::forward(3, None).to_path_query(), "shard=3");
    assert_eq!(
        EventsQuery::older(28, ID_B, 20).to_path_query(),
        format!("shard=28&tail=20&before={ID_B}")
    );
}

fn envelope(id: &str) -> serde_json::Value {
    json!({
        "schemaVersion": 1, "eventId": id, "runId": "01K5ZQ4T8M3N6P7R9S0V1W2X3Y", "agentId": "direct", "seq": 1,
        "ts": "2026-10-15T13:41:07Z", "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": {"kind": "message", "text": "hi"}}
    })
}

fn page_json(events: &[&str]) -> serde_json::Value {
    json!({"events": events.iter().map(|e| envelope(e)).collect::<Vec<_>>(), "nextAfter": events.last()})
}

#[test]
fn a_forward_page_without_extras_is_the_old_page_byte_for_byte() {
    let old: EventsPage = serde_json::from_value(page_json(&[ID_A, ID_B])).unwrap();
    let view: EventsView = serde_json::from_value(page_json(&[ID_A, ID_B])).unwrap();
    assert_eq!(
        serde_json::to_value(&old).unwrap(),
        serde_json::to_value(&view).unwrap()
    );
    assert!(!view.has_more);
    assert!(view.answers.is_empty());
    assert_eq!(view.next_before, None);
}

#[test]
fn an_old_client_parses_a_tail_page_with_answers() {
    let mut v = page_json(&[ID_A, ID_B]);
    v["nextBefore"] = json!(ID_A);
    v["hasMore"] = json!(true);
    v["answers"] = json!([{
        "requestId": ID_A, "action": "continue_with_prompt", "text": "go on", "createdAt": "2026-10-15T13:42:00Z"
    }]);
    let view: EventsView = serde_json::from_value(v.clone()).unwrap();
    assert!(view.has_more);
    assert_eq!(view.next_before.as_deref(), Some(ID_A));
    assert_eq!(view.answers.len(), 1);
    assert_eq!(view.answers[0].action, InstructAction::ContinueWithPrompt);
    assert_eq!(view.answers[0].text.as_deref(), Some("go on"));
    // The old client's type reads the same JSON and keeps its two members.
    let old: EventsPage = serde_json::from_value(v).unwrap();
    assert_eq!(old.events.len(), 2);
    assert_eq!(old.next_after.as_deref(), Some(ID_B));
}

#[test]
fn answer_members_are_left_out_when_empty() {
    let a = UserAnswer {
        request_id: ID_A.into(),
        action: InstructAction::Finish,
        text: None,
        model: None,
        created_at: "2026-10-15T13:42:00Z".into(),
        delivered_at: None,
    };
    let j = serde_json::to_value(&a).unwrap();
    assert_eq!(
        j,
        json!({"requestId": ID_A, "action": "finish", "createdAt": "2026-10-15T13:42:00Z"})
    );
    let back: UserAnswer = serde_json::from_value(j).unwrap();
    assert_eq!(back, a);
    let mut view = EventsView {
        events: vec![],
        next_after: None,
        next_before: None,
        has_more: false,
        answers: vec![],
        answers_truncated: false,
    };
    assert_eq!(serde_json::to_value(&view).unwrap(), json!({"events": []}));
    // A cut-off list of answers says so: a missing answer must not read as "unanswered".
    view.answers_truncated = true;
    assert_eq!(
        serde_json::to_value(&view).unwrap(),
        json!({"events": [], "answersTruncated": true})
    );
}
