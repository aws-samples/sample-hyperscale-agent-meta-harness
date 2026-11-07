//! The small test request of provider failover: what agentd sends to a route while every route has failed, how
//! the answer is read, and that nothing but a one-message Converse call with a few output tokens leaves the VM.
#![allow(clippy::unwrap_used)]

use agentd::direct::creds_fn::CredsFn;
use agentd::direct::failover::{ProbeAnswer, Route, RouteProbe};
use agentd::direct::probe::{BedrockProbe, answer_for};
use harness_protocol::api::internal::ProcessCredentials;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn route(id: &str) -> Route {
    Route {
        id: id.into(),
        region: "us-east-1".into(),
        context_tokens: 1_000_000,
        max_output_tokens: 128_000,
    }
}

fn creds() -> CredsFn {
    Arc::new(|| {
        Box::pin(async {
            Ok(ProcessCredentials {
                version: 1,
                access_key_id: "ASIA-test-0123456789".into(),
                secret_access_key: "test-secret-not-real".into(),
                session_token: "test-session-token-not-real".into(),
                expiration: "2026-10-15T19:10:00Z".into(),
            })
        })
    })
}

/// One request the stub saw: the request line, the headers (lower case names) and the body.
#[derive(Clone, Debug)]
struct Seen {
    line: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// A stub that answers every request with `status`, an optional `x-amzn-errortype` and `body`.
async fn stub(
    status: u16,
    error_type: Option<&'static str>,
    body: &'static str,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, content_length) = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let cl = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (i + 4, cl);
                    }
                };
                while buf.len() < head_end + content_length {
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end - 4]).to_string();
                let mut lines = head.lines();
                let line = lines.next().unwrap_or_default().to_string();
                let headers = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                    .collect();
                log.lock().unwrap().push(Seen {
                    line,
                    headers,
                    body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
                });
                let mut resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                    body.len()
                );
                if let Some(t) = error_type {
                    resp.push_str(&format!("x-amzn-errortype: {t}\r\n"));
                }
                resp.push_str("\r\n");
                resp.push_str(body);
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (base, seen)
}

#[tokio::test]
async fn the_probe_is_one_signed_converse_call_with_one_message_and_a_few_output_tokens() {
    let (base, seen) = stub(200, None, r#"{"output":{"message":{"role":"assistant","content":[{"text":"OK"}]}},"stopReason":"end_turn"}"#).await;
    let probe = BedrockProbe::new(creds()).with_base(base);
    assert_eq!(
        probe.probe(&route("us.anthropic.claude-opus-5-5")).await,
        ProbeAnswer::Healthy
    );
    let s = seen.lock().unwrap().clone();
    assert_eq!(s.len(), 1);
    assert_eq!(
        s[0].line,
        "POST /model/us.anthropic.claude-opus-5-5/converse HTTP/1.1"
    );
    let auth = s[0]
        .headers
        .iter()
        .find(|(k, _)| k == "authorization")
        .map(|(_, v)| v.as_str())
        .unwrap();
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=ASIA-test-0123456789/"),
        "{auth}"
    );
    assert!(auth.contains("/us-east-1/bedrock/aws4_request"), "{auth}");
    assert!(
        s[0].headers
            .iter()
            .any(|(k, _)| k == "x-amz-security-token")
    );
    let body: Value = serde_json::from_str(&s[0].body).unwrap();
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["inferenceConfig"]["maxTokens"], 16);
    assert!(body.get("toolConfig").is_none() && body.get("system").is_none());
}

#[tokio::test]
async fn a_model_id_with_a_colon_is_encoded_in_the_path() {
    let (base, seen) = stub(200, None, "{}").await;
    let probe = BedrockProbe::new(creds()).with_base(base);
    let _ = probe
        .probe(&route("anthropic.claude-3-5-sonnet-20241022-v2:0"))
        .await;
    assert!(
        seen.lock().unwrap()[0]
            .line
            .starts_with("POST /model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse ")
    );
}

