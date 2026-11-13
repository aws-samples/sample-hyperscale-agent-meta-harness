//! The `auto` stage over the real clients: the keyed Tavily client over a key cache with a fake clock and a scripted key
//! source, the AgentCore provider over a fake gateway, and the production stage built by `build_stage` over recorded Secrets Manager answers. The rule is
//! one question asked before every call: is a Tavily key stored now, as the key cache answers it? Yes, Tavily serves. No, the gateway serves. An error
//! of a call never changes the answer of the next question, and no query goes to a service the stored key does not name. Every key value is built at run
//! time. No test reaches the network or AWS.

#![allow(clippy::unwrap_used)]

#[path = "support/net_fake.rs"]
mod net_fake;
#[path = "support/net_gateway.rs"]
mod net_gateway;
#[path = "support/net_keys.rs"]
mod net_keys;

use aws_credential_types::provider::SharedCredentialsProvider;
use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use harness_protocol::research::{ResearchErrorCode as C, ResearchProvider as P, SearchRequest};
use net_fake::{FakeServer, Reply};
use net_gateway::{
    GATEWAY_HOST, GATEWAY_URL, NOW_S, agentcore_deps, calls, credentials, initializes, ok_gateway,
};
use net_keys::{FakeClock, Script, key};
use research_api::agentcore::{AgentcoreProvider, DEFAULT_TOOL, GatewayTarget};
use research_api::fakes::FixedClock;
use research_api::http::testing::TestNet;
use research_api::http::{HttpClient, HttpOptions};
use research_api::metrics::{Event, Metrics};
use research_api::ports::{
    ExtractOutcome, Fail, KeyProbe, Provider, SearchOutcome, Stage, StageKind,
};
use research_api::providers::{
    AnyProvider, AnyStage, AutoProvider, ProviderDeps, StageChoice, build_stage,
};
use research_api::secret::{AwsSecretSource, CachedSecret};
use research_api::tavily::TavilyClient;
use serde_json::json;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TAVILY_OK: &str = r#"{"results":[{"url":"https://example.org/a","title":"A","content":"c","score":0.5}],"request_id":"r1","usage":{"credits":1}}"#;

fn query(q: &str) -> SearchRequest {
    SearchRequest {
        query: q.into(),
        ..Default::default()
    }
}

/// One client that reaches both fakes: Tavily's host and the gateway's host.
fn http_both(tavily: SocketAddr, gateway: SocketAddr) -> HttpClient {
    let mut hosts = BTreeMap::new();
    hosts.insert("api.tavily.com".to_string(), tavily);
    hosts.insert(GATEWAY_HOST.to_string(), gateway);
    HttpClient::with_test_net(
        HttpOptions {
            connect_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_secs(3),
            max_response_bytes: 4 * 1024 * 1024,
        },
        TestNet { hosts },
    )
    .unwrap()
}

// ---- the two halves as one type, for the rule over a key cache with a fake clock ---------------------------------------------------------

enum Half {
    Tavily(TavilyClient<Script, Arc<FakeClock>>),
    Agent(AgentcoreProvider<FixedClock>),
}

impl Provider for Half {
    fn id(&self) -> P {
        match self {
            Self::Tavily(c) => c.id(),
            Self::Agent(a) => a.id(),
        }
    }

    fn max_query_chars(&self) -> Option<usize> {
        match self {
            Self::Tavily(c) => c.max_query_chars(),
            Self::Agent(a) => a.max_query_chars(),
        }
    }

    fn can_extract(&self) -> bool {
        match self {
            Self::Tavily(c) => c.can_extract(),
            Self::Agent(a) => a.can_extract(),
        }
    }

    async fn search(&self, req: &SearchRequest) -> Result<SearchOutcome, Fail> {
        match self {
            Self::Tavily(c) => c.search(req).await,
            Self::Agent(a) => a.search(req).await,
        }
    }

    async fn extract(&self, urls: &[String], query: Option<&str>) -> Result<ExtractOutcome, Fail> {
        match self {
            Self::Tavily(c) => c.extract(urls, query).await,
            Self::Agent(a) => a.extract(urls, query).await,
        }
    }
}

impl KeyProbe for Half {
    async fn key_stored(&self) -> Result<bool, Fail> {
        match self {
            Self::Tavily(c) => c.key_stored().await,
            Self::Agent(_) => Ok(false),
        }
    }
}

