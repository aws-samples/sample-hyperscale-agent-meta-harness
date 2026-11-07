//! SigV4-signed HTTP: /internal/creds refresh, /internal/events and heartbeats, and S3
//! GET/PUT, against a local server standing in for API Gateway and S3.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::creds::{CredStore, HttpRefresher, RefreshContext, Refresher};
use agentd::events::{EventSink, HttpEventSink};
use agentd::http::{SignedHttp, SignedRequest, signed_headers, signed_headers_with};
use agentd::s3::{PutNew, S3Objects, object_url};
use agentd::time::{SystemClock, format_rfc3339, now_unix};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use common::{CANARY_NONCE, CANARY_SECRET, CANARY_TOKEN, canary_key_id, creds, scratch};
use harness_protocol::api::internal::{CredsRequest, EventsBatchRequest, HeartbeatRequest};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[derive(Default)]
struct Seen {
    requests: Vec<(String, HashMap<String, String>, Vec<u8>)>,
    objects: HashMap<String, Vec<u8>>,
    deny_creds: bool,
}

type S = Arc<Mutex<Seen>>;

/// The key ID the fake refresh endpoint issues: `ASIA` and 16 base-32 characters, joined at run time so no
/// AWS key-ID-shaped literal is committed.
fn refreshed_key_id() -> String {
    format!("ASIA{}", "REFRESHEDBYAPIGW")
}

fn record(s: &S, path: &str, h: &HeaderMap, body: &[u8]) {
    let headers = h
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    s.lock()
        .unwrap()
        .requests
        .push((path.to_string(), headers, body.to_vec()));
}

