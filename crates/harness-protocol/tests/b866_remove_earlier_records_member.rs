//! A follow-up of a run that reported done starts without the lifecycle records of that run, and one member of
//! the start request says when the worker leaves them where they are.
//!
//! One thing changed on the wire: the start request takes the member `removeEarlierRecords`. Absent means that the worker keeps the
//! earlier records, as the releases before did, so a request without the member is byte for byte the request of protocol 1.63.0. Two
//! values are ever stored. control-api stores `true` for the start of a follow-up of a run that reported done, and it is the only writer
//! of `true`. `ham` sends `false` for the user's `run.remove_earlier_records = false`, and it is stored as sent.
//!
//! [`removes_earlier_records`] is the one rule the worker reads: a follow-up (`continueFrom`) whose member is `true`.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{ContinueFrom, removes_earlier_records};
use serde_json::{Value, json};

const SOURCE_RUN: &str = "01M7NSQREDB5GXR6N9RTDG1TBJ";

fn head() -> String {
    "1".repeat(40)
}

fn start(extra: Value) -> Result<StartDirectRunRequest, serde_json::Error> {
    let mut v = json!({
        "prompt": "Return every word in upper case.",
        "target": {"mode": "s3-only"},
        "model": "us.anthropic.claude-sonnet-5-5",
        "continueFrom": {"runId": SOURCE_RUN, "headSha": head()},
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::from_value(v)
}

#[test]
fn b866_the_protocol_is_1_64_0() {
    assert_eq!(PROTOCOL_VERSION, "1.64.0");
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    assert_eq!(docs["openapi-v1.json"]["info"]["version"], "1.64.0");
    assert_eq!(docs["openapi-internal.json"]["info"]["version"], "1.64.0");
}

#[test]
fn b866_a_request_without_the_member_is_the_request_of_1_63_0_and_both_values_round_trip() {
    // The default: no member, in and out. The stored bytes of a follow-up without the member are the ones of protocol 1.63.0.
    let default = start(json!({})).unwrap();
    assert_eq!(default.remove_earlier_records, None);
    let as_1_63_0 = format!(
        "{{\"prompt\":\"Return every word in upper case.\",\"target\":{{\"mode\":\"s3-only\"}},\"model\":\"us.anthropic.claude-sonnet-5-5\",\"rules\":[],\"continueFrom\":{{\"runId\":\"{SOURCE_RUN}\",\"headSha\":\"{}\"}}}}",
        head()
    );
    assert_eq!(serde_json::to_string(&default).unwrap(), as_1_63_0);

    // `false` goes in and comes out, as the last member of the stored request.
    let off = start(json!({"removeEarlierRecords": false})).unwrap();
    assert_eq!(off.remove_earlier_records, Some(false));
    let text = serde_json::to_string(&off).unwrap();
    assert_eq!(
        text,
        format!(
            "{},\"removeEarlierRecords\":false}}",
            &as_1_63_0[..as_1_63_0.len() - 1]
        )
    );
    let again: StartDirectRunRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(again, off);

    // `true` goes in and comes out as well: control-api stores it for a follow-up of a run that reported done.
    let on = start(json!({"removeEarlierRecords": true})).unwrap();
    assert_eq!(on.remove_earlier_records, Some(true));
    let text = serde_json::to_string(&on).unwrap();
    assert_eq!(
        text,
        format!(
            "{},\"removeEarlierRecords\":true}}",
            &as_1_63_0[..as_1_63_0.len() - 1]
        )
    );
    let again: StartDirectRunRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(again, on);
    // The member is a boolean and nothing else.
    for bad in [
        json!(0),
        json!(1),
        json!("false"),
        json!({"enabled": false}),
    ] {
        assert!(
            start(json!({"removeEarlierRecords": bad})).is_err(),
            "a value that is no boolean is refused"
        );
    }
    // The spelling is the wire's: the Rust name is refused as an unknown member.
    assert!(start(json!({"remove_earlier_records": false})).is_err());
}

#[test]
fn b866_r2_the_worker_removes_only_in_a_follow_up_whose_member_is_true() {
    let from = ContinueFrom {
        run_id: SOURCE_RUN.to_string(),
        head_sha: Some(head()),
    };
    // An absent member keeps the records, as the releases before the member did: a request an older service stored, a resume, a
    // follow-up of a run that did not report done.
    assert!(!removes_earlier_records(Some(&from), None));
    assert!(removes_earlier_records(Some(&from), Some(true)));
    assert!(!removes_earlier_records(Some(&from), Some(false)));
    // No `continueFrom`: the member means nothing, whatever it says.
    assert!(!removes_earlier_records(None, None));
    assert!(!removes_earlier_records(None, Some(true)));
    assert!(!removes_earlier_records(None, Some(false)));
}

#[test]
fn b866_the_contract_file_names_the_member_and_the_key() {
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    let text = docs["openapi-v1.json"].to_string();
    assert!(text.contains("removeEarlierRecords"));
    assert!(
        text.contains("run.remove_earlier_records"),
        "the description of removeEarlierRecords names the key"
    );
}
