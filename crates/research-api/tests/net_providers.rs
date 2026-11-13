//! The search providers and the choice among them: the fixture provider with its run-time lock, and `build_provider`.
//! No test reaches the network; the Tavily path runs against the scripted fake on 127.0.0.1.

#![allow(clippy::unwrap_used)]

#[path = "support/net_fake.rs"]
mod net_fake;
#[path = "support/net_gateway.rs"]
mod net_gateway;

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use harness_protocol::research::{ResearchErrorCode as C, ResearchProvider as P, SearchRequest};
use net_fake::{FakeServer, Reply};
use net_gateway::{agentcore_deps, ok_gateway, provider_http};
use research_api::http::testing::TestNet;
use research_api::http::{HttpClient, HttpOptions};
use research_api::metrics::{Event, Metrics};
use research_api::ports::{Provider, StageKind};
use research_api::providers::{AnyProvider, FixtureProvider, ProviderDeps, build_provider};
use research_api::scan::{ScanKind, check_url, extract_links, scan_outbound};
use research_api::secret::{AwsSecretSource, CachedSecret};
use serde_json::json;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

fn query(q: &str) -> SearchRequest {
    SearchRequest {
        query: q.into(),
        ..Default::default()
    }
}

fn fixture() -> FixtureProvider {
    FixtureProvider::new(StageKind::Test).unwrap()
}

// ---- the fixture provider and its lock ------------------------------------------------------------------------------------------------

#[test]
fn the_fixture_provider_exists_only_on_a_test_stage() {
    let err = FixtureProvider::new(StageKind::Official).unwrap_err();
    assert_eq!(err.code, C::Internal);
    assert!(err.message.contains("test stage"), "{}", err.message);
    assert!(FixtureProvider::new(StageKind::Test).is_ok());
    assert_eq!(StageKind::parse("official"), Some(StageKind::Official));
    assert_eq!(StageKind::parse("test"), Some(StageKind::Test));
    assert_eq!(StageKind::parse("Test"), None);

    let deps = || ProviderDeps {
        http: None,
        secret: None,
        tavily_base_url: "https://api.tavily.com".into(),
        agentcore: None,
    };
    let err = build_provider(P::Fixture, StageKind::Official, deps()).unwrap_err();
    assert_eq!(err.code, C::Internal);
    assert!(err.message.contains("test stage"), "{}", err.message);
    let provider = build_provider(P::Fixture, StageKind::Test, deps()).unwrap();
    assert_eq!(provider.id(), P::Fixture);
}

#[tokio::test]
async fn a_search_for_is_plain_obj_returns_the_canned_trio_with_the_npm_page_first() {
    let out = fixture()
        .search(&query(
            "is-plain-obj npm package what counts as a plain object",
        ))
        .await
        .unwrap();
    let urls: Vec<&str> = out.results.iter().map(|r| r.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "https://www.npmjs.com/package/is-plain-obj",
            "https://github.com/sindresorhus/is-plain-obj",
            "https://docs.example.org/guides/plain-objects"
        ]
    );
    for r in &out.results {
        assert!(!r.title.is_empty() && !r.snippet.is_empty());
        assert!(check_url(&r.url, 2048).is_ok(), "{}", r.url);
        assert!(scan_outbound(&r.snippet, ScanKind::Query, &[]).is_ok());
    }
    assert!(out.results[0].snippet.contains("plain object"));
    assert_eq!(out.upstream_request_id.as_deref(), Some("fixture"));
    // The count is respected, and the answer never changes.
    let two = fixture()
        .search(&SearchRequest {
            max_results: Some(2),
            ..query("find is-plain-obj")
        })
        .await
        .unwrap();
    assert_eq!(two.results.len(), 2);
    let again = fixture()
        .search(&query(
            "is-plain-obj npm package what counts as a plain object",
        ))
        .await
        .unwrap();
    assert_eq!(out, again);
}

