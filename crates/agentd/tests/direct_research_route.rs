//! The signed call from agentd to the research route. agentd signs a POST with the run's session
//! credentials (SigV4, service `execute-api`) to `<internal API>/internal/research/<op>`, the private path the VM already has. The
//! answer is read in the route's own words (`ResearchError`); an answer that is not in them is read by its status, and the failure says
//! whether asking again can help.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::creds::CredStore;
use agentd::direct::research::route::{FailureKind, HttpRoute, Route};
use agentd::time::{SystemClock, format_rfc3339, now_unix};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use common::{CANARY_NONCE, CANARY_SECRET, CANARY_TOKEN, creds};
use harness_protocol::research::ResearchErrorCode;
use harness_protocol::research::route as paths;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Seen {
    requests: Vec<(String, HeaderMap, Vec<u8>)>,
    replies: VecDeque<(u16, String)>,
}

type Shared = Arc<Mutex<Seen>>;

async fn handler(
    State(s): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let mut g = s.lock().unwrap();
    g.requests
        .push((uri.path().to_string(), headers, body.to_vec()));
    let (status, text) = g
        .replies
        .pop_front()
        .unwrap_or((500, "no scripted reply".to_string()));
    (
        StatusCode::from_u16(status).unwrap(),
        [("content-type", "application/json")],
        text,
    )
}

