//! The AgentCore Runtime calls against recorded HTTP exchanges: the exact requests they send and how they read the
//! responses. No network.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use dispatcher::aws::{
    AwsError, CONFLICT_ATTEMPTS, conflict_backoff_ms, delete_capacity_provider_session, invoke_at,
    stop_session_at,
};
use dispatcher::endpoints::LIVE;
use dispatcher::ids::runtime_session_id;

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const ARN: &str = "arn:aws:bedrock-agentcore:us-west-2:800000000022:runtime/w-abc";
const CONFLICT: &[u8] =
    br#"{"__type":"RetryableConflictException","message":"Session operation in progress, please retry"}"#;

fn replay_raw(responses: Vec<(u16, Vec<u8>, &str)>) -> StaticReplayClient {
    StaticReplayClient::new(
        responses
            .into_iter()
            .map(|(status, body, content_type)| {
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://example.invalid/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    http::Response::builder()
                        .status(status)
                        .header("content-type", content_type)
                        .body(SdkBody::from(body))
                        .unwrap(),
                )
            })
            .collect(),
    )
}

fn agentcore(r: &StaticReplayClient) -> aws_sdk_bedrockagentcore::Client {
    aws_sdk_bedrockagentcore::Client::from_conf(
        aws_sdk_bedrockagentcore::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_bedrockagentcore::config::Region::new("us-west-2"))
            .credentials_provider(aws_sdk_bedrockagentcore::config::Credentials::for_tests())
            .retry_config(aws_sdk_bedrockagentcore::config::retry::RetryConfig::disabled())
            .http_client(r.clone())
            .build(),
    )
}