struct Rig {
    stage: AutoProvider<Half>,
    tavily: FakeServer,
    gateway: FakeServer,
    clock: Arc<FakeClock>,
    script: Script,
}

async fn rig(tavily_reply: Reply, script: Script) -> Rig {
    let tavily = FakeServer::start(move |_| tavily_reply.clone()).await;
    let gateway = ok_gateway(2).await;
    let http = http_both(tavily.addr, gateway.addr);
    let clock = Arc::new(FakeClock::default());
    let keyed = TavilyClient::keyed(
        http.clone(),
        "https://api.tavily.com".into(),
        CachedSecret::with_clock(script.clone(), clock.clone()),
    );
    let agent = AgentcoreProvider::with_clock(
        http,
        GatewayTarget::parse(GATEWAY_URL).unwrap(),
        DEFAULT_TOOL,
        SharedCredentialsProvider::new(credentials()),
        FixedClock::at_ms(NOW_S * 1000),
    );
    Rig {
        stage: AutoProvider::new(Half::Tavily(keyed), Half::Agent(agent)),
        tavily,
        gateway,
        clock,
        script,
    }
}

impl Rig {
    /// The provider the stage chooses now.
    async fn chosen(&self) -> P {
        self.stage.serve().await.unwrap().id()
    }

    /// One search the way the route does it: choose, then use that one provider for the call.
    async fn search(&self) -> Result<(P, SearchOutcome), Fail> {
        let served = self.stage.serve().await?;
        let id = served.id();
        served.search(&query("serde")).await.map(|o| (id, o))
    }

    fn tavily_calls(&self) -> usize {
        self.tavily.count()
    }

    fn gateway_searches(&self) -> usize {
        calls(&self.gateway).len()
    }
}

// ---- the rule follows the key cache ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_choice_follows_the_key_cache_as_a_key_is_stored_and_cleared() {
    let k = key("one");
    let r = rig(
        Reply::json(200, TAVILY_OK),
        Script::with(vec![Ok(None), Ok(Some(k.clone())), Ok(None)]),
    )
    .await;

    // No key is stored: AgentCore serves, and Tavily is never asked.
    let (id, _) = r.search().await.unwrap();
    assert_eq!(id, P::Agentcore);
    // "No key" is remembered for 30 seconds.
    r.clock.advance_s(29);
    assert_eq!(r.chosen().await, P::Agentcore);
    assert_eq!(r.script.reads(), 1);
    assert_eq!(r.tavily_calls(), 0, "no query went to Tavily");

    // The owner stores a key: within 30 seconds of the last read the next call is Tavily's.
    r.clock.advance_s(2);
    let (id, out) = r.search().await.unwrap();
    assert_eq!(id, P::Tavily);
    assert_eq!(out.credits, Some(1));
    assert_eq!(r.script.reads(), 2);
    assert_eq!(
        r.tavily.requests()[0].header("authorization"),
        Some(format!("Bearer {k}").as_str())
    );

    // A found key is kept 5 minutes; a key cleared in the store is still used until that read expires.
    r.clock.advance_s(299);
    let (id, _) = r.search().await.unwrap();
    assert_eq!(id, P::Tavily);
    assert_eq!(r.script.reads(), 2);
    r.clock.advance_s(2);
    let (id, _) = r.search().await.unwrap();
    assert_eq!(id, P::Agentcore, "the cleared key is read at expiry");
    assert_eq!(r.script.reads(), 3);

    assert_eq!(
        r.tavily_calls(),
        2,
        "Tavily saw only the two calls with a key"
    );
    assert_eq!(r.gateway_searches(), 2);
}

#[tokio::test]
async fn with_no_key_stored_no_query_ever_reaches_tavily() {
    let r = rig(Reply::json(200, TAVILY_OK), Script::with(vec![Ok(None)])).await;
    for _ in 0..3 {
        let (id, out) = r.search().await.unwrap();
        assert_eq!(id, P::Agentcore);
        assert_eq!(out.results.len(), 2);
    }
    assert_eq!(r.tavily_calls(), 0);
    assert_eq!(r.gateway_searches(), 3);
    assert_eq!(initializes(&r.gateway), 1, "one handshake per instance");
}