async fn server() -> (String, S) {
    let seen: S = Arc::default();
    let app = Router::new()
        .route(
            "/live/internal/creds",
            post(|State(s): State<S>, h: HeaderMap, b: Bytes| async move {
                record(&s, "/live/internal/creds", &h, &b);
                if s.lock().unwrap().deny_creds {
                    return (
                        StatusCode::FORBIDDEN,
                        "{\"message\":\"revoked\"}".to_string(),
                    );
                }
                let mut c = creds(&format_rfc3339(now_unix() + 3600));
                c.access_key_id = refreshed_key_id();
                (
                    StatusCode::OK,
                    serde_json::json!({"credentials": c, "nextNonce": "n2"}).to_string(),
                )
            }),
        )
        .route(
            "/live/internal/events",
            post(|State(s): State<S>, h: HeaderMap, b: Bytes| async move {
                record(&s, "/live/internal/events", &h, &b);
                let n = serde_json::from_slice::<EventsBatchRequest>(&b)
                    .map(|r| r.events.len())
                    .unwrap_or(0);
                (
                    StatusCode::OK,
                    serde_json::json!({"accepted": n, "pending": [{"kind": "cancel"}]}).to_string(),
                )
            }),
        )
        .route(
            "/live/internal/heartbeat",
            post(|State(s): State<S>, h: HeaderMap, b: Bytes| async move {
                record(&s, "/live/internal/heartbeat", &h, &b);
                (StatusCode::OK, "{\"ok\":true}".to_string())
            }),
        )
        .route(
            "/{bucket}/{*key}",
            get(
                |State(s): State<S>, AxPath((b, k)): AxPath<(String, String)>| async move {
                    match s.lock().unwrap().objects.get(&format!("{b}/{k}")) {
                        Some(v) => (StatusCode::OK, v.clone()),
                        None => (StatusCode::NOT_FOUND, b"NoSuchKey".to_vec()),
                    }
                },
            )
            .put(
                |State(s): State<S>,
                 AxPath((b, k)): AxPath<(String, String)>,
                 h: HeaderMap,
                 body: Bytes| async move {
                    record(&s, &format!("/{b}/{k}"), &h, &body);
                    let mut seen = s.lock().unwrap();
                    // S3's conditional write: `If-None-Match: *` answers 412 when the key already holds an object (the
                    // fake of `direct_support` has the same branch).
                    if h.get("if-none-match").and_then(|v| v.to_str().ok()) == Some("*")
                        && seen.objects.contains_key(&format!("{b}/{k}"))
                    {
                        return (
                            StatusCode::PRECONDITION_FAILED,
                            "<Error><Code>PreconditionFailed</Code><Message>At least one of the pre-conditions you specified did not hold</Message></Error>".to_string(),
                        );
                    }
                    seen.objects.insert(format!("{b}/{k}"), body.to_vec());
                    (StatusCode::OK, String::new())
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn ctx(base: &str) -> RefreshContext {
    RefreshContext {
        run_id: "run1".into(),
        agent_id: "a1".into(),
        runtime_session_id: "run1-a1-session-000000000000000000001".into(),
        internal_api_url: format!("{base}/live"),
        region: "us-west-2".into(),
    }
}

fn assert_sigv4(h: &HashMap<String, String>, service: &str) {
    let auth = &h["authorization"];
    assert!(
        auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential={}/", canary_key_id())),
        "{auth}"
    );
    assert!(
        auth.contains(&format!("/us-west-2/{service}/aws4_request")),
        "{auth}"
    );
    assert!(
        auth.contains("SignedHeaders=")
            && auth.contains("x-amz-date")
            && auth.contains("x-amz-security-token"),
        "{auth}"
    );
    assert_eq!(h["x-amz-security-token"], CANARY_TOKEN);
    for v in h.values() {
        assert!(!v.contains(CANARY_SECRET), "secret in a header");
    }
}

/// The names in the `SignedHeaders=` list of an Authorization value.
fn signed_names(auth: &str) -> Vec<&str> {
    auth.split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .split(';')
        .collect()
}

#[test]
fn signed_headers_scope_the_date_region_and_service() {
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_312_400);
    let c = creds("2026-09-25T06:00:00Z");
    let h: HashMap<String, String> = signed_headers(
        "POST",
        "https://abc.execute-api.us-west-2.amazonaws.com/live/internal/creds",
        "execute-api",
        "us-west-2",
        &c,
        b"{}",
        t,
    )
    .unwrap()
    .into_iter()
    .collect();
    assert!(h["authorization"].contains(&format!(
        "Credential={}/20260925/us-west-2/execute-api/aws4_request",
        canary_key_id()
    )));
    assert_eq!(h["x-amz-date"], "20260925T050000Z");
    assert!(signed_headers("GET", "not a url", "s3", "us-west-2", &c, b"", t).is_err());
}

/// An extra header given to the signer is part of the signature (S3 needs SigV4 for a conditional write and refuses an `x-amz-*`
/// header that the signature does not cover), and the helper still returns only the headers SigV4 adds: the caller puts the extra header on the request once, so it is never sent twice.
#[test]
fn signed_headers_with_signs_the_extra_headers() {
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_312_400);
    let c = creds("2026-09-25T06:00:00Z");
    let url = "https://b.s3.us-west-2.amazonaws.com/runs/r/agents/direct/turns/1/summary.json";
    let with: HashMap<String, String> = signed_headers_with(
        "PUT",
        url,
        "s3",
        "us-west-2",
        &c,
        &[("if-none-match", "*")],
        b"{}",
        t,
    )
    .unwrap()
    .into_iter()
    .collect();
    let without: HashMap<String, String> =
        signed_headers("PUT", url, "s3", "us-west-2", &c, b"{}", t)
            .unwrap()
            .into_iter()
            .collect();
    assert!(
        signed_names(&with["authorization"]).contains(&"if-none-match"),
        "{}",
        with["authorization"]
    );
    assert!(signed_names(&with["authorization"]).contains(&"x-amz-security-token"));
    assert!(!signed_names(&without["authorization"]).contains(&"if-none-match"));
    assert_ne!(
        with["authorization"], without["authorization"],
        "the condition changes the signature"
    );
    assert!(
        !with.contains_key("if-none-match"),
        "the helper returns the SigV4 headers only"
    );
    assert_eq!(
        signed_headers_with("PUT", url, "s3", "us-west-2", &c, &[], b"{}", t).unwrap(),
        signed_headers("PUT", url, "s3", "us-west-2", &c, b"{}", t).unwrap(),
        "with no extra header it is the plain signature"
    );
}

/// A create-only put (`If-None-Match: *`, signed): the first write stores, the second answers `Exists` (S3's 412) and the stored
/// bytes are the first write's. The end sequence writes the bundle, `summary.json` and `test-report.json` this way, so a process
/// that got there first is found out and never written over.
#[tokio::test]
async fn put_file_new_stores_once_and_answers_exists_on_a_taken_key() {
    let (base, seen) = server().await;
    let d = scratch("s3-new");
    let first = d.join("summary.json");
    std::fs::write(&first, b"{\"first\":true}").unwrap();
    let second = d.join("summary-2.json");
    std::fs::write(&second, b"{\"second\":true}").unwrap();
    let s3 = S3Objects::new("us-west-2".into(), Some(base.clone()));
    let c = creds(&format_rfc3339(now_unix() + 3600));
    let uri = "s3://ham-test-bucket/runs/run1/agents/direct/turns/1/summary.json";
    assert_eq!(
        s3.put_file_new(uri, &c, &first).await.unwrap(),
        PutNew::Stored
    );
    assert_eq!(
        s3.put_file_new(uri, &c, &second).await.unwrap(),
        PutNew::Exists
    );
    let back = d.join("back");
    s3.get_to_file(uri, &c, &back).await.unwrap();
    assert_eq!(
        std::fs::read(&back).unwrap(),
        b"{\"first\":true}",
        "the stored bytes are the first put's"
    );
    {
        let seen = seen.lock().unwrap();
        let puts: Vec<_> = seen
            .requests
            .iter()
            .filter(|r| r.0.ends_with("/summary.json"))
            .collect();
        assert_eq!(puts.len(), 2);
        for (_, h, _) in &puts {
            assert_sigv4(h, "s3");
            assert_eq!(h["if-none-match"], "*");
            assert!(
                signed_names(&h["authorization"]).contains(&"if-none-match"),
                "the condition is signed: {}",
                h["authorization"]
            );
        }
    }
    // A transport failure is an error, not an answer.
    let dead = S3Objects::new("us-west-2".into(), Some("http://127.0.0.1:9".into()));
    assert!(dead.put_file_new(uri, &c, &first).await.is_err());
}

#[tokio::test]
async fn refresh_posts_a_signed_creds_request_and_parses_the_response() {
    let (base, seen) = server().await;
    let c = creds(&format_rfc3339(now_unix() + 60));
    let req = CredsRequest {
        run_id: "run1".into(),
        agent_id: "a1".into(),
        runtime_session_id: "run1-a1-session-000000000000000000001".into(),
        nonce: CANARY_NONCE.into(),
    };
    let resp = HttpRefresher::new()
        .refresh(&ctx(&base), &c, req.clone())
        .await
        .unwrap();
    assert_eq!(resp.credentials.access_key_id, refreshed_key_id());
    assert_eq!(resp.next_nonce, "n2");
    let seen = seen.lock().unwrap();
    let (_, h, body) = &seen.requests[0];
    assert_sigv4(h, "execute-api");
    assert_eq!(serde_json::from_slice::<CredsRequest>(body).unwrap(), req);
}

#[tokio::test]
async fn a_refused_refresh_is_an_error_without_secrets() {
    let (base, seen) = server().await;
    seen.lock().unwrap().deny_creds = true;
    let c = creds(&format_rfc3339(now_unix() + 60));
    let req = CredsRequest {
        run_id: "run1".into(),
        agent_id: "a1".into(),
        runtime_session_id: "x".repeat(40),
        nonce: CANARY_NONCE.into(),
    };
    let err = HttpRefresher::new()
        .refresh(&ctx(&base), &c, req)
        .await
        .unwrap_err();
    assert!(err.contains("403"), "{err}");
    assert!(!err.contains(CANARY_NONCE) && !err.contains(CANARY_SECRET));
    // Unreachable endpoint: transport error.
    let mut dead = ctx("http://127.0.0.1:9");
    dead.internal_api_url = "http://127.0.0.1:9/live".into();
    let req = CredsRequest {
        run_id: "r".into(),
        agent_id: "a".into(),
        runtime_session_id: "x".repeat(40),
        nonce: "n".into(),
    };
    assert!(HttpRefresher::new().refresh(&dead, &c, req).await.is_err());
}

#[tokio::test]
async fn events_and_heartbeats_are_signed_posts_to_the_internal_api() {
    let (base, seen) = server().await;
    let store = CredStore::new(Arc::new(HttpRefresher::new()), Arc::new(SystemClock));
    store
        .install(
            ctx(&base),
            creds(&format_rfc3339(now_unix() + 3600)),
            CANARY_NONCE.into(),
        )
        .await;
    let sink = HttpEventSink::new(store, format!("{base}/live"), "us-west-2".into());
    let batch = EventsBatchRequest {
        run_id: "run1".into(),
        agent_id: "a1".into(),
        events: vec![],
        take_controls: None,
    };
    let resp = sink.post_events(batch).await.unwrap();
    assert_eq!(resp.accepted, 0);
    assert_eq!(resp.pending.len(), 1);
    sink.heartbeat(HeartbeatRequest {
        run_id: "run1".into(),
        agent_id: "a1".into(),
        turn: 1,
    })
    .await
    .unwrap();
    let seen = seen.lock().unwrap();
    let paths: Vec<&str> = seen.requests.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        paths,
        vec!["/live/internal/events", "/live/internal/heartbeat"]
    );
    for (_, h, _) in &seen.requests {
        assert_sigv4(h, "execute-api");
    }
}

#[tokio::test]
async fn s3_put_and_get_round_trip_with_signed_requests() {
    let (base, seen) = server().await;
    let d = scratch("s3");
    let src = d.join("bundle");
    std::fs::write(&src, b"git bundle bytes").unwrap();
    let s3 = S3Objects::new("us-west-2".into(), Some(base.clone()));
    let c = creds(&format_rfc3339(now_unix() + 3600));
    let uri = "s3://hamh-live-harness-800000000022/runs/run1/agents/a1/turns/1/bundle";
    s3.put_file(uri, &c, &src).await.unwrap();
    let dest = d.join("back");
    s3.get_to_file(uri, &c, &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"git bundle bytes");
    assert_sigv4(&seen.lock().unwrap().requests[0].1, "s3");
    let missing = s3
        .get_to_file(
            "s3://hamh-live-harness-800000000022/runs/run1/input/none",
            &c,
            &d.join("x"),
        )
        .await
        .unwrap_err();
    assert!(missing.contains("404"), "{missing}");
}

#[test]
fn object_urls_are_virtual_hosted_in_aws_and_path_style_on_an_endpoint() {
    assert_eq!(
        object_url("s3://b/runs/r/input/repo.bundle", "us-west-2", None).unwrap(),
        "https://b.s3.us-west-2.amazonaws.com/runs/r/input/repo.bundle"
    );
    assert_eq!(
        object_url("s3://b/k", "us-west-2", Some("http://127.0.0.1:9")).unwrap(),
        "http://127.0.0.1:9/b/k"
    );
    for bad in ["https://b/k", "s3://b", "s3:///k", "s3://b/"] {
        assert!(object_url(bad, "us-west-2", None).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn a_signed_request_reports_status_and_body() {
    let (base, _) = server().await;
    let c = creds("2099-01-01T00:00:00Z");
    let r = SignedHttp::new()
        .send(SignedRequest {
            method: "GET",
            url: &format!("{base}/b/none"),
            service: "s3",
            region: "us-west-2",
            creds: &c,
            content_type: None,
            body: vec![],
        })
        .await
        .unwrap();
    assert_eq!(r.status, 404);
    assert_eq!(r.body, b"NoSuchKey");
}