async fn plane() -> (String, Shared) {
    let seen: Shared = Arc::default();
    let app = Router::new().fallback(handler).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

async fn route_to(base: &str) -> HttpRoute {
    let store = CredStore::new(
        Arc::new(agentd::creds::HttpRefresher::new()),
        Arc::new(SystemClock),
    );
    store
        .install(
            agentd::creds::RefreshContext {
                run_id: "r".into(),
                agent_id: "direct".into(),
                runtime_session_id: "x".repeat(40),
                internal_api_url: base.to_string(),
                region: "us-east-1".into(),
            },
            creds(&format_rfc3339(now_unix() + 3600)),
            CANARY_NONCE.into(),
        )
        .await;
    HttpRoute::new(store, format!("{base}/live"), "us-east-1".into())
}

fn reply(plane: &Shared, status: u16, body: &str) {
    plane
        .lock()
        .unwrap()
        .replies
        .push_back((status, body.to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_is_a_signed_post_of_the_json_body_to_the_route_path() {
    let (base, seen) = plane().await;
    let route = route_to(&base).await;
    reply(&seen, 200, r#"{"id":"s-0001"}"#);
    let sent = json!({"query": "q", "idempotencyKey": "k1"});
    let got = route.call(paths::SEARCH, &sent).await.unwrap();
    assert_eq!(got, json!({"id": "s-0001"}));
    let g = seen.lock().unwrap();
    let (path, headers, body) = &g.requests[0];
    assert_eq!(path, "/live/internal/research/search");
    assert_eq!(serde_json::from_slice::<Value>(body).unwrap(), sent);
    let auth = headers["authorization"].to_str().unwrap();
    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential="), "{auth}");
    assert!(
        auth.contains("/us-east-1/execute-api/aws4_request"),
        "{auth}"
    );
    assert!(headers.contains_key("x-amz-date"));
    assert_eq!(headers["x-amz-security-token"], CANARY_TOKEN);
    assert!(
        !auth.contains(CANARY_SECRET),
        "the secret key is never sent"
    );
    assert_eq!(headers["content-type"], "application/json");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_route_path_is_called_as_named() {
    let (base, seen) = plane().await;
    let route = route_to(&base).await;
    for p in paths::ALL {
        reply(&seen, 200, "{}");
        route.call(p, &json!({})).await.unwrap();
    }
    let got: Vec<String> = seen
        .lock()
        .unwrap()
        .requests
        .iter()
        .map(|r| r.0.clone())
        .collect();
    let want: Vec<String> = paths::ALL.iter().map(|p| format!("/live{p}")).collect();
    assert_eq!(got, want);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_202_is_an_answer_too() {
    let (base, seen) = plane().await;
    let route = route_to(&base).await;
    reply(&seen, 202, r#"{"jobId":"j","state":"queued"}"#);
    let got = route.call(paths::REPO, &json!({})).await.unwrap();
    assert_eq!(got["state"], "queued");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_in_the_routes_words_keeps_its_code_and_hint_and_is_final_unless_it_is_a_throttle()
 {
    let (base, seen) = plane().await;
    let route = route_to(&base).await;
    let cases = [
        (403, ResearchErrorCode::RefusedSecret, FailureKind::Final),
        (503, ResearchErrorCode::Disabled, FailureKind::Final),
        (429, ResearchErrorCode::LimitCalls, FailureKind::Final),
        (429, ResearchErrorCode::LimitRate, FailureKind::Transient),
        (
            429,
            ResearchErrorCode::UpstreamRateLimited,
            FailureKind::Transient,
        ),
        (502, ResearchErrorCode::UpstreamError, FailureKind::Final),
        (413, ResearchErrorCode::TooLarge, FailureKind::Final),
        (500, ResearchErrorCode::Internal, FailureKind::Transient),
    ];
    for (status, code, kind) in cases {
        reply(
            &seen,
            status,
            &json!({"code": code.as_str(), "message": "the route says", "retryAfterS": 7, "requestId": "req-1"}).to_string(),
        );
        let f = route.call(paths::SEARCH, &json!({})).await.unwrap_err();
        assert_eq!(f.error.code, code, "{status}");
        assert_eq!(f.error.message, "the route says");
        assert_eq!(f.error.retry_after_s, Some(7));
        assert_eq!(f.kind, kind, "{code}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_is_not_in_the_routes_words_is_read_by_its_status() {
    let (base, seen) = plane().await;
    let route = route_to(&base).await;
    // The gateway's own refusals (no such route, or no permission to call it) say the route is not available to this run.
    for (status, body) in [
        (403, r#"{"message":"Missing Authentication Token"}"#),
        (
            403,
            r#"{"message":"not authorized to perform: execute-api:Invoke"}"#,
        ),
        (404, r#"{"message":"Not Found"}"#),
        (405, ""),
    ] {
        reply(&seen, status, body);
        let f = route.call(paths::LIMITS, &json!({})).await.unwrap_err();
        assert_eq!(f.kind, FailureKind::Absent, "{status}");
        assert_eq!(f.error.code, ResearchErrorCode::Disabled, "{status}");
    }
    // A server fault or a throttle with no body of ours can be tried again.
    for status in [500, 502, 503, 504, 429] {
        reply(&seen, status, "upstream connect error");
        let f = route.call(paths::SEARCH, &json!({})).await.unwrap_err();
        assert_eq!(f.kind, FailureKind::Transient, "{status}");
    }
    // Anything else that is not ours is final.
    reply(&seen, 400, r#"{"message":"bad"}"#);
    let f = route.call(paths::SEARCH, &json!({})).await.unwrap_err();
    assert_eq!(f.kind, FailureKind::Final);
    assert_eq!(f.error.code, ResearchErrorCode::Internal);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_200_that_is_not_json_is_a_final_internal_failure() {
    let (base, seen) = plane().await;
    let route = route_to(&base).await;
    reply(&seen, 200, "<html>");
    let f = route.call(paths::SEARCH, &json!({})).await.unwrap_err();
    assert_eq!(f.error.code, ResearchErrorCode::Internal);
    assert_eq!(f.kind, FailureKind::Final);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_that_cannot_be_made_can_be_tried_again() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let route = route_to(&base).await;
    let f = route.call(paths::SEARCH, &json!({})).await.unwrap_err();
    assert_eq!(f.kind, FailureKind::Transient);
    assert_eq!(f.error.code, ResearchErrorCode::Internal);
    assert!(!f.error.message.contains(CANARY_TOKEN));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_session_credentials_the_call_fails_for_good_and_names_no_secret() {
    let store = CredStore::new(
        Arc::new(agentd::creds::HttpRefresher::new()),
        Arc::new(SystemClock),
    );
    let route = HttpRoute::new(store, "http://127.0.0.1:9".into(), "us-east-1".into());
    let f = route.call(paths::SEARCH, &json!({})).await.unwrap_err();
    assert_eq!(f.kind, FailureKind::Final);
    assert_eq!(f.error.code, ResearchErrorCode::Internal);
}
