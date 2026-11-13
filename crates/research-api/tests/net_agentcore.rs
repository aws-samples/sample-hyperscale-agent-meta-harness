//! The `agentcore` search provider against a scripted fake gateway on 127.0.0.1: the MCP requests it
//! sends (initialize once, then tools/call), the SigV4 signature, the mapping of answers and failures to the route's error words, and the
//! rule that a refused query costs no call. No test reaches the network or AWS; the credentials and the secret are stand-ins built at run time.

#![allow(clippy::unwrap_used)]

#[path = "support/net_fake.rs"]
mod net_fake;
#[path = "support/net_gateway.rs"]
mod net_gateway;

use harness_protocol::research::{ResearchErrorCode as C, ResearchProvider, TimeRange};
use net_fake::{FakeServer, Recorded, Reply};
use research_api::agentcore::{GatewayTarget, MAX_QUERY_CHARS, sign_headers};
use research_api::ports::Provider;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::{Duration, UNIX_EPOCH};

use net_gateway::*;

// ---- the requests ----------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_provider_is_agentcore_and_searches_through_initialize_then_tools_call() {
    let server = ok_gateway(3).await;
    let p = provider(&server);
    assert_eq!(p.id(), ResearchProvider::Agentcore);
    let out = p.search(&search("serde derive macros")).await.unwrap();
    assert_eq!(out.results.len(), 3);

    let seen = server.requests();
    assert_eq!(seen.len(), 2, "initialize, then one tools/call");
    for r in &seen {
        assert_eq!((r.method.as_str(), r.target.as_str()), ("POST", "/mcp"));
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(
            r.header("accept"),
            Some("application/json, text/event-stream")
        );
        assert_eq!(r.header("mcp-protocol-version"), Some("2025-03-26"));
        assert_eq!(r.header("host"), Some(GATEWAY_HOST));
        assert!(r.header("authorization").is_some());
        assert_eq!(
            r.header("x-amz-security-token"),
            Some(token().as_str()),
            "the session token travels with the signature"
        );
    }
    let init = seen[0].json();
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["method"], "initialize");
    assert_eq!(init["params"]["protocolVersion"], "2025-03-26");
    assert!(init["params"]["clientInfo"]["name"].is_string());

    let call = seen[1].json();
    assert_eq!(call["jsonrpc"], "2.0");
    assert_eq!(call["method"], "tools/call");
    assert_eq!(call["params"]["name"], "web-search-tool___WebSearch");
    assert_eq!(
        call["params"]["arguments"],
        json!({"query": "serde derive macros", "maxResults": 5}),
        "no filters member when the request names none"
    );
    assert_ne!(init["id"], call["id"], "each request has its own id");
}

#[tokio::test]
async fn initialize_runs_once_for_many_searches_and_once_for_searches_that_start_together() {
    let server = ok_gateway(1).await;
    let p = provider(&server);
    for _ in 0..3 {
        p.search(&search("q")).await.unwrap();
    }
    assert_eq!((initializes(&server), calls(&server).len()), (1, 3));

    let server = ok_gateway(1).await;
    let p = provider(&server);
    let (qa, qb, qc) = (search("a"), search("b"), search("c"));
    let (a, b, c) = tokio::join!(p.search(&qa), p.search(&qb), p.search(&qc));
    assert!(a.is_ok() && b.is_ok() && c.is_ok());
    assert_eq!(
        initializes(&server),
        1,
        "three first calls share one handshake"
    );
}

#[tokio::test]
async fn a_failed_initialize_fails_the_search_and_the_next_search_tries_again() {
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c2 = count.clone();
    let server = FakeServer::start(move |rec| match method_of(rec).as_str() {
        "initialize" => {
            if c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Reply::json(503, r#"{"message":"try later"}"#)
            } else {
                init_reply(rec)
            }
        }
        _ => tool_ok(rec, &results_text(1, "MARK")),
    })
    .await;
    let p = provider(&server);
    let err = p.search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert!(calls(&server).is_empty(), "no search without a handshake");
    p.search(&search("q")).await.unwrap();
    assert_eq!(initializes(&server), 2);
}