// ---- no fallback on errors -------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_refused_key_a_429_and_a_plan_limit_keep_their_words_and_the_gateway_is_never_asked() {
    let cases: [(Reply, C, Option<u32>); 5] = [
        (Reply::json(401, r#"{"detail":"x"}"#), C::NoKey, None),
        (Reply::json(403, r#"{"detail":"x"}"#), C::NoKey, None),
        (
            Reply::bytes(429, &[("Retry-After", "9")], b"{}"),
            C::UpstreamRateLimited,
            Some(9),
        ),
        (
            Reply::json(432, r#"{"detail":"plan"}"#),
            C::UpstreamError,
            None,
        ),
        (
            Reply::json(433, r#"{"detail":"payg"}"#),
            C::UpstreamError,
            None,
        ),
    ];
    let held = key("held");
    for (reply, code, retry) in cases {
        let r = rig(reply, Script::keys(&[held.as_str()])).await;
        let err = r.search().await.unwrap_err();
        assert_eq!((err.code, err.retry_after_s), (code, retry), "{err}");
        assert!(r.tavily_calls() >= 1);
        assert_eq!(
            r.gateway_searches(),
            0,
            "{code:?}: no fallback to the gateway"
        );
        assert_eq!(initializes(&r.gateway), 0, "{code:?}: not even a handshake");
        // The next question has the same answer: the failure changed nothing.
        assert_eq!(r.chosen().await, P::Tavily);
    }
}

#[tokio::test]
async fn a_key_store_that_cannot_be_read_with_no_key_held_fails_the_choice_and_sends_nothing() {
    let r = rig(
        Reply::json(200, TAVILY_OK),
        Script::with(vec![Err(Fail::upstream("the key store did not answer"))]),
    )
    .await;
    let err = r.search().await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert_eq!((r.tavily_calls(), r.gateway.count()), (0, 0));
}

#[tokio::test]
async fn a_key_read_within_the_hour_is_still_used_while_the_store_is_down_and_then_the_choice_fails()
 {
    let r = rig(
        Reply::json(200, TAVILY_OK),
        Script::with(vec![
            Ok(Some(key("held"))),
            Err(Fail::upstream("the key store did not answer")),
        ]),
    )
    .await;
    assert_eq!(r.chosen().await, P::Tavily);
    r.clock.advance_s(301);
    assert_eq!(
        r.chosen().await,
        P::Tavily,
        "the store is down; the key read 301 s ago is served"
    );
    r.clock.advance_s(3300);
    let err = r.stage.serve().await.err().expect("the choice fails");
    assert_eq!(err.code, C::UpstreamError);
    assert_eq!(
        r.gateway.count(),
        0,
        "the failure sent nothing to the gateway"
    );
}

// ---- the production stage over recorded Secrets Manager answers --------------------------------------------------------------------------

/// A Secrets Manager client that replays `answers` in order; the client is returned too, so a test can count the requests it saw.
fn secret_over(answers: Vec<(u16, String)>) -> (CachedSecret<AwsSecretSource>, StaticReplayClient) {
    let events = answers
        .into_iter()
        .map(|(status, body)| {
            ReplayEvent::new(
                http::Request::builder()
                    .uri("https://example.invalid/")
                    .body(SdkBody::empty())
                    .unwrap(),
                http::Response::builder()
                    .status(status)
                    .header("content-type", "application/x-amz-json-1.1")
                    .body(SdkBody::from(body))
                    .unwrap(),
            )
        })
        .collect();
    let client = StaticReplayClient::new(events);
    let conf = aws_sdk_secretsmanager::Config::builder()
        .behavior_version_latest()
        .region(aws_sdk_secretsmanager::config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_secretsmanager::config::Credentials::for_tests())
        .http_client(client.clone())
        .build();
    (
        CachedSecret::new(AwsSecretSource::new(
            aws_sdk_secretsmanager::Client::from_conf(conf),
            "hamh-direct-research-tavily-api-key".into(),
        )),
        client,
    )
}

fn stored(value: &str, created_s: Option<u64>) -> (u16, String) {
    let mut answer = json!({
        "ARN": "arn:aws:secretsmanager:us-east-1:111122223333:secret:hamh-direct-research-tavily-api-key-AbCdEf",
        "Name": "hamh-direct-research-tavily-api-key", "VersionId": "v1", "SecretString": value, "VersionStages": ["AWSCURRENT"]
    });
    if let Some(created) = created_s {
        answer["CreatedDate"] = json!(created);
    }
    (200, answer.to_string())
}

fn no_current_version() -> (u16, String) {
    (
        400,
        r#"{"__type":"ResourceNotFoundException","Message":"Secrets Manager can't find the specified secret value for staging label: AWSCURRENT"}"#
            .to_string(),
    )
}

async fn production_stage(
    tavily: &FakeServer,
    gateway: &FakeServer,
    secret: CachedSecret<AwsSecretSource>,
) -> AnyStage {
    build_stage(
        StageChoice::Auto,
        StageKind::Official,
        ProviderDeps {
            http: Some(http_both(tavily.addr, gateway.addr)),
            secret: Some(secret),
            tavily_base_url: "https://api.tavily.com".into(),
            agentcore: Some(agentcore_deps()),
        },
    )
    .unwrap()
}

#[tokio::test]
async fn the_production_stage_serves_from_tavily_when_the_secret_holds_a_key() {
    let tavily = FakeServer::start(|_| Reply::json(200, TAVILY_OK)).await;
    let gateway = ok_gateway(2).await;
    let k = key("prod");
    let (secret, _) = secret_over(vec![stored(&k, None)]);
    let stage = production_stage(&tavily, &gateway, secret).await;
    let served = stage.serve().await.unwrap();
    assert!(matches!(served, AnyProvider::Tavily(_)));
    assert_eq!(served.id(), P::Tavily);
    assert_eq!(
        served.search(&query("serde")).await.unwrap().credits,
        Some(1)
    );
    assert_eq!(
        tavily.requests()[0].header("authorization"),
        Some(format!("Bearer {k}").as_str())
    );
    assert_eq!(gateway.count(), 0);
}

#[tokio::test]
async fn a_blank_value_and_a_secret_with_no_current_version_both_mean_agentcore() {
    for answer in [stored("   ", None), stored("", None), no_current_version()] {
        let tavily = FakeServer::start(|_| Reply::json(200, TAVILY_OK)).await;
        let gateway = ok_gateway(2).await;
        let (secret, _) = secret_over(vec![answer.clone()]);
        let stage = production_stage(&tavily, &gateway, secret).await;
        let served = stage.serve().await.unwrap();
        assert!(matches!(served, AnyProvider::Agentcore(_)), "{answer:?}");
        assert_eq!(
            served.search(&query("serde")).await.unwrap().results.len(),
            2
        );
        assert_eq!(tavily.count(), 0, "{answer:?}: no query went to Tavily");
    }
}

#[tokio::test]
async fn the_production_stage_reports_the_provider_caps_and_names_the_archive_for_repositories() {
    let tavily = FakeServer::start(|_| Reply::json(200, TAVILY_OK)).await;
    let gateway = ok_gateway(2).await;
    let (secret, _) = secret_over(vec![no_current_version()]);
    let stage = production_stage(&tavily, &gateway, secret).await;
    assert!(stage.reports_provider_caps());
    assert_eq!(stage.audit_word(), P::GithubArchive);
    let served = stage.serve().await.unwrap();
    assert_eq!(served.max_query_chars(), Some(200));
    assert!(!served.can_extract());
}

#[tokio::test]
async fn a_stage_that_names_one_provider_serves_it_without_reading_the_key() {
    let tavily = FakeServer::start(|_| Reply::json(200, TAVILY_OK)).await;
    let gateway = ok_gateway(2).await;
    for (choice, expected) in [
        (StageChoice::Pinned(P::Tavily), P::Tavily),
        (StageChoice::Pinned(P::Agentcore), P::Agentcore),
    ] {
        let (secret, client) = secret_over(vec![stored(&key("pin"), None)]);
        let stage = build_stage(
            choice,
            StageKind::Official,
            ProviderDeps {
                http: Some(http_both(tavily.addr, gateway.addr)),
                secret: Some(secret),
                tavily_base_url: "https://api.tavily.com".into(),
                agentcore: Some(agentcore_deps()),
            },
        )
        .unwrap();
        assert!(matches!(stage, AnyStage::Fixed(_)));
        assert!(!stage.reports_provider_caps());
        assert_eq!(stage.audit_word(), expected);
        assert_eq!(stage.serve().await.unwrap().id(), expected);
        assert_eq!(
            client.actual_requests().count(),
            0,
            "{expected:?}: no key read to choose"
        );
    }
}

#[test]
fn an_auto_stage_needs_the_client_the_key_secret_and_the_gateway_settings() {
    let (secret, _) = secret_over(vec![]);
    let addr = SocketAddr::from(([127, 0, 0, 1], 9));
    let all = |http: bool, secret: Option<CachedSecret<AwsSecretSource>>, gw: bool| ProviderDeps {
        http: http.then(|| http_both(addr, addr)),
        secret,
        tavily_base_url: "https://api.tavily.com".into(),
        agentcore: gw.then(agentcore_deps),
    };
    for (deps, what) in [
        (all(false, Some(secret), true), "HTTP client"),
        (all(true, None, true), "key secret"),
        (
            all(true, Some(secret_over(vec![]).0), false),
            "gateway settings",
        ),
    ] {
        let err = build_stage(StageChoice::Auto, StageKind::Official, deps).unwrap_err();
        assert_eq!(err.code, C::Internal);
        assert!(err.message.contains(what), "{what}: {}", err.message);
    }
    // The fixture is still a test-stage word.
    let err = build_stage(
        StageChoice::Pinned(P::Fixture),
        StageKind::Official,
        all(false, None, false),
    )
    .unwrap_err();
    assert!(err.message.contains("test stage"), "{}", err.message);
}

// ---- metrics ----------------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Event>>>);

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
async fn the_metrics_sink_reaches_the_inner_tavily_client_when_tavily_serves() {
    let stored_s = 1_792_000_000u64;
    let tavily = FakeServer::start(|_| Reply::json(433, r#"{"detail":{"error":"limit"}}"#)).await;
    let gateway = ok_gateway(2).await;
    let (secret, _) = secret_over(vec![stored(&key("met"), Some(stored_s))]);
    let rec = Recorder::default();
    let stage = production_stage(&tavily, &gateway, secret)
        .await
        .with_metrics(Arc::new(rec.clone()));
    let err = stage
        .serve()
        .await
        .unwrap()
        .search(&query("serde"))
        .await
        .unwrap_err();
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
    assert_eq!(gateway.count(), 0);
}

#[tokio::test]
async fn a_refused_key_is_reported_as_it_is_on_a_tavily_stage() {
    let tavily = FakeServer::start(|_| Reply::json(401, r#"{"detail":"x"}"#)).await;
    let gateway = ok_gateway(2).await;
    let k = key("met2");
    // The refusal makes the client read the secret again (a rotation), so the replay holds two answers.
    let (secret, _) = secret_over(vec![
        stored(&k, Some(1_792_000_000)),
        stored(&k, Some(1_792_000_000)),
    ]);
    let rec = Recorder::default();
    let stage = production_stage(&tavily, &gateway, secret)
        .await
        .with_metrics(Arc::new(rec.clone()));
    let err = stage
        .serve()
        .await
        .unwrap()
        .search(&query("serde"))
        .await
        .unwrap_err();
    assert_eq!(err.code, C::NoKey);
    assert!(
        rec.events().contains(&Event::KeyRefused),
        "{:?}",
        rec.events()
    );
    assert_eq!(gateway.count(), 0, "no fallback");
}

#[tokio::test]
async fn nothing_is_reported_while_agentcore_serves() {
    let tavily = FakeServer::start(|_| Reply::json(200, TAVILY_OK)).await;
    let gateway = ok_gateway(2).await;
    let (secret, _) = secret_over(vec![no_current_version()]);
    let rec = Recorder::default();
    let stage = production_stage(&tavily, &gateway, secret)
        .await
        .with_metrics(Arc::new(rec.clone()));
    stage
        .serve()
        .await
        .unwrap()
        .search(&query("serde"))
        .await
        .unwrap();
    assert!(rec.events().is_empty(), "{:?}", rec.events());
}

// ---- the operator's key-state question -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_production_auto_stage_reports_tavily_and_the_version_of_the_key_it_holds() {
    let tavily = FakeServer::start(|_| Reply::json(200, TAVILY_OK)).await;
    let gateway = ok_gateway(2).await;
    let (secret, client) = secret_over(vec![stored(&key("ks"), None)]);
    let stage = production_stage(&tavily, &gateway, secret).await;
    let state = stage.key_state().await.unwrap();
    assert_eq!(state.provider, P::Tavily);
    assert_eq!(state.version_id.as_deref(), Some("v1"));
    assert_eq!(
        client.actual_requests().count(),
        1,
        "the choice and the version come from one read"
    );
    assert_eq!(
        (tavily.count(), gateway.count()),
        (0, 0),
        "the question sends nothing to Tavily or the gateway"
    );
}

#[tokio::test]
async fn with_no_key_stored_the_auto_stage_reports_agentcore_and_no_version() {
    for answer in [stored("   ", None), stored("", None), no_current_version()] {
        let tavily = FakeServer::start(|_| Reply::json(200, TAVILY_OK)).await;
        let gateway = ok_gateway(2).await;
        let (secret, _) = secret_over(vec![answer.clone()]);
        let stage = production_stage(&tavily, &gateway, secret).await;
        let state = stage.key_state().await.unwrap();
        assert_eq!(state.provider, P::Agentcore, "{answer:?}");
        assert_eq!(state.version_id, None, "{answer:?}");
        assert_eq!((tavily.count(), gateway.count()), (0, 0));
    }
}

#[tokio::test]
async fn a_key_store_that_cannot_be_read_fails_the_question_and_sends_nothing() {
    let tavily = FakeServer::start(|_| Reply::json(200, TAVILY_OK)).await;
    let gateway = ok_gateway(2).await;
    let denied = (
        400,
        r#"{"__type":"AccessDeniedException","Message":"marker-text"}"#.to_string(),
    );
    let (secret, _) = secret_over(vec![denied]);
    let stage = production_stage(&tavily, &gateway, secret).await;
    let err = stage.key_state().await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert!(!err.message.contains("marker-text"));
    assert_eq!((tavily.count(), gateway.count()), (0, 0));
}

#[tokio::test]
async fn a_stage_pinned_to_tavily_reports_the_version_and_one_pinned_to_agentcore_reads_no_key() {
    let tavily = FakeServer::start(|_| Reply::json(200, TAVILY_OK)).await;
    let gateway = ok_gateway(2).await;
    for (choice, expected, version, reads) in [
        (StageChoice::Pinned(P::Tavily), P::Tavily, Some("v1"), 1),
        (StageChoice::Pinned(P::Agentcore), P::Agentcore, None, 0),
    ] {
        let (secret, client) = secret_over(vec![stored(&key("pin"), None)]);
        let stage = build_stage(
            choice,
            StageKind::Official,
            ProviderDeps {
                http: Some(http_both(tavily.addr, gateway.addr)),
                secret: Some(secret),
                tavily_base_url: "https://api.tavily.com".into(),
                agentcore: Some(agentcore_deps()),
            },
        )
        .unwrap();
        let state = stage.key_state().await.unwrap();
        assert_eq!(state.provider, expected);
        assert_eq!(state.version_id.as_deref(), version);
        assert_eq!(client.actual_requests().count(), reads, "{expected:?}");
    }
}

#[tokio::test]
async fn the_question_leaves_the_creation_time_for_the_key_metrics() {
    // The metrics hand out the creation time of each read once (`take_load`); asking for the version must not use it up.
    let tavily = FakeServer::start(|_| Reply::json(200, TAVILY_OK)).await;
    let gateway = ok_gateway(2).await;
    let (secret, _) = secret_over(vec![stored(&key("tl"), Some(1_792_929_600))]);
    let recorder = Recorder::default();
    let stage = production_stage(&tavily, &gateway, secret)
        .await
        .with_metrics(Arc::new(recorder.clone()));
    stage.key_state().await.unwrap();
    assert!(
        recorder.events().is_empty(),
        "a question is not a Tavily answer"
    );
    let served = stage.serve().await.unwrap();
    served.search(&query("serde")).await.unwrap();
    assert!(
        recorder.events().contains(&Event::KeyLoaded {
            stored_at_ms: 1_792_929_600_000
        }),
        "{:?}",
        recorder.events()
    );
}
