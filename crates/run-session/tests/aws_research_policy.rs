//! The DynamoDB write of the research policy row against recorded HTTP exchanges: one conditional put into the research table.
#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use harness_protocol::research::{PolicyItem, ResearchLimits, ResearchProvider};
use run_session::aws::DynamoStore;
use run_session::ports::Store;
use serde_json::{Value, json};

const CCF: &str = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;
const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";

fn replay(responses: Vec<(u16, &str)>) -> StaticReplayClient {
    StaticReplayClient::new(
        responses
            .into_iter()
            .map(|(status, body)| {
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://example.invalid/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    http::Response::builder()
                        .status(status)
                        .body(SdkBody::from(body.to_string()))
                        .unwrap(),
                )
            })
            .collect(),
    )
}

fn ddb(r: &StaticReplayClient, research_table: Option<&str>) -> DynamoStore {
    DynamoStore {
        client: aws_sdk_dynamodb::Client::from_conf(
            aws_sdk_dynamodb::Config::builder()
                .behavior_version_latest()
                .region(aws_sdk_dynamodb::config::Region::new("us-east-1"))
                .credentials_provider(aws_sdk_dynamodb::config::Credentials::for_tests())
                .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
                .http_client(r.clone())
                .build(),
        ),
        table: "hamh-direct-harness".into(),
        research_table: research_table.map(str::to_string),
    }
}

fn row() -> PolicyItem {
    PolicyItem::new(
        RUN,
        true,
        ResearchProvider::Tavily,
        ResearchLimits::default(),
        1_792_200_000,
    )
    .with_prompt_urls(vec!["https://docs.rs/serde/latest/serde/".into()])
}

fn bodies(c: &StaticReplayClient) -> Vec<Value> {
    c.actual_requests()
        .map(|r| serde_json::from_slice(r.body().bytes().unwrap_or_default()).unwrap())
        .collect()
}

#[tokio::test]
async fn the_row_is_one_conditional_put_into_the_research_table() {
    let c = replay(vec![(200, "{}")]);
    ddb(&c, Some("hamh-direct-research"))
        .put_research_policy(&row())
        .await
        .unwrap();
    let b = &bodies(&c)[0];
    assert_eq!(b["TableName"], "hamh-direct-research");
    assert_eq!(b["ConditionExpression"], "attribute_not_exists(PK)");
    assert_eq!(b["Item"]["PK"], json!({"S": format!("run#{RUN}")}));
    assert_eq!(b["Item"]["SK"], json!({"S": "cfg"}));
    assert_eq!(b["Item"]["enabled"], json!({"BOOL": true}));
    assert_eq!(b["Item"]["provider"], json!({"S": "tavily"}));
    assert_eq!(
        b["Item"]["promptUrls"]["L"][0],
        json!({"S": "https://docs.rs/serde/latest/serde/"})
    );
    assert!(b["Item"]["ttl"]["N"].as_str().is_some());
}

#[tokio::test]
async fn an_existing_row_is_kept_and_a_store_without_a_research_table_writes_nothing() {
    let c = replay(vec![(400, CCF)]);
    ddb(&c, Some("hamh-direct-research"))
        .put_research_policy(&row())
        .await
        .unwrap();
    let c = replay(vec![]);
    ddb(&c, None).put_research_policy(&row()).await.unwrap();
    assert_eq!(c.actual_requests().count(), 0);
}

#[tokio::test]
async fn any_other_failure_is_unavailable() {
    let c = replay(vec![(
        400,
        r#"{"__type":"com.amazonaws.dynamodb.v20120810#ResourceNotFoundException","message":"no table"}"#,
    )]);
    let e = ddb(&c, Some("hamh-direct-research"))
        .put_research_policy(&row())
        .await
        .unwrap_err();
    assert!(
        matches!(e, run_session::error::RunSessionError::Unavailable(_)),
        "{e}"
    );
}