#[tokio::test]
async fn an_initialize_answer_for_another_protocol_version_is_an_upstream_error() {
    let server = FakeServer::start(|rec| {
        rpc(
            rec,
            json!({"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "x", "version": "1"}}),
        )
    })
    .await;
    let err = provider(&server).search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert!(err.message.contains("2025-03-26"), "{}", err.message);
    assert!(calls(&server).is_empty());
}

#[tokio::test]
async fn the_requested_result_count_is_sent_and_extra_results_are_cut() {
    let server = ok_gateway(10).await;
    let p = provider(&server);
    let mut req = search("q");
    req.max_results = Some(3);
    let out = p.search(&req).await.unwrap();
    assert_eq!(out.results.len(), 3, "the answer had 10");
    assert_eq!(
        calls(&server)[0].json()["params"]["arguments"]["maxResults"],
        3
    );
    // The gateway takes 1 to 25; the route's own cap is 10.
    req.max_results = Some(10);
    p.search(&req).await.unwrap();
    assert_eq!(
        calls(&server)[1].json()["params"]["arguments"]["maxResults"],
        10
    );
}

#[tokio::test]
async fn domains_and_the_time_range_become_the_gateway_filters() {
    let server = ok_gateway(1).await;
    let p = provider(&server);
    let mut req = search("q");
    req.include_domains = vec!["docs.rs".into(), "serde.rs".into()];
    req.time_range = Some(TimeRange::Week);
    p.search(&req).await.unwrap();
    let args = calls(&server)[0].json()["params"]["arguments"].clone();
    assert_eq!(
        args["filters"],
        json!({
            "domainFilter": {"include": ["docs.rs", "serde.rs"]},
            "publishedDateFilter": {"from": "2026-10-18T12:00:00Z"}
        }),
        "a week back from the fixed clock, no exclude list, no `to`"
    );
    for (range, from) in [
        (TimeRange::Day, "2026-10-24T12:00:00Z"),
        (TimeRange::Month, "2026-09-25T12:00:00Z"),
        (TimeRange::Year, "2025-10-25T12:00:00Z"),
    ] {
        let mut req = search("q");
        req.time_range = Some(range);
        p.search(&req).await.unwrap();
        let n = calls(&server).len();
        let args = calls(&server)[n - 1].json()["params"]["arguments"].clone();
        assert_eq!(
            args["filters"],
            json!({"publishedDateFilter": {"from": from}}),
            "{range:?}"
        );
    }
    let mut req = search("q");
    req.include_domains = vec!["docs.rs".into()];
    p.search(&req).await.unwrap();
    let n = calls(&server).len();
    let args = calls(&server)[n - 1].json()["params"]["arguments"].clone();
    assert_eq!(
        args["filters"],
        json!({"domainFilter": {"include": ["docs.rs"]}})
    );
}

// ---- the 200 character cap ------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_query_over_two_hundred_characters_is_refused_before_any_call() {
    assert_eq!(MAX_QUERY_CHARS, 200);
    let server = ok_gateway(1).await;
    let p = provider(&server);
    assert_eq!(p.max_query_chars(), Some(200));
    let err = p.search(&search(&"a".repeat(201))).await.unwrap_err();
    assert_eq!(err.code, C::Usage);
    assert!(err.message.contains("200"), "{}", err.message);
    assert_eq!(server.count(), 0, "not even the handshake");
    // 200 characters pass; the cap counts characters, not bytes.
    p.search(&search(&"a".repeat(200))).await.unwrap();
    let wide = "\u{1F980}".repeat(200);
    assert!(wide.len() > 200);
    p.search(&search(&wide)).await.unwrap();
    let err = p
        .search(&search(&"\u{1F980}".repeat(201)))
        .await
        .unwrap_err();
    assert_eq!(err.code, C::Usage);
}

// ---- the answer ------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_answer_maps_to_results_with_bounded_snippets_and_the_gateways_result_id() {
    let server = ok_gateway(10).await;
    let mut req = search("q");
    req.max_results = Some(10);
    let out = provider(&server).search(&req).await.unwrap();
    assert_eq!(out.results.len(), 10);
    let first = &out.results[0];
    assert_eq!(first.title, "Result 0");
    assert_eq!(first.url, "https://docs.example.org/page-0");
    assert_eq!(first.published.as_deref(), Some("2025-03-01"));
    assert!(first.snippet.starts_with("MARK excerpt number 0."));
    assert!(
        (900..=1024).contains(&first.snippet.chars().count()),
        "the 2,300 character excerpt is cut to 1 KiB, {} characters",
        first.snippet.chars().count()
    );
    assert_eq!(out.results[1].published, None, "\"unknown\" is no date");
    assert_eq!(out.results[0].score, None);
    assert_eq!(out.upstream_request_id.as_deref(), Some("31ff0adc"));
    assert_eq!(out.credits, None, "the Web Search Tool reports no credits");
}

