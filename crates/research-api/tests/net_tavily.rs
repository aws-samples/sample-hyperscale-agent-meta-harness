//! The Tavily client against a scripted fake Tavily on 127.0.0.1. Request shapes, response mapping, the key's
//! one place in a request, the 401 re-read, rate limits, and the rule that the key never reaches an error. No test reaches the network.

#![allow(clippy::unwrap_used)]

#[path = "support/net_fake.rs"]
mod net_fake;
#[path = "support/net_keys.rs"]
mod net_keys;

use harness_protocol::research::{
    ResearchErrorCode as C, ResearchProvider, SearchDepth, SearchRequest, TimeRange,
};
use net_fake::{FakeServer, Recorded, Reply};
use net_keys::{FakeClock, Script, key};
use research_api::http::testing::TestNet;
use research_api::http::{HttpClient, HttpOptions};
use research_api::ports::{Fail, Provider};
use research_api::secret::CachedSecret;
use research_api::tavily::TavilyClient;
use serde_json::json;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn http_for(addr: SocketAddr, total: Duration) -> HttpClient {
    let mut hosts = BTreeMap::new();
    hosts.insert("api.tavily.com".to_string(), addr);
    HttpClient::with_test_net(
        HttpOptions {
            connect_timeout: Duration::from_secs(2),
            total_timeout: total,
            max_response_bytes: 1024 * 1024,
        },
        TestNet { hosts },
    )
    .unwrap()
}

type Keyed = TavilyClient<Script, Arc<FakeClock>>;

fn keyed(server: &FakeServer, script: &Script) -> (Keyed, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::default());
    let secret = CachedSecret::with_clock(script.clone(), clock.clone());
    let client = TavilyClient::keyed(
        http_for(server.addr, Duration::from_secs(3)),
        "https://api.tavily.com".into(),
        secret,
    );
    (client, clock)
}

fn keyless(server: &FakeServer) -> TavilyClient<Script, Arc<FakeClock>> {
    TavilyClient::keyless(
        http_for(server.addr, Duration::from_secs(3)),
        "https://api.tavily.com/".into(),
    )
}

fn search(q: &str) -> SearchRequest {
    SearchRequest {
        query: q.into(),
        ..Default::default()
    }
}

/// The shape Tavily answered on 2026-10-16 (recorded in the project's evidence), shortened.
fn search_answer() -> String {
    json!({
        "query": "is-plain-obj", "follow_up_questions": null, "answer": null, "images": [],
        "results": [
            {"url": "https://www.npmjs.com/package/is-plain-obj", "title": "is-plain-obj - npm", "content": "Check if a value is a plain object",
             "score": 0.8861155, "raw_content": null, "id": "a-00"},
            {"url": "https://github.com/sindresorhus/is-plain-obj", "title": "GitHub - is-plain-obj", "content": "readme text",
             "score": 0.5, "raw_content": "THE WHOLE PAGE MUST NOT BE USED", "published_date": "2024-05-01", "id": "a-01"},
            {"title": "no url, skipped", "content": "x", "score": 0.1},
            {"url": "https://example.org/third", "title": "third", "content": "third text", "score": 0.2}
        ],
        "response_time": 1.13, "request_id": "req-up-1"
    })
    .to_string()
}

fn extract_answer() -> String {
    json!({
        "results": [
            {"url": "https://www.npmjs.com/package/is-plain-obj", "title": "npm", "raw_content": "# is-plain-obj\n\nCheck if a value is a plain object.", "images": []},
            {"url": "https://example.org/empty", "title": "empty", "raw_content": null, "images": []}
        ],
        "failed_results": [{"url": "https://example.org/gone", "error": "Failed to fetch url"}],
        "response_time": 0.97, "request_id": "req-up-2"
    })
    .to_string()
}

fn json_ok(body: String) -> Reply {
    Reply::json(200, &body)
}