#[tokio::test]
async fn invoke_posts_the_payload_to_the_runtime_session_and_returns_the_body() {
    let r = replay_raw(vec![(200, br#"{"ok":1}"#.to_vec(), "application/json")]);
    let sid = runtime_session_id(RUN, "a1", 1);
    let body = invoke_at(&agentcore(&r), ARN, LIVE, &sid, br#"{"in":1}"#.to_vec(), 0)
        .await
        .unwrap();
    assert_eq!(body, br#"{"ok":1}"#);
    let req = r.actual_requests().next().unwrap();
    assert_eq!(req.method(), "POST");
    let uri = req.uri().to_string();
    assert!(uri.contains("/invocations"), "{uri}");
    assert!(uri.contains("qualifier=live"), "{uri}");
    assert!(
        req.headers().iter().any(|(_, v)| v == sid),
        "no header carries the runtime session id"
    );
    assert_eq!(req.headers().get("content-type"), Some("application/json"));
    assert_eq!(req.body().bytes().unwrap(), br#"{"in":1}"#);
}

#[tokio::test]
async fn an_invoke_error_names_the_operation() {
    let r = replay_raw(vec![(
        400,
        br#"{"message":"bad session"}"#.to_vec(),
        "application/json",
    )]);
    let e = invoke_at(&agentcore(&r), ARN, LIVE, &"s".repeat(40), vec![], 0)
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("InvokeAgentRuntime"), "{e}");
}

#[tokio::test]
async fn stop_posts_to_the_session_on_the_live_qualifier() {
    let r = replay_raw(vec![(200, b"{}".to_vec(), "application/json")]);
    let sid = runtime_session_id(RUN, "a1", 1);
    assert!(
        stop_session_at(&agentcore(&r), ARN, LIVE, &sid, 0)
            .await
            .unwrap()
    );
    let req = r.actual_requests().next().unwrap();
    assert_eq!(req.method(), "POST");
    let uri = req.uri().to_string();
    assert!(uri.contains("/stopruntimesession"), "{uri}");
    assert!(uri.contains("qualifier=live"), "{uri}");
    assert!(
        req.headers().iter().any(|(_, v)| v == sid),
        "no header carries the runtime session id"
    );
}

#[tokio::test]
async fn stopping_a_session_that_already_ended_is_not_an_error() {
    let r = replay_raw(vec![(
        404,
        br#"{"__type":"ResourceNotFoundException","message":"session not found"}"#.to_vec(),
        "application/json",
    )]);
    let sid = runtime_session_id(RUN, "a1", 1);
    assert!(
        !stop_session_at(&agentcore(&r), ARN, LIVE, &sid, 0)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_stop_error_names_the_operation() {
    let r = replay_raw(vec![(
        403,
        br#"{"__type":"AccessDeniedException","message":"not allowed"}"#.to_vec(),
        "application/json",
    )]);
    let e = stop_session_at(&agentcore(&r), ARN, LIVE, &"s".repeat(40), 0)
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("StopRuntimeSession"), "{e}");
    assert!(e.to_string().contains("not allowed"), "{e}");
}

// ---- pinned versions and session-operation conflicts -------------------------

#[tokio::test]
async fn calls_at_a_pinned_endpoint_carry_it_as_the_qualifier() {
    let r = replay_raw(vec![
        (200, br#"{"ok":1}"#.to_vec(), "application/json"),
        (200, b"{}".to_vec(), "application/json"),
    ]);
    let sid = runtime_session_id(RUN, "a1", 1);
    let c = agentcore(&r);
    invoke_at(&c, ARN, "v7", &sid, b"{}".to_vec(), 0)
        .await
        .unwrap();
    assert!(stop_session_at(&c, ARN, "v7", &sid, 0).await.unwrap());
    for q in r.actual_requests() {
        let uri = q.uri().to_string();
        assert!(uri.contains("qualifier=v7"), "{uri}");
        assert!(!uri.contains("qualifier=live"), "{uri}");
    }
}

#[tokio::test]
async fn a_qualifier_that_is_not_an_endpoint_name_is_refused_before_any_request() {
    // AgentCore answers a bare version with ResourceNotFoundException ("No endpoint or agent
    // found with qualifier '50'"); the adapters refuse it before a call leaves.
    let r = replay_raw(vec![]);
    let sid = runtime_session_id(RUN, "a1", 1);
    let c = agentcore(&r);
    for q in ["50", "", "v-7", "7live"] {
        let e = invoke_at(&c, ARN, q, &sid, b"{}".to_vec(), 0)
            .await
            .unwrap_err();
        assert!(
            matches!(&e, AwsError::Qualifier { op: "InvokeAgentRuntime", qualifier } if qualifier == q),
            "{e:?}"
        );
        let e = stop_session_at(&c, ARN, q, &sid, 0).await.unwrap_err();
        assert!(
            matches!(
                &e,
                AwsError::Qualifier {
                    op: "StopRuntimeSession",
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(e.to_string().contains("not an endpoint name"), "{e}");
    }
    assert_eq!(r.actual_requests().count(), 0);
}

#[tokio::test]
async fn a_session_being_torn_down_is_retried_until_it_answers() {
    // After a planned handoff's stop, the next operation on the session answers 409 briefly.
    let r = replay_raw(vec![
        (409, CONFLICT.to_vec(), "application/json"),
        (409, CONFLICT.to_vec(), "application/json"),
        (200, br#"{"ok":1}"#.to_vec(), "application/json"),
        (409, CONFLICT.to_vec(), "application/json"),
        (200, b"{}".to_vec(), "application/json"),
    ]);
    let sid = runtime_session_id(RUN, "a1", 1);
    let c = agentcore(&r);
    assert_eq!(
        invoke_at(&c, ARN, "v7", &sid, b"{}".to_vec(), 0)
            .await
            .unwrap(),
        br#"{"ok":1}"#
    );
    assert!(stop_session_at(&c, ARN, "v7", &sid, 0).await.unwrap());
    assert_eq!(r.actual_requests().count(), 5);
}

#[tokio::test]
async fn a_conflict_that_outlasts_the_attempts_is_an_error_naming_the_operation() {
    let r = replay_raw(
        (0..CONFLICT_ATTEMPTS)
            .map(|_| (409, CONFLICT.to_vec(), "application/json"))
            .collect(),
    );
    let e = invoke_at(&agentcore(&r), ARN, "v7", &"s".repeat(40), vec![], 0)
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("InvokeAgentRuntime"), "{e}");
    assert_eq!(r.actual_requests().count(), CONFLICT_ATTEMPTS as usize);
    // The waits double from the base: 1, 2, 4, ... seconds.
    assert_eq!(conflict_backoff_ms(1, 1_000), 1_000);
    assert_eq!(conflict_backoff_ms(3, 1_000), 4_000);
    assert_eq!(conflict_backoff_ms(6, 1_000), 32_000);
    assert_eq!(conflict_backoff_ms(4, 0), 0);
}

// ---- DeleteCapacityProviderSession: the call that ends an Instances session and its billing --------------------------------

const PROVIDER: &str = "hamh_direct_instances-AbCdEfGhIj";

fn provider_arn() -> String {
    format!("arn:aws:bedrock-agentcore:us-east-1:123456789012:capacity-provider/{PROVIDER}")
}

#[tokio::test]
async fn deleting_a_provider_session_sends_the_provider_and_the_session_and_reads_the_status() {
    let body = format!(
        r#"{{"capacityProviderArn":"{}","sessionId":"s1","status":"Deprovisioning"}}"#,
        provider_arn()
    );
    let r = replay_raw(vec![(200, body.into_bytes(), "application/json")]);
    let sid = runtime_session_id(RUN, "direct", 1);
    assert!(
        delete_capacity_provider_session(&agentcore(&r), PROVIDER, &sid)
            .await
            .unwrap(),
        "accepted: the session is being deleted"
    );
    let req = r.actual_requests().next().unwrap();
    let uri = req.uri().to_string();
    assert!(
        uri.contains(PROVIDER) && uri.contains(&sid),
        "the provider id and the session id are in the request: {uri}"
    );
    assert_eq!(req.method(), "DELETE", "{uri}");
}

#[tokio::test]
async fn a_provider_session_that_is_already_gone_is_not_an_error() {
    let r = replay_raw(vec![(
        404,
        br#"{"__type":"ResourceNotFoundException","message":"no such session"}"#.to_vec(),
        "application/json",
    )]);
    let sid = runtime_session_id(RUN, "direct", 1);
    assert!(
        !delete_capacity_provider_session(&agentcore(&r), PROVIDER, &sid)
            .await
            .unwrap(),
        "false: it had already ended"
    );
}

#[tokio::test]
async fn a_delete_error_names_the_operation_and_an_id_that_is_not_an_id_never_leaves() {
    let r = replay_raw(vec![(
        403,
        br#"{"__type":"AccessDeniedException","message":"not allowed"}"#.to_vec(),
        "application/json",
    )]);
    let sid = runtime_session_id(RUN, "direct", 1);
    let e = delete_capacity_provider_session(&agentcore(&r), PROVIDER, &sid)
        .await
        .unwrap_err();
    assert!(
        e.to_string().starts_with("DeleteCapacityProviderSession:"),
        "{e}"
    );
    // A provider id with a slash or a space is refused before anything is sent.
    let none = replay_raw(vec![]);
    for bad in ["", "a/b-1234567890", "has space-1234567890", "x"] {
        let e = delete_capacity_provider_session(&agentcore(&none), bad, &sid)
            .await
            .unwrap_err();
        assert!(matches!(e, AwsError::Call { .. }), "{bad:?}: {e}");
    }
    assert_eq!(none.actual_requests().count(), 0, "nothing was sent");
}
