//! The AWS adapters of the compute choice against recorded HTTP exchanges: the Instances runtime's ARN from its SSM parameter, the
//! DeleteCapacityProviderSession call, and the DynamoDB item that counts active Instances sessions (a string set of run ids, claimed with a
//! condition on its size and released by removing the run).
#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use run_session::aws::{AgentCoreSessions, DynamoStore, RuntimeParameter};
use run_session::error::RunSessionError;
use run_session::ports::{Sessions, Store};
use serde_json::{Value, json};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const PROVIDER: &str = "hamh_direct_instances-AbCdEfGhIj";
const ARN: &str = "arn:aws:bedrock-agentcore:us-east-1:123456789012:runtime/hamh_direct_harness_worker_python_instances-XyZaBc";
const CCF: &str = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;
const DDB_500: &str =
    r#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"boom"}"#;

fn event(status: u16, content_type: &str, body: &str) -> ReplayEvent {
    ReplayEvent::new(
        http::Request::builder()
            .uri("https://example.invalid/")
            .body(SdkBody::empty())
            .unwrap(),
        http::Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(SdkBody::from(body.to_string()))
            .unwrap(),
    )
}

fn replay(content_type: &str, responses: Vec<(u16, &str)>) -> StaticReplayClient {
    StaticReplayClient::new(
        responses
            .into_iter()
            .map(|(s, b)| event(s, content_type, b))
            .collect(),
    )
}

macro_rules! client {
    ($sdk:ident, $replay:expr) => {
        $sdk::Client::from_conf(
            $sdk::Config::builder()
                .behavior_version_latest()
                .region($sdk::config::Region::new("us-east-1"))
                .credentials_provider($sdk::config::Credentials::for_tests())
                .retry_config($sdk::config::retry::RetryConfig::disabled())
                .http_client($replay.clone())
                .build(),
        )
    };
}

fn ddb(r: &StaticReplayClient) -> DynamoStore {
    DynamoStore {
        client: client!(aws_sdk_dynamodb, r),
        table: "hamh-direct-harness".into(),
        research_table: None,
    }
}

fn sessions(agentcore: &StaticReplayClient, ssm: Option<&StaticReplayClient>) -> AgentCoreSessions {
    AgentCoreSessions {
        client: client!(aws_sdk_bedrockagentcore, agentcore),
        control: client!(aws_sdk_bedrockagentcorecontrol, agentcore),
        conflict_base_ms: 0,
        runtime_parameter: None,
        instances_parameter: ssm.map(|r| RuntimeParameter {
            client: client!(aws_sdk_ssm, r),
            name: "/hamh/direct/worker-python-instances-runtime-arn".into(),
        }),
        capacity_provider_parameter: None,
    }
}

/// An adapter that names the capacity provider's parameter and nothing else.
fn sessions_with_provider(ssm: &StaticReplayClient) -> AgentCoreSessions {
    AgentCoreSessions {
        capacity_provider_parameter: Some(RuntimeParameter {
            client: client!(aws_sdk_ssm, ssm),
            name: "/hamh/direct/instances-capacity-provider-id".into(),
        }),
        ..sessions(&replay("application/json", vec![]), None)
    }
}

fn bodies(r: &StaticReplayClient) -> Vec<Value> {
    r.actual_requests()
        .map(|q| {
            serde_json::from_slice(q.body().bytes().unwrap_or_default()).unwrap_or(Value::Null)
        })
        .collect()
}

// ---- the slot item ------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_adds_the_run_to_the_set_under_a_condition_on_its_size() {
    let r = replay("application/x-amz-json-1.0", vec![(200, "{}")]);
    assert!(ddb(&r).claim_instances_slot(RUN, 6).await.unwrap());
    let b = &bodies(&r)[0];
    assert_eq!(b["TableName"], "hamh-direct-harness");
    let key = b["Key"].as_object().unwrap();
    assert_eq!(key["PK"], json!({"S": "STAGE#instances"}));
    assert_eq!(key["SK"], json!({"S": "ACTIVE"}));
    assert_eq!(b["UpdateExpression"], "ADD runs :r");
    let cond = b["ConditionExpression"].as_str().unwrap();
    assert_eq!(
        cond,
        "attribute_not_exists(runs) OR contains(runs, :id) OR size(runs) < :max"
    );
    let v = &b["ExpressionAttributeValues"];
    assert_eq!(v[":r"], json!({"SS": [RUN]}));
    assert_eq!(v[":id"], json!({"S": RUN}));
    assert_eq!(v[":max"], json!({"N": "6"}));
}

#[tokio::test]
async fn a_full_stage_is_false_and_a_failing_table_is_unavailable() {
    let r = replay(
        "application/x-amz-json-1.0",
        vec![(400, CCF), (500, DDB_500)],
    );
    let store = ddb(&r);
    assert!(
        !store.claim_instances_slot(RUN, 1).await.unwrap(),
        "the condition failed: no room"
    );
    let e = store.claim_instances_slot(RUN, 1).await.unwrap_err();
    assert!(matches!(e, RunSessionError::Unavailable(_)), "{e:?}");
}

