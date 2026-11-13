//! The utility model's Converse call from the Lambda against a loopback stub of the service: the request is SigV4-signed
//! for the `bedrock` service with the function's own credentials, goes to `/model/<encoded route>/converse` with the body the caller
//! built and the limit the caller set, and every answer the live service gave in the recordings (`crates/ham-core/tests/fixtures/
//! runname/replies/`) becomes the reply or the error class the retry rule reads.
#![allow(clippy::unwrap_used)]

use aws_credential_types::Credentials;
use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider, future};
use ham_core::utility::{UtilityFault, UtilityModels, request_body};
use run_session::utility::{BedrockConverse, UtilityCall, UtilityClient, converse_url};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const HAIKU: &str = "us.anthropic.claude-haiku-5-5";

/// A stand-in key id, built from two pieces so that no file holds a probe-shaped literal.
fn key_id() -> String {
    ["ASIA", "-test-0123456789"].concat()
}

const TOKEN: &str = "test-session-token-not-real";

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../ham-core/tests/fixtures/runname/replies/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn credentials() -> SharedCredentialsProvider {
    SharedCredentialsProvider::new(Credentials::new(
        key_id(),
        "test-secret-not-real",
        Some(TOKEN.to_string()),
        None,
        "test",
    ))
}

/// What the stub answers to one request.
#[derive(Clone, Debug)]
struct Answer {
    status: u16,
    error_type: Option<String>,
    body: String,
    /// Accept the request and never answer.
    hang: bool,
}

impl Answer {
    fn ok(body: String) -> Answer {
        Answer {
            status: 200,
            error_type: None,
            body,
            hang: false,
        }
    }
    fn hang() -> Answer {
        Answer {
            status: 200,
            error_type: None,
            body: String::new(),
            hang: true,
        }
    }
}

/// One request the stub saw: the request line, the headers (lower-case names) and the body.
#[derive(Clone, Debug)]
struct Seen {
    line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A stub that answers each request with the next of `answers` (the last one repeats).
async fn stub(answers: Vec<Answer>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let log = log.clone();
            let answers = answers.clone();
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
                let index = {
                    let mut l = log.lock().unwrap();
                    l.push(Seen {
                        line,
                        headers,
                        body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
                    });
                    l.len() - 1
                };
                let answer = answers[index.min(answers.len() - 1)].clone();
                if answer.hang {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    return;
                }
                let mut resp = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                    answer.status,
                    answer.body.len()
                );
                if let Some(t) = &answer.error_type {
                    resp.push_str(&format!("x-amzn-errortype: {t}\r\n"));
                }
                resp.push_str("\r\n");
                resp.push_str(&answer.body);
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (base, seen)
}

fn model_body() -> Value {
    request_body(
        UtilityModels::builtin().default_model(),
        "system text",
        "<request>\nuser text\n</request>",
        64,
        &["</title>"],
    )
}

async fn converse(
    client: &BedrockConverse,
    route: &str,
    body: &Value,
    timeout: Duration,
) -> Result<ham_core::utility::UtilityReply, UtilityFault> {
    client
        .converse(&UtilityCall {
            route,
            region: "us-east-1",
            body,
            timeout,
        })
        .await
}