#[tokio::test]
async fn a_result_without_a_url_is_skipped_and_an_empty_answer_is_an_empty_list() {
    let server = gateway(|rec| {
        tool_ok(
            rec,
            &json!({"id": "a1b2c3d4", "results": [
                {"title": "no url", "text": "x"},
                {"title": "ok", "url": "https://docs.example.org/ok", "publishedDate": "unknown", "text": "y"}
            ]})
            .to_string(),
        )
    })
    .await;
    let out = provider(&server).search(&search("q")).await.unwrap();
    assert_eq!(out.results.len(), 1);
    assert_eq!(out.results[0].title, "ok");
    let server = gateway(|rec| tool_ok(rec, r#"{"id":"a1b2c3d4","results":[]}"#)).await;
    let out = provider(&server).search(&search("q")).await.unwrap();
    assert!(out.results.is_empty());
}

#[tokio::test]
async fn an_answer_in_server_sent_event_framing_is_read() {
    let server = FakeServer::start(|rec| match method_of(rec).as_str() {
        "initialize" => init_reply(rec),
        _ => {
            let body = json!({"jsonrpc": "2.0", "id": rec.json()["id"].clone(),
                "result": {"isError": false, "content": [{"type": "text", "text": results_text(2, "MARK")}]}})
            .to_string();
            Reply::bytes(
                200,
                &[("Content-Type", "text/event-stream")],
                format!("event: message\ndata: {body}\n\n").as_bytes(),
            )
        }
    })
    .await;
    let out = provider(&server).search(&search("q")).await.unwrap();
    assert_eq!(out.results.len(), 2);
}

// ---- failures --------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_tool_error_is_an_upstream_error_with_a_short_excerpt() {
    let text = "DependencyFailedException - The web search tool received an invalid request: 1 validation error detected: \
Value at 'query' failed to satisfy constraint: Member must have length less than or equal to 200";
    let err = fail_with(move |rec| tool_err(rec, text)).await;
    assert_eq!(err.code, C::UpstreamError);
    assert!(
        err.message.contains("DependencyFailedException"),
        "{}",
        err.message
    );
    assert!(err.retry_after_s.is_none());
    let long = "x".repeat(5000);
    let err = fail_with(move |rec| tool_err(rec, &long)).await;
    assert_eq!(err.code, C::UpstreamError);
    assert!(
        err.message.matches('x').count() <= 200,
        "an excerpt of at most 200 characters, got {}",
        err.message.matches('x').count()
    );
}

#[tokio::test]
async fn a_tool_error_names_no_url_and_no_arn() {
    let err = fail_with(|rec| {
        tool_err(
            rec,
            "DependencyFailedException - failed for https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp \
in arn:aws:bedrock-agentcore:us-east-1:123456789012:gateway/gw-abc12345 with https://example.org/page",
        )
    })
    .await;
    assert_eq!(err.code, C::UpstreamError);
    for banned in [
        "https://",
        "gateway.bedrock-agentcore",
        "arn:aws",
        "123456789012",
    ] {
        assert!(!err.message.contains(banned), "{banned} in {}", err.message);
    }
}

#[tokio::test]
async fn a_throttle_in_a_tool_error_is_a_rate_limit_and_look_alikes_are_not() {
    for text in [
        "Error code 429",
        "ERROR CODE 429.",
        "ThrottlingException: Rate exceeded",
        "Too Many Requests",
        "DependencyFailedException - Error code 429: slow down",
    ] {
        let t = text.to_string();
        let err = fail_with(move |rec| tool_err(rec, &t)).await;
        assert_eq!(err.code, C::UpstreamRateLimited, "{text}");
        assert_eq!(err.retry_after_s, Some(60), "a fixed hint for {text}");
    }
    for text in [
        "Error code 4290",
        "found 14291 results for the id 0429abcd",
        "DependencyFailedException - The web search tool received an invalid request: 1 validation error detected: \
Value at 'query' failed to satisfy constraint: 429 BC",
        "throttling configuration is invalid",
    ] {
        let t = text.to_string();
        let err = fail_with(move |rec| tool_err(rec, &t)).await;
        assert_eq!(err.code, C::UpstreamError, "{text}");
    }
}

#[tokio::test]
async fn a_search_text_that_mentions_throttling_is_still_a_success() {
    let server = gateway(|rec| {
        tool_ok(
            rec,
            &json!({"id": "a1b2c3d4", "results": [{"title": "Error code 429 explained", "url": "https://docs.example.org/429",
                "publishedDate": "unknown", "text": "Error code 429 means Too Many Requests; ThrottlingException"}]})
            .to_string(),
        )
    })
    .await;
    let out = provider(&server).search(&search("q")).await.unwrap();
    assert_eq!(out.results.len(), 1);
}

#[tokio::test]
async fn an_unsupported_protocol_version_names_the_version() {
    let err = fail_with(|rec| {
        let _ = rec;
        Reply::json(
            400,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Unsupported protocol version","data":{"supported":["2025-03-26"]}}}"#,
        )
    })
    .await;
    assert_eq!(err.code, C::UpstreamError);
    assert!(err.message.contains("protocol version"), "{}", err.message);
    assert!(!err.message.contains("gateway.bedrock-agentcore"));
}

#[tokio::test]
async fn a_refusal_names_the_missing_iam_action_and_never_the_gateway() {
    let arn_text = "AccessDeniedException: User: arn:aws:sts::123456789012:assumed-role/research/fn is not authorized to perform: \
bedrock-agentcore:InvokeGateway on resource: arn:aws:bedrock-agentcore:us-east-1:123456789012:gateway/gw-abc12345";
    let from_http =
        fail_with(move |_| Reply::json(403, &json!({"message": arn_text}).to_string())).await;
    let from_rpc_code = fail_with(|rec| {
        Reply::json(
            200,
            &json!({"jsonrpc": "2.0", "id": rec.json()["id"].clone(), "error": {"code": -32002, "message": "denied"}}).to_string(),
        )
    })
    .await;
    let from_rpc_text = fail_with(move |rec| {
        Reply::json(
            200,
            &json!({"jsonrpc": "2.0", "id": rec.json()["id"].clone(), "error": {"code": -32603, "message": arn_text}}).to_string(),
        )
    })
    .await;
    let from_tool_text = fail_with(move |rec| tool_err(rec, arn_text)).await;
    for err in [from_http, from_rpc_code, from_rpc_text, from_tool_text] {
        assert_eq!(err.code, C::UpstreamError);
        assert!(
            err.message.contains("bedrock-agentcore:InvokeGateway"),
            "{}",
            err.message
        );
        for banned in [
            "123456789012",
            "gw-abc12345",
            "gateway.bedrock-agentcore",
            "arn:aws",
        ] {
            assert!(!err.message.contains(banned), "{banned} in {}", err.message);
        }
    }
}

#[tokio::test]
async fn http_429_is_rate_limited_with_a_capped_retry_after() {
    for (header, want) in [
        (Some("7"), Some(7)),
        (Some("99999"), Some(3600)),
        (Some("soon"), None),
        (Some("-5"), None),
        (None, None),
    ] {
        let err = fail_with(move |_| match header {
            Some(h) => Reply::bytes(429, &[("Retry-After", h)], b"{}"),
            None => Reply::json(429, "{}"),
        })
        .await;
        assert_eq!(err.code, C::UpstreamRateLimited, "{header:?}");
        assert_eq!(err.retry_after_s, want, "{header:?}");
    }
}

#[tokio::test]
async fn a_server_error_is_an_upstream_error_with_the_status() {
    for status in [500u16, 502, 503] {
        let err = fail_with(move |_| Reply::json(status, "{\"message\":\"boom\"}")).await;
        assert_eq!(err.code, C::UpstreamError, "{status}");
        assert!(err.message.contains(&status.to_string()), "{}", err.message);
    }
}

#[tokio::test]
async fn a_call_that_hangs_times_out_as_an_upstream_error() {
    let server = FakeServer::start(|rec| match method_of(rec).as_str() {
        "initialize" => init_reply(rec),
        _ => Reply::Delayed(Duration::from_secs(5), Box::new(Reply::json(200, "{}"))),
    })
    .await;
    let p = provider_with(&server, Duration::from_millis(400));
    let err = p.search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert!(err.message.contains("in time"), "{}", err.message);
}

type Handler = Box<dyn Fn(&Recorded) -> Reply + Send + Sync>;

#[tokio::test]
async fn an_answer_that_cannot_be_read_is_an_upstream_error() {
    let big = "x".repeat(700 * 1024);
    let cases: Vec<(&str, Handler)> = vec![
        ("not JSON", Box::new(|_| Reply::json(200, "<html>"))),
        (
            "a malformed text",
            Box::new(|rec| tool_ok(rec, "{\"results\": [")),
        ),
        (
            "no result member",
            Box::new(|rec| {
                Reply::json(
                    200,
                    &json!({"jsonrpc": "2.0", "id": rec.json()["id"].clone()}).to_string(),
                )
            }),
        ),
        (
            "no content",
            Box::new(|rec| rpc(rec, json!({"isError": false, "content": []}))),
        ),
        (
            "another request id",
            Box::new(|_| {
                Reply::json(
                    200,
                    r#"{"jsonrpc":"2.0","id":9999,"result":{"isError":false,"content":[{"type":"text","text":"{\"id\":\"a\",\"results\":[]}"}]}}"#,
                )
            }),
        ),
        (
            "an answer over the cap",
            Box::new(move |rec| tool_ok(rec, &big)),
        ),
    ];
    for (name, reply) in cases {
        let err = fail_with(reply).await;
        assert_eq!(err.code, C::UpstreamError, "{name}");
        assert!(!err.message.contains("gateway.bedrock-agentcore"), "{name}");
    }
}

#[tokio::test]
async fn another_json_rpc_error_is_an_upstream_error_with_its_code_only() {
    let err = fail_with(|rec| {
        Reply::json(
            200,
            &json!({"jsonrpc": "2.0", "id": rec.json()["id"].clone(), "error": {"code": -32603, "message": "Internal error"}}).to_string(),
        )
    })
    .await;
    assert_eq!(err.code, C::UpstreamError);
    assert!(err.message.contains("-32603"), "{}", err.message);
}

// ---- extract ---------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_provider_cannot_extract_and_says_so_without_a_call() {
    let server = ok_gateway(1).await;
    let p = provider(&server);
    assert!(!p.can_extract());
    let err = p
        .extract(&["https://docs.example.org/a".to_string()], None)
        .await
        .unwrap_err();
    assert_eq!(err.code, C::Disabled);
    assert!(err.message.contains("agentcore"), "{}", err.message);
    assert_eq!(server.count(), 0);
}