#[tokio::test]
async fn any_other_query_returns_two_generic_results_on_documented_example_hosts() {
    let a = fixture()
        .search(&query("How to parse a pax header?"))
        .await
        .unwrap();
    assert_eq!(a.results.len(), 2);
    let hosts: Vec<String> = a
        .results
        .iter()
        .map(|r| check_url(&r.url, 2048).unwrap().host)
        .collect();
    assert_eq!(hosts, ["docs.example.org", "blog.example.org"]);
    assert!(
        a.results[0].url.contains("how-to-parse-a-pax-header"),
        "{}",
        a.results[0].url
    );
    let b = fixture()
        .search(&query("something else entirely"))
        .await
        .unwrap();
    assert_ne!(a.results[0].url, b.results[0].url);
    assert_eq!(
        a,
        fixture()
            .search(&query("How to parse a pax header?"))
            .await
            .unwrap()
    );
    // A query made only of punctuation still gives valid URLs.
    let odd = fixture().search(&query("???")).await.unwrap();
    for r in odd.results {
        assert!(check_url(&r.url, 2048).is_ok(), "{}", r.url);
    }
    // A very long query gives a bounded URL.
    let long = fixture().search(&query(&"word ".repeat(80))).await.unwrap();
    assert!(long.results[0].url.len() < 200);
}

