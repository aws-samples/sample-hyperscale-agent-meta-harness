//! follow-up (limits audit rows D09 and D26): twenty messages may wait for a running Cline, a claim still takes three, and one fetch
//! call may name twenty addresses. The bound of the seal answer is in the generated schema too, and the schema is the contract of every writer.

#![allow(clippy::unwrap_used)]

use harness_protocol::export::all_schemas;
use harness_protocol::messages::{
    MAX_CLAIM_BATCH, MAX_OUTSTANDING_MESSAGES, MessageMode, MessageNotice, SealResponse,
};
use harness_protocol::research::{FetchRequest, MAX_FETCH_URLS, ResearchLimits};
use serde_json::{Value, json};
use std::path::PathBuf;

fn openapi_internal_fresh() -> Value {
    all_schemas()
        .into_iter()
        .find(|(name, _)| *name == "openapi-internal.json")
        .unwrap()
        .1
}

fn openapi_internal_committed() -> Value {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/openapi-internal.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn notices(n: u32) -> Vec<MessageNotice> {
    (1..=n)
        .map(|seq| MessageNotice {
            message_id: format!("01M5A2R7K3C9XJ4D8E6FM{seq:05}"),
            seq,
            mode: MessageMode::Next,
            epoch: Some("s-1".into()),
            bytes: 262_144,
            accepted_at: "2026-10-16T14:02:11Z".into(),
        })
        .collect()
}

fn fetch_of(n: usize) -> FetchRequest {
    FetchRequest {
        urls: (0..n).map(|i| format!("https://example.org/{i}")).collect(),
        ..Default::default()
    }
}

#[test]
fn b776w_twenty_messages_may_wait_and_a_claim_still_takes_three() {
    assert_eq!(MAX_OUTSTANDING_MESSAGES, 20);
    // each claimed message can carry 256 KiB of text and the claim answers in one Lambda response of 6 MB
    assert_eq!(MAX_CLAIM_BATCH, 3);
}

#[test]
fn b776w_the_seal_answer_schema_allows_twenty_notices_and_the_claim_schema_three() {
    for (which, doc) in [
        ("fresh export", openapi_internal_fresh()),
        ("committed file", openapi_internal_committed()),
    ] {
        let schemas = &doc["components"]["schemas"];
        assert_eq!(
            schemas["SealResponse"]["oneOf"][0]["properties"]["messages"]["maxItems"],
            json!(20),
            "{which}"
        );
        assert_eq!(
            schemas["ClaimResponse"]["oneOf"][0]["properties"]["messages"]["maxItems"],
            json!(3),
            "{which}"
        );
    }
}

#[test]
fn b776w_a_pending_seal_answer_of_twenty_notices_round_trips_and_stays_small() {
    let answer = SealResponse::Pending {
        messages: notices(MAX_OUTSTANDING_MESSAGES),
    };
    let text = serde_json::to_string(&answer).unwrap();
    let back: SealResponse = serde_json::from_str(&text).unwrap();
    assert_eq!(back, answer);
    // a notice names a message and never holds its text: twenty of them are a few kilobytes whatever the messages hold
    assert!(text.len() < 6_000, "{} bytes", text.len());
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["messages"]
            .as_array()
            .unwrap()
            .len(),
        20
    );
}

#[test]
fn b776w_a_fetch_names_up_to_twenty_addresses() {
    assert_eq!(MAX_FETCH_URLS, 20);
    let limits = ResearchLimits::default();
    assert!(fetch_of(1).validate(&limits).is_ok());
    assert!(fetch_of(5).validate(&limits).is_ok());
    assert!(fetch_of(6).validate(&limits).is_ok());
    assert!(fetch_of(20).validate(&limits).is_ok());
}

#[test]
fn b776w_a_fetch_of_no_address_or_of_twenty_one_is_a_usage_error_that_names_twenty() {
    let limits = ResearchLimits::default();
    for n in [0, 21, 100] {
        let e = fetch_of(n).validate(&limits).unwrap_err();
        assert_eq!(e.message, "urls is 1 to 20 addresses", "{n} addresses");
    }
}