#[tokio::test]
async fn a_release_removes_the_run_from_the_set_and_a_repeat_is_harmless() {
    let r = replay("application/x-amz-json-1.0", vec![(200, "{}"), (200, "{}")]);
    let store = ddb(&r);
    store.release_instances_slot(RUN).await.unwrap();
    store.release_instances_slot(RUN).await.unwrap();
    let b = &bodies(&r)[0];
    assert_eq!(b["Key"]["PK"], json!({"S": "STAGE#instances"}));
    assert_eq!(b["UpdateExpression"], "DELETE runs :r");
    assert_eq!(b["ExpressionAttributeValues"][":r"], json!({"SS": [RUN]}));
    assert!(
        b.get("ConditionExpression").is_none(),
        "no condition: removing an absent run is fine"
    );
}

// ---- the sessions adapter -----------------------------------------------------------------------------------------------------

fn parameter(value: Option<&str>) -> String {
    match value {
        Some(v) => json!({"Parameter": {"Name": "/hamh/direct/worker-python-instances-runtime-arn", "Type": "String", "Value": v}}).to_string(),
        None => json!({"__type": "ParameterNotFound", "message": "no such parameter"}).to_string(),
    }
}

#[tokio::test]
async fn the_instances_runtime_arn_is_read_from_its_parameter_and_an_absent_one_is_not_deployed() {
    let agentcore = replay("application/json", vec![]);
    let present = parameter(Some(ARN));
    let absent = parameter(None);
    let r = replay(
        "application/x-amz-json-1.1",
        vec![(200, &present), (400, &absent)],
    );
    let s = sessions(&agentcore, Some(&r));
    assert_eq!(
        s.instances_runtime_arn().await.unwrap().as_deref(),
        Some(ARN)
    );
    let e = s.instances_runtime_arn().await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid", "{e}");
    assert!(e.to_string().contains("not deployed"), "{e}");
    // An adapter with no parameter named has no lookup.
    let none = sessions(&agentcore, None);
    assert_eq!(none.instances_runtime_arn().await.unwrap(), None);
}

#[tokio::test]
async fn the_provider_session_is_deleted_and_not_found_is_false() {
    let accepted = json!({"capacityProviderArn": format!("arn:aws:bedrock-agentcore:us-east-1:123456789012:capacity-provider/{PROVIDER}"), "sessionId": "s", "status": "Deprovisioning"}).to_string();
    let r = replay(
        "application/json",
        vec![
            (200, &accepted),
            (
                404,
                r#"{"__type":"ResourceNotFoundException","message":"gone"}"#,
            ),
            (403, r#"{"__type":"AccessDeniedException","message":"no"}"#),
        ],
    );
    let s = sessions(&r, None);
    let sid = format!("{RUN}-direct-g1");
    assert!(s.delete_provider_session(PROVIDER, &sid).await.unwrap());
    assert!(!s.delete_provider_session(PROVIDER, &sid).await.unwrap());
    let e = s.delete_provider_session(PROVIDER, &sid).await.unwrap_err();
    assert!(matches!(e, RunSessionError::Unavailable(_)), "{e:?}");
    assert!(
        e.to_string().contains("DeleteCapacityProviderSession"),
        "{e}"
    );
}

// ---- the capacity provider's id -------------------------------------------------------------------

fn provider_parameter(value: &str) -> String {
    json!({"Parameter": {"Name": "/hamh/direct/instances-capacity-provider-id", "Type": "String", "Value": value}}).to_string()
}

#[tokio::test]
async fn the_capacity_provider_id_is_read_from_its_parameter_each_time_it_is_asked() {
    let present = provider_parameter(PROVIDER);
    let r = replay(
        "application/x-amz-json-1.1",
        vec![(200, &present), (200, &present)],
    );
    let s = sessions_with_provider(&r);
    assert_eq!(
        s.capacity_provider_id().await.unwrap().as_deref(),
        Some(PROVIDER)
    );
    assert_eq!(
        s.capacity_provider_id().await.unwrap().as_deref(),
        Some(PROVIDER),
        "a warm function reads a parameter that was written after it started"
    );
    let b = bodies(&r);
    assert_eq!(b.len(), 2);
    assert_eq!(b[0]["Name"], "/hamh/direct/instances-capacity-provider-id");
    // An adapter that names no parameter has no lookup.
    let none = sessions(&replay("application/json", vec![]), None);
    assert_eq!(none.capacity_provider_id().await.unwrap(), None);
}

#[tokio::test]
async fn a_missing_or_unusable_provider_parameter_names_the_stage_setup_and_a_failed_read_may_pass()
{
    let absent = parameter(None);
    let not_an_id = provider_parameter("has space-1234567890");
    let denied = r#"{"__type": "AccessDeniedException", "message": "not allowed"}"#;
    let throttled = r#"{"__type": "ThrottlingException", "message": "slow down"}"#;
    let r = replay(
        "application/x-amz-json-1.1",
        vec![
            (400, &absent),
            (200, &not_an_id),
            (400, denied),
            (400, throttled),
        ],
    );
    let s = sessions_with_provider(&r);
    for want in ["does not exist", "capacity provider id"] {
        let e = s.capacity_provider_id().await.unwrap_err();
        assert!(matches!(e, RunSessionError::Invalid(_)), "{e:?}");
        assert!(
            e.to_string()
                .contains("the Instances capacity provider is not set up")
                && e.to_string().contains(want),
            "{want}: {e}"
        );
    }
    // Any other failure of the read (a missing grant, a throttle) is retried by the machine.
    for _ in 0..2 {
        let e = s.capacity_provider_id().await.unwrap_err();
        assert!(matches!(e, RunSessionError::Unavailable(_)), "{e:?}");
    }
}