#[tokio::test]
async fn a_call_is_one_signed_converse_request() {
    let (base, seen) = stub(vec![Answer::ok(fixture("ok-no-close-tag"))]).await;
    let client = BedrockConverse::new(credentials()).with_base(base);
    let body = model_body();
    let reply = converse(&client, HAIKU, &body, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(reply.stop_reason, "stop_sequence");
    assert_eq!(
        reply.text,
        "<title>Exponential backoff retry for S3 uploader"
    );
    assert_eq!((reply.input_tokens, reply.output_tokens), (454, 22));

    let s = seen.lock().unwrap().clone();
    assert_eq!(s.len(), 1);
    assert_eq!(
        s[0].line,
        "POST /model/us.anthropic.claude-haiku-5-5/converse HTTP/1.1"
    );
    let auth = s[0].header("authorization").unwrap();
    assert!(
        auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential={}/", key_id())),
        "{auth}"
    );
    assert!(
        auth.contains("/us-east-1/bedrock/aws4_request"),
        "signed for the bedrock service: {auth}"
    );
    assert_eq!(
        s[0].header("x-amz-security-token"),
        Some(TOKEN),
        "the function's session token is signed in"
    );
    assert_eq!(s[0].header("content-type"), Some("application/json"));
    assert_eq!(
        serde_json::from_str::<Value>(&s[0].body).unwrap(),
        body,
        "the body is the one the caller built"
    );
    // No credential or prompt text in a place the service logs differently: the secret key never travels.
    assert!(
        !s[0]
            .headers
            .iter()
            .any(|(_, v)| v.contains("test-secret-not-real"))
    );
}

#[tokio::test]
async fn the_route_is_one_percent_encoded_path_segment() {
    let (base, seen) = stub(vec![Answer::ok(fixture("ok-no-close-tag"))]).await;
    let client = BedrockConverse::new(credentials()).with_base(base);
    converse(&client, "a:b/c", &model_body(), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        seen.lock().unwrap()[0].line,
        "POST /model/a%3Ab%2Fc/converse HTTP/1.1"
    );
}

#[test]
fn the_default_endpoint_is_the_regions_bedrock_runtime() {
    assert_eq!(
        converse_url(None, "us-east-1", HAIKU),
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/us.anthropic.claude-haiku-5-5/converse"
    );
    assert_eq!(
        converse_url(Some("http://127.0.0.1:9/"), "us-east-1", "a:b/c"),
        "http://127.0.0.1:9/model/a%3Ab%2Fc/converse",
        "a test base replaces the host, a trailing slash is dropped, the route is encoded"
    );
}

#[tokio::test]
async fn recorded_replies_parse() {
    for (file, stop, text) in [
        (
            "ok-no-close-tag",
            "stop_sequence",
            "<title>Exponential backoff retry for S3 uploader",
        ),
        (
            "reasoning-first",
            "stop_sequence",
            "<title>Ratenbegrenzung im Bestell-Service samt Tests",
        ),
        (
            "two-text-blocks",
            "stop_sequence",
            "<title>Add exponential backoff retry to S3 uploader",
        ),
        ("max-tokens-reasoning-only", "max_tokens", ""),
        ("content-filtered", "content_filtered", ""),
        (
            "untagged-lines",
            "end_turn",
            "Here is a name for the run:\nAdd exponential backoff retry to S3 uploader",
        ),
    ] {
        let (base, _seen) = stub(vec![Answer::ok(fixture(file))]).await;
        let client = BedrockConverse::new(credentials()).with_base(base);
        let reply = converse(&client, HAIKU, &model_body(), Duration::from_secs(5))
            .await
            .unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(
            (reply.stop_reason.as_str(), reply.text.as_str()),
            (stop, text),
            "{file}"
        );
    }
    // A 200 with no output at all is an answer without text.
    let (base, _seen) = stub(vec![Answer::ok(fixture("no-output"))]).await;
    let client = BedrockConverse::new(credentials()).with_base(base);
    assert_eq!(
        converse(&client, HAIKU, &model_body(), Duration::from_secs(5)).await,
        Err(UtilityFault::NoText)
    );
    // A 200 that is no JSON is a service fault, not a name.
    let (base, _seen) = stub(vec![Answer::ok("<html>gateway</html>".into())]).await;
    let client = BedrockConverse::new(credentials()).with_base(base);
    assert_eq!(
        converse(&client, HAIKU, &model_body(), Duration::from_secs(5)).await,
        Err(UtilityFault::Service)
    );
}

#[tokio::test]
async fn every_recorded_error_has_its_class() {
    let errors: Vec<Value> = serde_json::from_str(&fixture("errors")).unwrap();
    assert!(errors.len() >= 9);
    for e in errors {
        let want = match e["expect"].as_str().unwrap() {
            "config" => UtilityFault::Config,
            "throttle" => UtilityFault::Throttle,
            "service" => UtilityFault::Service,
            "timeout" => UtilityFault::Timeout,
            other => panic!("unknown class {other}"),
        };
        let (base, _seen) = stub(vec![Answer {
            status: e["status"].as_u64().unwrap() as u16,
            error_type: e["errortype"].as_str().map(str::to_string),
            body: json!({"message": e["message"]}).to_string(),
            hang: false,
        }])
        .await;
        let client = BedrockConverse::new(credentials()).with_base(base);
        let got = converse(&client, HAIKU, &model_body(), Duration::from_secs(5)).await;
        assert_eq!(got, Err(want), "{}", e["name"]);
    }
}

#[tokio::test]
async fn a_hang_past_the_limit_is_a_timeout_and_a_closed_port_is_a_service_fault() {
    let (base, seen) = stub(vec![Answer::hang()]).await;
    let client = BedrockConverse::new(credentials()).with_base(base);
    let started = Instant::now();
    let got = converse(&client, HAIKU, &model_body(), Duration::from_millis(150)).await;
    assert_eq!(got, Err(UtilityFault::Timeout));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(seen.lock().unwrap().len(), 1, "the call reached the stub");

    // Nothing listens: the connection is refused at once, which is weather, so a retry is worth one more try.
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let client = BedrockConverse::new(credentials()).with_base(closed);
    let got = converse(&client, HAIKU, &model_body(), Duration::from_secs(5)).await;
    assert_eq!(got, Err(UtilityFault::Service));
}

#[derive(Debug)]
struct NoCredentials;

impl ProvideCredentials for NoCredentials {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::ready(Err(CredentialsError::not_loaded("no credentials here")))
    }
}

#[tokio::test]
async fn a_function_without_credentials_is_a_configuration_fault_and_sends_nothing() {
    let (base, seen) = stub(vec![Answer::ok(fixture("ok-no-close-tag"))]).await;
    let client =
        BedrockConverse::new(SharedCredentialsProvider::new(NoCredentials)).with_base(base);
    let got = converse(&client, HAIKU, &model_body(), Duration::from_secs(5)).await;
    assert_eq!(got, Err(UtilityFault::Config));
    assert!(seen.lock().unwrap().is_empty());
}