#[tokio::test]
async fn throttling_and_server_errors_mean_the_route_is_down() {
    for (status, kind) in [
        (429, "ThrottlingException"),
        (503, "ServiceUnavailableException"),
        (500, "InternalServerException"),
        (424, "ModelNotReadyException"),
        (408, "ModelTimeoutException"),
    ] {
        let (base, _) = stub(status, Some(kind), r#"{"message":"later"}"#).await;
        let a = BedrockProbe::new(creds())
            .with_base(base)
            .probe(&route("x.y"))
            .await;
        assert!(matches!(a, ProbeAnswer::Down(_)), "{status} {kind}: {a:?}");
    }
}

#[tokio::test]
async fn a_missing_model_or_an_access_denial_means_the_route_is_refused() {
    for (status, kind, body) in [
        (
            404,
            "ResourceNotFoundException",
            r#"{"message":"no such model"}"#,
        ),
        (403, "AccessDeniedException", r#"{"message":"not allowed"}"#),
        (
            400,
            "ValidationException",
            r#"{"message":"The provided model identifier is invalid."}"#,
        ),
        (
            400,
            "ValidationException",
            r#"{"message":"Invocation of model ID x with on-demand throughput isn't supported."}"#,
        ),
    ] {
        let (base, _) = stub(status, Some(kind), body).await;
        let a = BedrockProbe::new(creds())
            .with_base(base)
            .probe(&route("x.y"))
            .await;
        assert!(
            matches!(a, ProbeAnswer::Refused(_)),
            "{status} {kind}: {a:?}"
        );
    }
}

#[tokio::test]
async fn any_other_answer_shows_the_service_is_reachable_and_counts_as_healthy() {
    // A 400 about the probe's own shape (a limit this model wants higher) is the probe's problem; the route answered.
    let (base, _) = stub(
        400,
        Some("ValidationException"),
        r#"{"message":"maxTokens must be at least 64"}"#,
    )
    .await;
    let a = BedrockProbe::new(creds())
        .with_base(base)
        .probe(&route("x.y"))
        .await;
    assert_eq!(a, ProbeAnswer::Healthy);
}

#[tokio::test]
async fn an_unreachable_endpoint_and_missing_credentials_mean_down_and_say_nothing_secret() {
    let a = BedrockProbe::new(creds())
        .with_base("http://127.0.0.1:1")
        .probe(&route("x.y"))
        .await;
    assert!(matches!(a, ProbeAnswer::Down(_)), "{a:?}");
    let no: CredsFn = Arc::new(|| Box::pin(async { Err("no credentials installed".to_string()) }));
    let a = BedrockProbe::new(no)
        .with_base("http://127.0.0.1:1")
        .probe(&route("x.y"))
        .await;
    match a {
        ProbeAnswer::Down(d) => assert!(d.contains("credentials"), "{d}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_answers_are_read_by_status_and_error_type() {
    assert_eq!(answer_for(200, None, b""), ProbeAnswer::Healthy);
    assert!(matches!(answer_for(429, None, b""), ProbeAnswer::Down(_)));
    assert!(matches!(
        answer_for(500, Some("InternalServerException"), b""),
        ProbeAnswer::Down(_)
    ));
    assert!(matches!(
        answer_for(403, Some("ExpiredTokenException"), b""),
        ProbeAnswer::Down(_)
    ));
    assert!(matches!(
        answer_for(
            403,
            Some("AccessDeniedException:http://internal.amazon.com/coral/"),
            b""
        ),
        ProbeAnswer::Refused(_)
    ));
    assert!(matches!(
        answer_for(404, None, b""),
        ProbeAnswer::Refused(_)
    ));
    assert_eq!(
        answer_for(400, Some("ValidationException"), b"maxTokens"),
        ProbeAnswer::Healthy
    );
}