// ---- the signature ---------------------------------------------------------------------------------------------------------------------

/// The known answer: fixed credentials, time, host, headers and body, computed by an independent SigV4 implementation (Python `hmac`).
#[test]
fn the_signature_equals_the_known_answer_of_a_fixed_request() {
    let headers = sign_headers(
        GATEWAY_URL,
        "us-east-1",
        &[
            ("content-type", "application/json"),
            ("accept", "application/json, text/event-stream"),
            ("mcp-protocol-version", "2025-03-26"),
        ],
        br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        &credentials(),
        UNIX_EPOCH + Duration::from_secs(NOW_S),
    )
    .unwrap();
    let get = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(get("x-amz-date"), Some("20261025T120000Z"));
    assert_eq!(get("x-amz-security-token"), Some(token().as_str()));
    assert_eq!(
        get("authorization"),
        Some(
            "AWS4-HMAC-SHA256 Credential=ASIA-test-vector/20261025/us-east-1/bedrock-agentcore/aws4_request, \
SignedHeaders=accept;content-type;host;mcp-protocol-version;x-amz-date;x-amz-security-token, \
Signature=a530bc60175028eb10e658d181cf60652a97524bfdfc7cf5216fdcc60c68c6df"
        )
    );
}

#[test]
fn a_changed_region_service_header_or_body_changes_the_signature() {
    let sign = |region: &str, extra: &[(&str, &str)], body: &[u8]| {
        sign_headers(
            GATEWAY_URL,
            region,
            extra,
            body,
            &credentials(),
            UNIX_EPOCH + Duration::from_secs(NOW_S),
        )
        .unwrap()
        .into_iter()
        .find(|(n, _)| n == "authorization")
        .unwrap()
        .1
    };
    let base_headers = [("content-type", "application/json")];
    let base = sign("us-east-1", &base_headers, b"{}");
    assert_ne!(base, sign("us-west-2", &base_headers, b"{}"));
    assert_ne!(
        base,
        sign("us-east-1", &[("content-type", "text/plain")], b"{}")
    );
    assert_ne!(base, sign("us-east-1", &base_headers, b"{ }"));
    assert_eq!(base, sign("us-east-1", &base_headers, b"{}"));
}