// ---- search ----------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_keyed_search_sends_the_documented_request_and_maps_the_answer() {
    let k = key("a");
    let server = FakeServer::start(|_| json_ok(search_answer())).await;
    let (client, _) = keyed(&server, &Script::keys(&[&k]));
    assert_eq!(client.id(), ResearchProvider::Tavily);
    let out = client.search(&search("is-plain-obj npm")).await.unwrap();

    let seen = &server.requests()[0];
    assert_eq!(
        (seen.method.as_str(), seen.target.as_str()),
        ("POST", "/search")
    );
    assert_eq!(
        seen.header("authorization"),
        Some(format!("Bearer {k}").as_str())
    );
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert!(seen.header("x-tavily-access-mode").is_none());
    assert_eq!(
        seen.json(),
        json!({"query": "is-plain-obj npm", "max_results": 5, "search_depth": "basic",
               "include_answer": false, "include_raw_content": false})
    );

    assert_eq!(out.results.len(), 3, "a result without a URL is skipped");
    assert_eq!(
        out.results[0].url,
        "https://www.npmjs.com/package/is-plain-obj"
    );
    assert_eq!(out.results[0].title, "is-plain-obj - npm");
    assert_eq!(out.results[0].snippet, "Check if a value is a plain object");
    assert_eq!(out.results[0].score, Some(0.8861155));
    assert_eq!(out.results[1].published.as_deref(), Some("2024-05-01"));
    assert!(
        out.results
            .iter()
            .all(|r| !r.snippet.contains("WHOLE PAGE")),
        "raw_content is never used"
    );
    assert_eq!(
        out.credits,
        Some(1),
        "basic depth is one credit when Tavily reports none"
    );
    assert_eq!(out.upstream_request_id.as_deref(), Some("req-up-1"));
}

#[tokio::test]
async fn optional_members_are_sent_only_when_asked_and_the_result_count_is_respected() {
    let server = FakeServer::start(|_| json_ok(search_answer())).await;
    let (client, _) = keyed(&server, &Script::keys(&[&key("a")]));
    let req = SearchRequest {
        query: "q".into(),
        max_results: Some(2),
        depth: Some(SearchDepth::Advanced),
        include_domains: vec!["npmjs.com".into(), "github.com".into()],
        time_range: Some(TimeRange::Year),
        idempotency_key: Some("k-1".into()),
    };
    let out = client.search(&req).await.unwrap();
    assert_eq!(
        server.requests()[0].json(),
        json!({"query": "q", "max_results": 2, "search_depth": "advanced", "include_domains": ["npmjs.com", "github.com"],
               "time_range": "year", "include_answer": false, "include_raw_content": false})
    );
    assert_eq!(out.results.len(), 2);
    assert_eq!(
        out.credits,
        Some(2),
        "advanced depth is two credits when Tavily reports none"
    );
    // The idempotency key is the route's business and never goes to Tavily.
    assert!(!server.requests()[0].json().to_string().contains("k-1"));
}

#[tokio::test]
async fn credits_come_from_the_usage_object_when_tavily_reports_it() {
    let body = json!({"results": [], "usage": {"credits": 3}, "request_id": "r"}).to_string();
    let server = FakeServer::start(move |_| json_ok(body.clone())).await;
    let (client, _) = keyed(&server, &Script::keys(&[&key("a")]));
    let out = client.search(&search("q")).await.unwrap();
    assert_eq!((out.results.len(), out.credits), (0, Some(3)));
}

#[tokio::test]
async fn keyless_mode_sends_its_header_and_no_authorization() {
    let server = FakeServer::start(|_| json_ok(search_answer())).await;
    let client = keyless(&server);
    assert_eq!(client.id(), ResearchProvider::TavilyKeyless);
    client.search(&search("q")).await.unwrap();
    let seen = &server.requests()[0];
    assert_eq!(seen.header("x-tavily-access-mode"), Some("keyless"));
    assert!(seen.header("authorization").is_none());
    assert_eq!(
        seen.target, "/search",
        "a trailing slash on the base URL is ignored"
    );
}