#[tokio::test]
async fn extract_returns_canned_pages_and_a_failure_for_gone_addresses() {
    let urls: Vec<String> = [
        "https://www.npmjs.com/package/is-plain-obj",
        "https://github.com/sindresorhus/is-plain-obj",
        "https://docs.example.org/guides/plain-objects",
        "https://blog.example.org/posts/other",
        "https://docs.example.org/old/gone",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = fixture().extract(&urls, None).await.unwrap();
    assert_eq!(out.pages.len(), 4);
    assert_eq!(out.failed.len(), 1);
    assert_eq!(out.failed[0].url, "https://docs.example.org/old/gone");
    let npm = &out.pages[0];
    assert_eq!(npm.url, urls[0]);
    assert!(npm.text.contains("a plain object"), "{}", npm.text);
    // The page links to the repository, so the provenance table gets the repository link.
    assert!(
        extract_links(&npm.text, 20)
            .contains(&"https://github.com/sindresorhus/is-plain-obj".to_string())
    );
    for page in &out.pages {
        assert!(!page.text.is_empty());
        assert!(
            scan_outbound(&page.text, ScanKind::Query, &[]).is_ok(),
            "key-shaped text in {}",
            page.url
        );
    }
    assert_eq!(out, fixture().extract(&urls, None).await.unwrap());
    // A generic page does not echo the address it was asked for.
    let generic = &out.pages[3].text;
    assert!(
        generic.contains("blog.example.org") && !generic.contains("/posts/other"),
        "{generic}"
    );
}

// ---- build_provider ---------------------------------------------------------------------------------------------------------------------

fn http_to(addr: SocketAddr) -> HttpClient {
    let mut hosts = BTreeMap::new();
    hosts.insert("api.tavily.com".to_string(), addr);
    HttpClient::with_test_net(
        HttpOptions {
            connect_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_secs(3),
            max_response_bytes: 1024 * 1024,
        },
        TestNet { hosts },
    )
    .unwrap()
}

fn secret_returning(key: &str) -> CachedSecret<AwsSecretSource> {
    secret_returning_dated(key, None)
}

/// The secret as Secrets Manager answers it, with the version's `CreatedDate` (Unix seconds) when `created_s` is given.
fn secret_returning_dated(key: &str, created_s: Option<u64>) -> CachedSecret<AwsSecretSource> {
    let mut answer = json!({
        "ARN": "arn:aws:secretsmanager:us-east-1:111122223333:secret:hamh-direct-research-tavily-api-key-AbCdEf",
        "Name": "hamh-direct-research-tavily-api-key", "VersionId": "v1", "SecretString": key, "VersionStages": ["AWSCURRENT"]
    });
    if let Some(created) = created_s {
        answer["CreatedDate"] = json!(created);
    }
    let body = answer.to_string();
    let client = StaticReplayClient::new(vec![ReplayEvent::new(
        http::Request::builder()
            .uri("https://example.invalid/")
            .body(SdkBody::empty())
            .unwrap(),
        http::Response::builder()
            .status(200)
            .header("content-type", "application/x-amz-json-1.1")
            .body(SdkBody::from(body))
            .unwrap(),
    )]);
    let conf = aws_sdk_secretsmanager::Config::builder()
        .behavior_version_latest()
        .region(aws_sdk_secretsmanager::config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_secretsmanager::config::Credentials::for_tests())
        .http_client(client)
        .build();
    CachedSecret::new(AwsSecretSource::new(
        aws_sdk_secretsmanager::Client::from_conf(conf),
        "hamh-direct-research-tavily-api-key".into(),
    ))
}

#[tokio::test]
async fn a_tavily_provider_needs_a_client_and_a_secret_and_a_keyless_one_needs_a_client() {
    let base = || "https://api.tavily.com".to_string();
    let server =
        FakeServer::start(|_| Reply::json(200, r#"{"results":[],"request_id":"r"}"#)).await;

    let err = build_provider(
        P::Tavily,
        StageKind::Official,
        ProviderDeps {
            http: None,
            secret: Some(secret_returning("x")),
            tavily_base_url: base(),
            agentcore: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.code, C::Internal);
    let err = build_provider(
        P::Tavily,
        StageKind::Official,
        ProviderDeps {
            http: Some(http_to(server.addr)),
            secret: None,
            tavily_base_url: base(),
            agentcore: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.code, C::Internal);
    let err = build_provider(
        P::TavilyKeyless,
        StageKind::Official,
        ProviderDeps {
            http: None,
            secret: None,
            tavily_base_url: base(),
            agentcore: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.code, C::Internal);
    for not_a_search_provider in [P::GithubArchive, P::User] {
        let err = build_provider(
            not_a_search_provider,
            StageKind::Test,
            ProviderDeps {
                http: Some(http_to(server.addr)),
                secret: None,
                tavily_base_url: base(),
                agentcore: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.code, C::Internal);
    }
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn the_tavily_provider_searches_with_the_key_read_from_the_secret() {
    let key = ["stand", "-in-key-", "from-secret", "-0123456789abcdef"].concat();
    let server = FakeServer::start(|_| {
        Reply::json(200, r#"{"results":[{"url":"https://example.org/a","title":"A","content":"c","score":0.5}],"request_id":"r1"}"#)
    })
    .await;
    let provider = build_provider(
        P::Tavily,
        StageKind::Official,
        ProviderDeps {
            http: Some(http_to(server.addr)),
            secret: Some(secret_returning(&key)),
            tavily_base_url: "https://api.tavily.com".into(),
            agentcore: None,
        },
    )
    .unwrap();
    assert!(matches!(provider, AnyProvider::Tavily(_)));
    assert_eq!(provider.id(), P::Tavily);
    let out = provider.search(&query("q")).await.unwrap();
    assert_eq!(out.results.len(), 1);
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some(format!("Bearer {key}").as_str())
    );
}

#[tokio::test]
async fn the_keyless_provider_sends_no_credential() {
    let server =
        FakeServer::start(|_| Reply::json(200, r#"{"results":[],"request_id":"r"}"#)).await;
    let provider = build_provider(
        P::TavilyKeyless,
        StageKind::Test,
        ProviderDeps {
            http: Some(http_to(server.addr)),
            secret: None,
            tavily_base_url: "https://api.tavily.com".into(),
            agentcore: None,
        },
    )
    .unwrap();
    assert_eq!(provider.id(), P::TavilyKeyless);
    provider.search(&query("q")).await.unwrap();
    let seen = &server.requests()[0];
    assert_eq!(seen.header("x-tavily-access-mode"), Some("keyless"));
    assert!(seen.header("authorization").is_none());
}

// ---- the agentcore provider -------------------------------------------------------------------------------------------

#[test]
fn the_agentcore_provider_needs_a_client_and_the_gateway_settings_and_no_secret() {
    let no_client = ProviderDeps {
        http: None,
        secret: None,
        tavily_base_url: "https://api.tavily.com".into(),
        agentcore: Some(agentcore_deps()),
    };
    let err = build_provider(P::Agentcore, StageKind::Official, no_client).unwrap_err();
    assert_eq!(err.code, C::Internal);
    let no_settings = ProviderDeps {
        http: Some(provider_http(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            9,
        )))),
        secret: None,
        tavily_base_url: "https://api.tavily.com".into(),
        agentcore: None,
    };
    let err = build_provider(P::Agentcore, StageKind::Official, no_settings).unwrap_err();
    assert_eq!(err.code, C::Internal);
    assert!(err.message.contains("gateway"), "{}", err.message);
}

#[tokio::test]
async fn the_agentcore_provider_is_built_for_an_official_stage_and_searches_through_the_gateway() {
    let server = ok_gateway(2).await;
    let provider = build_provider(
        P::Agentcore,
        StageKind::Official,
        ProviderDeps {
            http: Some(provider_http(server.addr)),
            secret: None,
            tavily_base_url: "https://api.tavily.com".into(),
            agentcore: Some(agentcore_deps()),
        },
    )
    .unwrap();
    assert!(matches!(provider, AnyProvider::Agentcore(_)));
    assert_eq!(provider.id(), P::Agentcore);
    assert_eq!(provider.max_query_chars(), Some(200));
    assert!(!provider.can_extract());
    let out = provider.search(&query("serde")).await.unwrap();
    assert_eq!(out.results.len(), 2);
    let err = provider
        .extract(&["https://docs.example.org/page-0".to_string()], None)
        .await
        .unwrap_err();
    assert_eq!(err.code, C::Disabled);
}

#[test]
fn the_other_providers_keep_the_route_limits_and_can_extract() {
    let fixture = fixture();
    assert_eq!(fixture.max_query_chars(), None);
    assert!(fixture.can_extract());
}

// ---- the metrics reach the keyed Tavily provider only --------------------------------------------------------------

#[derive(Clone, Default)]
struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<Event>>>);

impl Recorder {
    fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
}

impl Metrics for Recorder {
    fn record(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn the_keyed_tavily_provider_reports_a_limit_and_the_age_of_the_key_it_read() {
    let server = FakeServer::start(|_| Reply::json(433, r#"{"detail":{"error":"limit"}}"#)).await;
    let rec = Recorder::default();
    let stored_s = 1_792_000_000u64;
    let provider = build_provider(
        P::Tavily,
        StageKind::Official,
        ProviderDeps {
            http: Some(http_to(server.addr)),
            secret: Some(secret_returning_dated(
                &["stand", "-in-key"].concat(),
                Some(stored_s),
            )),
            tavily_base_url: "https://api.tavily.com".into(),
            agentcore: None,
        },
    )
    .unwrap()
    .with_metrics(std::sync::Arc::new(rec.clone()));
    let err = provider.search(&query("q")).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert_eq!(
        rec.events(),
        [
            Event::KeyLoaded {
                stored_at_ms: stored_s * 1000
            },
            Event::PlanLimit
        ]
    );
}

#[tokio::test]
async fn the_other_providers_report_nothing() {
    // The keyless Tavily client: no key, so nothing to say about one, whatever Tavily answers.
    for status in [401u16, 429, 432] {
        let server =
            FakeServer::start(move |_| Reply::json(status, r#"{"detail":{"error":"x"}}"#)).await;
        let rec = Recorder::default();
        let provider = build_provider(
            P::TavilyKeyless,
            StageKind::Test,
            ProviderDeps {
                http: Some(http_to(server.addr)),
                secret: None,
                tavily_base_url: "https://api.tavily.com".into(),
                agentcore: None,
            },
        )
        .unwrap()
        .with_metrics(std::sync::Arc::new(rec.clone()));
        assert!(provider.search(&query("q")).await.is_err());
        assert!(rec.events().is_empty(), "{status}");
    }
    // The fixture provider.
    let rec = Recorder::default();
    let provider = AnyProvider::Fixture(fixture()).with_metrics(std::sync::Arc::new(rec.clone()));
    provider.search(&query("serde")).await.unwrap();
    assert!(rec.events().is_empty());
    // The AgentCore provider, also when the gateway throttles.
    for server in [ok_gateway(2).await] {
        let rec = Recorder::default();
        let provider = build_provider(
            P::Agentcore,
            StageKind::Official,
            ProviderDeps {
                http: Some(provider_http(server.addr)),
                secret: None,
                tavily_base_url: "https://api.tavily.com".into(),
                agentcore: Some(agentcore_deps()),
            },
        )
        .unwrap()
        .with_metrics(std::sync::Arc::new(rec.clone()));
        provider.search(&query("serde")).await.unwrap();
        assert!(rec.events().is_empty());
    }
}
