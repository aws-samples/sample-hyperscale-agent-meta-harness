//! Wave 2 (backend lambdas): the run's searchable text takes the user's answers from every page of the control items, not only the first 50
//! items, and stops reading pages once it holds all the text it keeps. Each test sends real requests through the real SDK client against
//! recorded DynamoDB answers (the aws-smithy replay client; no network).
#![allow(clippy::unwrap_used)]

use aws_credential_types::Credentials;
use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use harness_protocol::api::internal::PendingControl;
use harness_protocol::direct_input::{InstructAction, InstructOrigin};
use harness_protocol::store::ControlItem;
use run_session::aws::DynamoStore;
use run_session::ports::Store;
use serde_json::{Value, json};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";

/// A serde value as DynamoDB writes it on the wire.
fn wire(v: &Value) -> Value {
    match v {
        Value::Null => json!({"NULL": true}),
        Value::Bool(b) => json!({"BOOL": b}),
        Value::Number(n) => json!({"N": n.to_string()}),
        Value::String(s) => json!({"S": s}),
        Value::Array(a) => json!({"L": a.iter().map(wire).collect::<Vec<_>>()}),
        Value::Object(o) => {
            json!({"M": o.iter().map(|(k, x)| (k.clone(), wire(x))).collect::<serde_json::Map<_, _>>()})
        }
    }
}

fn control(n: u32, control: PendingControl) -> Value {
    let item = ControlItem {
        run_id: RUN.into(),
        agent_id: "direct".into(),
        control_id: format!("01M4X3Q7R9S0V1W2X3Y4Z5{n:04}"),
        control,
        created_at: "2026-11-05T10:00:00Z".into(),
        delivered_at: None,
    };
    let Value::Object(map) = wire(&serde_json::to_value(&item).unwrap()) else {
        unreachable!()
    };
    map["M"].clone()
}

fn answer(n: u32, text: &str) -> Value {
    control(
        n,
        PendingControl::Instruct {
            request_id: format!("req-{n}"),
            action: InstructAction::ContinueWithPrompt,
            text: Some(text.into()),
            model: None,
            origin: InstructOrigin::User,
        },
    )
}

fn steer(n: u32) -> Value {
    control(
        n,
        PendingControl::Steer {
            text: "ignored".into(),
            now: false,
        },
    )
}

fn page(items: Vec<Value>, next: Option<u32>) -> String {
    let mut body = json!({"Items": items, "Count": 0, "ScannedCount": 0});
    if let Some(n) = next {
        body["LastEvaluatedKey"] =
            json!({"PK": {"S": format!("run#{RUN}")}, "SK": {"S": format!("ctl#direct#{n}")}});
    }
    body.to_string()
}

fn store(replay: &StaticReplayClient) -> DynamoStore {
    DynamoStore {
        client: aws_sdk_dynamodb::Client::from_conf(
            aws_sdk_dynamodb::Config::builder()
                .behavior_version_latest()
                .region(aws_sdk_dynamodb::config::Region::new("us-east-1"))
                .credentials_provider(Credentials::for_tests())
                .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
                .http_client(replay.clone())
                .build(),
        ),
        table: "hamh-direct-harness".into(),
        research_table: None,
    }
}

fn replay(pages: Vec<String>) -> StaticReplayClient {
    StaticReplayClient::new(
        pages
            .into_iter()
            .map(|body| {
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://example.invalid/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    http::Response::builder()
                        .status(200)
                        .header("content-type", "application/x-amz-json-1.0")
                        .body(SdkBody::from(body))
                        .unwrap(),
                )
            })
            .collect(),
    )
}

fn request_bodies(r: &StaticReplayClient) -> Vec<Value> {
    r.actual_requests()
        .map(|q| {
            serde_json::from_slice(q.body().bytes().unwrap_or_default()).unwrap_or(Value::Null)
        })
        .collect()
}

#[tokio::test]
async fn b776w_answers_after_the_first_50_control_items_are_read_too() {
    // 50 control items that are no answer, then a page with the answer the user gave late in a long run.
    let first = page((1..=50).map(steer).collect(), Some(50));
    let second = page(vec![answer(51, "use the staging table")], None);
    let r = replay(vec![first, second]);
    let got = store(&r).user_answers(RUN).await.unwrap();
    assert_eq!(got, vec!["use the staging table".to_string()]);
    let sent = request_bodies(&r);
    assert_eq!(sent.len(), 2, "one query per page");
    assert!(sent[0].get("ExclusiveStartKey").is_none());
    assert_eq!(
        sent[1]["ExclusiveStartKey"]["SK"]["S"],
        json!("ctl#direct#50"),
        "the second query continues where the first stopped"
    );
    assert_eq!(sent[1]["Limit"], json!(50));
}

#[tokio::test]
async fn b776w_paging_stops_once_the_answers_fill_the_4_kib_the_search_body_keeps() {
    // Four answers of 1 KiB each fill the 4 KiB; a second page exists, and a request for it would find no recorded answer left.
    let kib = "a".repeat(1024);
    let first = page((1..=4).map(|n| answer(n, &kib)).collect(), Some(4));
    let r = replay(vec![first]);
    let got = store(&r).user_answers(RUN).await.unwrap();
    assert_eq!(got.len(), 4);
    assert_eq!(
        request_bodies(&r).len(),
        1,
        "no page is read after the text is full"
    );
}

#[tokio::test]
async fn b776w_a_run_without_a_second_page_costs_one_query() {
    let r = replay(vec![page(vec![answer(1, "yes"), steer(2)], None)]);
    let got = store(&r).user_answers(RUN).await.unwrap();
    assert_eq!(got, vec!["yes".to_string()]);
    assert_eq!(request_bodies(&r).len(), 1);
}