/// HMAC-SHA256 written out, so the check below does not use the code under test.
fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut k = if key.len() > 64 {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    k.resize(64, 0);
    let inner: Vec<u8> = k
        .iter()
        .map(|b| b ^ 0x36)
        .chain(msg.iter().copied())
        .collect();
    let inner_hash = Sha256::digest(&inner);
    let outer: Vec<u8> = k
        .iter()
        .map(|b| b ^ 0x5c)
        .chain(inner_hash.iter().copied())
        .collect();
    Sha256::digest(&outer).to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Recomputes the signature of a request the fake gateway received, from the bytes received.
fn signature_holds(rec: &Recorded) -> Result<(), String> {
    let auth = rec.header("authorization").ok_or("no authorization")?;
    let rest = auth
        .strip_prefix("AWS4-HMAC-SHA256 Credential=")
        .ok_or("not SigV4")?;
    let (cred, rest) = rest
        .split_once(", SignedHeaders=")
        .ok_or("no signed headers")?;
    let (signed, given) = rest.split_once(", Signature=").ok_or("no signature")?;
    let scope = cred
        .strip_prefix(&format!("{KEY_ID}/"))
        .ok_or("another key id")?;
    if !scope.ends_with("/us-east-1/bedrock-agentcore/aws4_request") {
        return Err(format!("scope {scope}"));
    }
    let day = scope.split('/').next().unwrap_or("");
    let date = rec.header("x-amz-date").ok_or("no date")?;
    let mut canon_headers = String::new();
    for name in signed.split(';') {
        let value = rec
            .header(name)
            .ok_or_else(|| format!("signed header {name} missing"))?;
        canon_headers.push_str(&format!("{name}:{}\n", value.trim()));
    }
    for must in [
        "host",
        "x-amz-date",
        "x-amz-security-token",
        "content-type",
        "accept",
        "mcp-protocol-version",
    ] {
        if !signed.split(';').any(|n| n == must) {
            return Err(format!("{must} is not signed"));
        }
    }
    let path = rec.target.split('?').next().unwrap_or("");
    let canon = format!(
        "POST\n{path}\n\n{canon_headers}\n{signed}\n{}",
        hex(&Sha256::digest(&rec.body))
    );
    let sts = format!(
        "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
        hex(&Sha256::digest(canon.as_bytes()))
    );
    let mut key = hmac(format!("AWS4{}", secret()).as_bytes(), day.as_bytes());
    for part in ["us-east-1", "bedrock-agentcore", "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let want = hex(&hmac(&key, sts.as_bytes()));
    if want == given {
        Ok(())
    } else {
        Err(format!("signature differs: {want} against {given}"))
    }
}

#[tokio::test]
async fn every_request_the_gateway_received_carries_a_signature_that_recomputes() {
    let server = ok_gateway(2).await;
    let p = provider(&server);
    p.search(&search("serde")).await.unwrap();
    p.search(&search("tokio")).await.unwrap();
    let seen = server.requests();
    assert_eq!(seen.len(), 3);
    for r in &seen {
        assert_eq!(signature_holds(r), Ok(()), "{}", method_of(r));
        assert_eq!(r.header("x-amz-date"), Some("20261025T120000Z"));
    }
}

// ---- the gateway address ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_gateway_address_is_one_https_host_of_the_service_with_the_path_mcp() {
    let t = GatewayTarget::parse(GATEWAY_URL).unwrap();
    assert_eq!(t.region, "us-east-1");
    assert_eq!(t.host, GATEWAY_HOST);
    assert_eq!(t.url, GATEWAY_URL);
    let t = GatewayTarget::parse(
        "https://hamh-direct-websearch-ab12cd34ef.gateway.bedrock-agentcore.eu-west-1.amazonaws.com/mcp",
    )
    .unwrap();
    assert_eq!(t.region, "eu-west-1");
    for bad in [
        "",
        "http://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp",
        "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/",
        "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp/",
        "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/other",
        "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp?x=1",
        "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp#f",
        "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com:8443/mcp",
        "https://user@gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp",
        "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com.example.org/mcp",
        "https://gw-abc12345.example.org/mcp",
        "https://example.org/gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp",
        "https://GW-ABC.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp",
        "https://.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp",
        "https://gw.gateway.bedrock-agentcore.nowhere.amazonaws.com/mcp",
        "https://gw abc.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp",
        "https://127.0.0.1/mcp",
    ] {
        assert!(GatewayTarget::parse(bad).is_err(), "accepted {bad:?}");
    }
}

#[test]
fn the_module_prints_nothing() {
    // A result, a query or a credential must not reach a log line: the provider has no logging at all.
    let source = include_str!("../src/agentcore.rs");
    for banned in [
        "println!",
        "eprintln!",
        "dbg!",
        "print!(",
        "tracing::",
        "log::",
    ] {
        assert!(!source.contains(banned), "agentcore.rs contains {banned}");
    }
}