#[tokio::test]
async fn a_keyed_client_with_no_key_stored_fails_and_never_falls_back_to_keyless() {
    let server = FakeServer::start(|_| json_ok(search_answer())).await;
    let (client, _) = keyed(&server, &Script::with(vec![Ok(None)]));
    let err = client.search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::NoKey);
    assert_eq!(server.count(), 0, "nothing was sent to Tavily");
    let err = client
        .extract(&["https://example.org/a".into()], None)
        .await
        .unwrap_err();
    assert_eq!(err.code, C::NoKey);
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn a_key_store_failure_is_an_upstream_error_and_sends_nothing() {
    let server = FakeServer::start(|_| json_ok(search_answer())).await;
    let (client, _) = keyed(
        &server,
        &Script::with(vec![Err(Fail::upstream("the key store did not answer"))]),
    );
    let err = client.search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert_eq!(server.count(), 0);
}

// ---- extract ----------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn extract_sends_basic_text_extraction_and_maps_pages_and_failures() {
    let k = key("a");
    let server = FakeServer::start(|_| json_ok(extract_answer())).await;
    let (client, _) = keyed(&server, &Script::keys(&[&k]));
    let urls: Vec<String> = [
        "https://www.npmjs.com/package/is-plain-obj",
        "https://example.org/empty",
        "https://example.org/gone",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = client
        .extract(&urls, Some("what counts as plain"))
        .await
        .unwrap();
    let seen = &server.requests()[0];
    assert_eq!(
        (seen.method.as_str(), seen.target.as_str()),
        ("POST", "/extract")
    );
    assert_eq!(
        seen.header("authorization"),
        Some(format!("Bearer {k}").as_str())
    );
    assert_eq!(
        seen.json(),
        json!({"urls": urls, "extract_depth": "basic", "format": "text", "timeout": 15, "query": "what counts as plain"})
    );
    assert_eq!(out.pages.len(), 1);
    assert_eq!(
        out.pages[0].url,
        "https://www.npmjs.com/package/is-plain-obj"
    );
    assert!(out.pages[0].text.contains("plain object"));
    let failed: Vec<(&str, &str)> = out
        .failed
        .iter()
        .map(|f| (f.url.as_str(), f.message.as_str()))
        .collect();
    assert_eq!(
        failed,
        [
            ("https://example.org/gone", "Failed to fetch url"),
            ("https://example.org/empty", "the page has no text")
        ]
    );
    assert_eq!(out.credits, Some(1), "one credit per five URLs, rounded up");
    assert_eq!(out.upstream_request_id.as_deref(), Some("req-up-2"));

    // Without a query the member is absent.
    client.extract(&urls[..1], None).await.unwrap();
    assert!(server.requests()[1].json().get("query").is_none());
}

#[tokio::test]
async fn extract_in_keyless_mode_uses_the_keyless_header() {
    let server = FakeServer::start(|_| json_ok(extract_answer())).await;
    keyless(&server)
        .extract(&["https://example.org/a".into()], None)
        .await
        .unwrap();
    let seen = &server.requests()[0];
    assert_eq!(seen.header("x-tavily-access-mode"), Some("keyless"));
    assert!(seen.header("authorization").is_none());
}

// ---- the key in a request ---------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_key_is_in_the_authorization_header_and_nowhere_else() {
    let k = key("only-header");
    let server = FakeServer::start(|_| json_ok(search_answer())).await;
    let (client, _) = keyed(&server, &Script::keys(&[&k]));
    client.search(&search("q")).await.unwrap();
    client
        .extract(&["https://example.org/a".into()], Some("q"))
        .await
        .unwrap();
    for r in server.requests() {
        assert!(!r.target.contains(&k), "not in the URL");
        assert!(
            !String::from_utf8_lossy(&r.body).contains(&k),
            "not in the body"
        );
        for (name, value) in &r.headers {
            if name != "authorization" {
                assert!(!value.contains(&k), "not in header {name}");
            }
        }
    }
}

// ---- 401 and the re-read ----------------------------------------------------------------------------------------------------------------

fn accepts_only(
    good: String,
    hits: Arc<AtomicUsize>,
) -> impl Fn(&Recorded) -> Reply + Send + Sync + 'static {
    move |r| {
        hits.fetch_add(1, Ordering::SeqCst);
        if r.header("authorization") == Some(format!("Bearer {good}").as_str()) {
            json_ok(search_answer())
        } else {
            Reply::json(
                401,
                r#"{"detail":{"error":"Unauthorized: missing or invalid API key."}}"#,
            )
        }
    }
}

#[tokio::test]
async fn a_refused_key_is_re_read_once_and_the_call_retried_with_the_new_one() {
    let (old, new) = (key("old"), key("new"));
    let hits = Arc::new(AtomicUsize::new(0));
    let server = FakeServer::start(accepts_only(new.clone(), hits.clone())).await;
    let script = Script::keys(&[&old, &new]);
    let (client, _) = keyed(&server, &script);
    let out = client.search(&search("q")).await.unwrap();
    assert_eq!(out.results.len(), 3);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let auth: Vec<String> = server
        .requests()
        .iter()
        .map(|r| r.header("authorization").unwrap().to_string())
        .collect();
    assert_eq!(auth, [format!("Bearer {old}"), format!("Bearer {new}")]);
    assert_eq!(script.reads(), 2);
    // The new key is the cached one now.
    client.search(&search("again")).await.unwrap();
    assert_eq!(
        server.requests()[2].header("authorization"),
        Some(format!("Bearer {new}").as_str())
    );
    assert_eq!(script.reads(), 2);
}

#[tokio::test]
async fn a_key_that_stays_refused_is_no_key_and_is_re_read_at_most_every_thirty_seconds() {
    let bad = key("bad");
    let hits = Arc::new(AtomicUsize::new(0));
    let server = FakeServer::start(accepts_only(key("never"), hits.clone())).await;
    let script = Script::keys(&[&bad]);
    let (client, clock) = keyed(&server, &script);
    // The re-read finds the same key: no retry, the key is refused.
    let err = client.search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::NoKey);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(script.reads(), 2, "one normal read and one re-read");
    // Inside the window there is no second re-read.
    let err = client.search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::NoKey);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(script.reads(), 2);
    // After the window the secret is read again.
    clock.advance_s(31);
    let err = client.search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::NoKey);
    assert_eq!(
        script.reads(),
        3,
        "after the window one more re-read is allowed"
    );
}

#[tokio::test]
async fn a_new_key_that_is_also_refused_is_no_key_after_one_retry() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = FakeServer::start(accepts_only(key("never"), hits.clone())).await;
    let (client, _) = keyed(&server, &Script::keys(&[&key("a"), &key("b")]));
    let err = client.search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::NoKey);
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one retry and no more");
}

#[tokio::test]
async fn a_forbidden_answer_is_treated_like_an_unauthorized_one() {
    let server =
        FakeServer::start(|_| Reply::json(403, r#"{"detail":{"error":"Forbidden"}}"#)).await;
    let (client, _) = keyed(&server, &Script::keys(&[&key("a")]));
    assert_eq!(
        client.search(&search("q")).await.unwrap_err().code,
        C::NoKey
    );
}

#[tokio::test]
async fn keyless_mode_refused_is_an_upstream_error_not_a_missing_key() {
    let server = FakeServer::start(|_| Reply::json(401, "{}")).await;
    let err = keyless(&server).search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert_eq!(server.count(), 1);
}

// ---- other statuses ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_429_is_rate_limited_and_honours_retry_after_up_to_an_hour() {
    for (header, want) in [
        (Some("7"), Some(7)),
        (Some("99999"), Some(3600)),
        (Some("soon"), None),
        (None, None),
    ] {
        let server = FakeServer::start(move |_| {
            let h: Vec<(&str, &str)> = header.map(|v| vec![("Retry-After", v)]).unwrap_or_default();
            Reply::bytes(429, &h, br#"{"detail":{"error":"Too many requests"}}"#)
        })
        .await;
        let (client, _) = keyed(&server, &Script::keys(&[&key("a")]));
        let err = client.search(&search("q")).await.unwrap_err();
        assert_eq!(err.code, C::UpstreamRateLimited, "{header:?}");
        assert_eq!(err.retry_after_s, want, "{header:?}");
    }
}

#[tokio::test]
async fn plan_limits_server_errors_and_odd_statuses_are_upstream_errors_with_the_status() {
    for status in [400u16, 404, 432, 433, 500, 502, 503] {
        let server = FakeServer::start(move |_| {
            Reply::json(status, r#"{"detail":{"error":"plan limit exceeded"}}"#)
        })
        .await;
        let (client, _) = keyed(&server, &Script::keys(&[&key("a")]));
        let err = client.search(&search("q")).await.unwrap_err();
        assert_eq!(err.code, C::UpstreamError, "{status}");
        assert!(err.message.contains(&status.to_string()), "{}", err.message);
        assert!(
            err.message.contains("plan limit exceeded"),
            "an excerpt of the body is kept: {}",
            err.message
        );
    }
}

#[tokio::test]
async fn an_unreadable_answer_a_timeout_and_a_dead_server_are_upstream_errors() {
    let server = FakeServer::start(|_| Reply::json(200, "<html>not json</html>")).await;
    let (client, _) = keyed(&server, &Script::keys(&[&key("a")]));
    let err = client.search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert!(err.message.contains("could not be read"), "{}", err.message);

    let slow = FakeServer::start(|_| {
        Reply::Delayed(Duration::from_secs(3), Box::new(json_ok(search_answer())))
    })
    .await;
    let clock = Arc::new(FakeClock::default());
    let client = TavilyClient::keyed(
        http_for(slow.addr, Duration::from_millis(200)),
        "https://api.tavily.com".into(),
        CachedSecret::with_clock(Script::keys(&[&key("a")]), clock),
    );
    let err = client.search(&search("q")).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert!(err.message.contains("in time"), "{}", err.message);

    let dead = net_fake::closed_addr().await;
    let clock = Arc::new(FakeClock::default());
    let client = TavilyClient::keyed(
        http_for(dead, Duration::from_secs(2)),
        "https://api.tavily.com".into(),
        CachedSecret::with_clock(Script::keys(&[&key("a")]), clock),
    );
    assert_eq!(
        client.search(&search("q")).await.unwrap_err().code,
        C::UpstreamError
    );
}

// ---- the key never reaches an error ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_upstream_body_that_contains_the_key_never_reaches_a_message_or_a_debug_dump() {
    let k = key("echoed");
    let echo = k.clone();
    let server = FakeServer::start(move |r| {
        // A server whose error text repeats what it was sent.
        let body = format!(
            r#"{{"detail":{{"error":"bad request: {} header {}"}}}}"#,
            echo,
            r.header("authorization").unwrap_or("")
        );
        Reply::json(500, &body)
    })
    .await;
    let (client, _) = keyed(&server, &Script::keys(&[&k]));
    let err = client.search(&search("q")).await.unwrap_err();
    for shown in [err.message.clone(), format!("{err:?}"), format!("{err}")] {
        assert!(!shown.contains(&k), "the key is in: {shown}");
    }
    assert!(
        err.message.contains("repeats the key"),
        "a body that repeats the key is replaced by a fixed sentence: {}",
        err.message
    );
    assert!(
        !err.message.contains('\u{1b}'),
        "control characters are removed"
    );
    assert!(err.message.chars().count() < 400);
    let err = client
        .extract(&["https://example.org/a".into()], None)
        .await
        .unwrap_err();
    assert!(!format!("{err:?}").contains(&k));
    assert!(!format!("{client:?}").contains(&k));
}

#[tokio::test]
async fn an_error_excerpt_is_at_most_two_hundred_characters() {
    let server = FakeServer::start(|_| Reply::json(500, &"y".repeat(5000))).await;
    let (client, _) = keyed(&server, &Script::keys(&[&key("a")]));
    let err = client.search(&search("q")).await.unwrap_err();
    let excerpt = err.message.strip_prefix("Tavily answered 500: ").unwrap();
    assert_eq!(excerpt.chars().count(), 200, "{}", err.message);
}

#[test]
fn the_module_prints_nothing() {
    // The key must not reach a log line: the Tavily client has no logging at all. A new print would need a test of its own.
    let source = include_str!("../src/tavily.rs");
    for banned in [
        "println!",
        "eprintln!",
        "dbg!",
        "print!(",
        "tracing::",
        "log::",
    ] {
        assert!(!source.contains(banned), "tavily.rs contains {banned}");
    }
}

// ---- an answer that repeats the key ------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Echo {
    SearchTitle,
    SearchRequestId,
    ExtractFailure,
    ExtractPage,
    ErrorBody,
}

#[tokio::test]
async fn an_answer_that_repeats_the_key_is_discarded_whatever_the_member_or_the_status() {
    let k = key("echoes");
    for case in [
        Echo::SearchTitle,
        Echo::SearchRequestId,
        Echo::ExtractFailure,
        Echo::ExtractPage,
        Echo::ErrorBody,
    ] {
        let server = FakeServer::start(move |r| {
            let auth = r.header("authorization").unwrap_or("").to_string();
            match case {
                Echo::SearchTitle => json_ok(
                    json!({"results": [{"url": "https://example.org/a", "title": auth, "content": "c", "score": 0.5}], "request_id": "r"})
                        .to_string(),
                ),
                Echo::SearchRequestId => json_ok(json!({"results": [], "request_id": auth}).to_string()),
                Echo::ExtractFailure => json_ok(
                    json!({"results": [], "failed_results": [{"url": "https://example.org/a", "error": auth}], "request_id": "r"})
                        .to_string(),
                ),
                Echo::ExtractPage => json_ok(
                    json!({"results": [{"url": "https://example.org/a", "raw_content": format!("page text {auth}")}], "failed_results": [], "request_id": "r"})
                        .to_string(),
                ),
                Echo::ErrorBody => Reply::json(500, &json!({"detail": {"error": auth}}).to_string()),
            }
        })
        .await;
        let (client, _) = keyed(&server, &Script::keys(&[&k]));
        let err = match case {
            Echo::SearchTitle | Echo::SearchRequestId | Echo::ErrorBody => {
                client.search(&search("q")).await.unwrap_err()
            }
            Echo::ExtractFailure | Echo::ExtractPage => client
                .extract(&["https://example.org/a".into()], None)
                .await
                .unwrap_err(),
        };
        assert_eq!(err.code, C::UpstreamError, "{case:?}");
        assert!(
            err.message.contains("repeats the key"),
            "{case:?}: {}",
            err.message
        );
        for shown in [err.message.clone(), format!("{err:?}"), format!("{err}")] {
            assert!(!shown.contains(&k), "{case:?}: the key is in {shown}");
        }
    }
}

#[tokio::test]
async fn answers_that_do_not_repeat_the_key_pass_and_the_keyless_client_has_no_key_to_repeat() {
    let server = FakeServer::start(|_| {
        json_ok(json!({"results": [{"url": "https://example.org/a", "title": "Bearer tokens explained", "content": "c"}], "request_id": "r"}).to_string())
    })
    .await;
    let (client, _) = keyed(&server, &Script::keys(&[&key("quiet")]));
    assert_eq!(client.search(&search("q")).await.unwrap().results.len(), 1);
    assert_eq!(
        keyless(&server)
            .search(&search("q"))
            .await
            .unwrap()
            .results
            .len(),
        1
    );
}

#[tokio::test]
async fn a_refusal_whose_body_repeats_the_key_still_leads_to_the_re_read() {
    // The body of a 401 or 403 is never read, so a repeated key in it does not stop the rotation path.
    let (old, new) = (key("old"), key("new"));
    let (good, echoed) = (new.clone(), old.clone());
    let server = FakeServer::start(move |r| {
        if r.header("authorization") == Some(format!("Bearer {good}").as_str()) {
            json_ok(search_answer())
        } else {
            Reply::json(
                401,
                &json!({"detail": {"error": format!("invalid key {echoed}")}}).to_string(),
            )
        }
    })
    .await;
    let (client, _) = keyed(&server, &Script::keys(&[&old, &new]));
    assert_eq!(client.search(&search("q")).await.unwrap().results.len(), 3);
    assert_eq!(server.count(), 2);
}
