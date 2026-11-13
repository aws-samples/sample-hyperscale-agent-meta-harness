//! The research route on an `auto` stage: the real handler over the in-memory table and bucket, with two scripted providers
//! behind the stage's one rule. A Tavily key that is stored (the key probe says so) makes the Tavily half serve search, page fetch and limits; no key makes
//! the AgentCore half serve search, answers fetch with `disabled` and reports the AgentCore query cap in limits. The choice follows the probe on every
//! call and never an earlier failure: a refused key, a 429 and a plan limit keep their own words and the other half is never asked. No test reaches the
//! network or AWS.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::research::{
    PolicyItem, ResearchErrorCode, ResearchLimits, ResearchProvider, route,
};
use research_api::fakes::{FixedClock, MemBlobs, MemTable, RecordingInvoker, ScriptedProvider};
use research_api::handler::{Config, Deps, handle};
use research_api::ports::{
    ExtractOutcome, Fail, KeyProbe, Provider, SearchOutcome, StageKind, Table,
};
use research_api::providers::AutoProvider;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use support::{NOW_MS, ROLE, RUN, extracted, outcome, page, request, result};

const NO_KEY: u8 = 0;
const STORED: u8 = 1;
const UNREADABLE: u8 = 2;

const DOC: &str = "https://docs.example.org/guide";

/// What the stage's key probe answers right now, and how often it was asked.
#[derive(Default)]
struct Key {
    mode: AtomicU8,
    probes: AtomicU32,
}

impl Key {
    fn set(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    fn probes(&self) -> u32 {
        self.probes.load(Ordering::SeqCst)
    }
}

/// One half of the stage: a scripted provider with the caps of the real one it stands for.
struct Half {
    inner: Arc<ScriptedProvider>,
    key: Option<Arc<Key>>,
    can_extract: bool,
    cap: Option<usize>,
}

impl Provider for Half {
    fn id(&self) -> ResearchProvider {
        self.inner.id()
    }

    fn max_query_chars(&self) -> Option<usize> {
        self.cap
    }

    fn can_extract(&self) -> bool {
        self.can_extract
    }

    async fn search(
        &self,
        req: &harness_protocol::research::SearchRequest,
    ) -> Result<SearchOutcome, Fail> {
        self.inner.search(req).await
    }

    async fn extract(&self, urls: &[String], query: Option<&str>) -> Result<ExtractOutcome, Fail> {
        self.inner.extract(urls, query).await
    }
}

impl KeyProbe for Half {
    async fn key_stored(&self) -> Result<bool, Fail> {
        let Some(key) = &self.key else {
            return Ok(false);
        };
        key.probes.fetch_add(1, Ordering::SeqCst);
        match key.mode.load(Ordering::SeqCst) {
            STORED => Ok(true),
            NO_KEY => Ok(false),
            _ => Err(Fail::upstream("the key store did not answer")),
        }
    }
}

type AutoDeps = Deps<MemTable, MemBlobs, AutoProvider<Half>, RecordingInvoker, FixedClock>;

struct World {
    d: AutoDeps,
    tavily: Arc<ScriptedProvider>,
    agent: Arc<ScriptedProvider>,
    key: Arc<Key>,
}

fn world_with(mode: u8, limits: ResearchLimits) -> World {
    let tavily = Arc::new(ScriptedProvider::new(ResearchProvider::Tavily));
    let agent = Arc::new(ScriptedProvider::new(ResearchProvider::Agentcore));
    let key = Arc::new(Key::default());
    key.set(mode);
    let provider = AutoProvider::new(
        Half {
            inner: tavily.clone(),
            key: Some(key.clone()),
            can_extract: true,
            cap: None,
        },
        Half {
            inner: agent.clone(),
            key: None,
            can_extract: false,
            cap: Some(200),
        },
    );
    let d = Deps {
        table: MemTable::new(),
        blobs: MemBlobs::new(),
        provider,
        invoker: RecordingInvoker::new(),
        clock: FixedClock::at_ms(NOW_MS),
        cfg: Config {
            worker_role_name: ROLE.into(),
            stage_rate_per_min: 80,
            stage_kind: StageKind::Official,
        },
    };
    d.table.put_policy(PolicyItem::new(
        RUN,
        true,
        ResearchProvider::Agentcore,
        limits,
        NOW_MS / 1000 + 8 * 3600,
    ));
    World {
        d,
        tavily,
        agent,
        key,
    }
}

fn world(mode: u8) -> World {
    world_with(mode, ResearchLimits::default())
}

/// What the gateway answers: results without credits, with its own result id.
fn gateway_outcome() -> Result<SearchOutcome, Fail> {
    Ok(SearchOutcome {
        results: vec![result("G", DOC, "an excerpt the route does not store")],
        credits: None,
        upstream_request_id: Some("gw-1".into()),
    })
}

fn tavily_outcome() -> Result<SearchOutcome, Fail> {
    Ok(outcome(vec![result("T", DOC, "a Tavily excerpt")]))
}

async fn post(w: &World, path: &str, body: Value) -> (u16, Value) {
    let resp = handle(&w.d, &request(path, &body)).await;
    (
        resp.status,
        serde_json::from_str(&resp.body).unwrap_or(Value::Null),
    )
}

fn audits_of(w: &World) -> Vec<Value> {
    let prefix = format!("runs/{RUN}/agents/direct/research/audit/");
    w.d.blobs
        .keys()
        .into_iter()
        .filter(|k| k.starts_with(&prefix))
        .map(|k| serde_json::from_slice(&w.d.blobs.bytes(&k).unwrap()).unwrap())
        .collect()
}

fn search_record(w: &World, n: u32) -> Value {
    serde_json::from_slice(
        &w.d.blobs
            .bytes(&format!(
                "runs/{RUN}/agents/direct/research/searches/s-{n:04}.json"
            ))
            .unwrap(),
    )
    .unwrap()
}

// ---- search: the rule ------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn with_a_key_stored_tavily_serves_the_search_and_the_gateway_is_not_asked() {
    let w = world(STORED);
    w.tavily.push_search(tavily_outcome());
    let (status, v) = post(&w, route::SEARCH, json!({"query": "serde derive"})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["provider"], "tavily");
    assert_eq!(v["credits"], 1);
    assert_eq!(v["results"][0]["snippet"], "a Tavily excerpt");
    assert_eq!(w.tavily.asked(), ["search:serde derive"]);
    assert!(w.agent.asked().is_empty());
    // What is stored is what Tavily's stage always stored.
    let record = search_record(&w, 1);
    assert_eq!(record["provider"], "tavily");
    assert_eq!(record["results"][0]["snippet"], "a Tavily excerpt");
    let a = &audits_of(&w)[0];
    assert_eq!(
        (a["provider"].as_str(), a["upstreamRequestId"].as_str()),
        (Some("tavily"), Some("tvly-req-1"))
    );
    assert_eq!(a["credits"], 1);
}

#[tokio::test]
async fn with_no_key_stored_agentcore_serves_the_search_and_tavily_is_not_asked() {
    let w = world(NO_KEY);
    w.agent.push_search(gateway_outcome());
    let (status, v) = post(&w, route::SEARCH, json!({"query": "serde derive"})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["provider"], "agentcore");
    // The live answer carries the excerpt; the stored record keeps titles and URLs only (the Web Search Tool's terms).
    assert_eq!(
        v["results"][0]["snippet"],
        "an excerpt the route does not store"
    );
    assert_eq!(w.agent.asked(), ["search:serde derive"]);
    assert!(w.tavily.asked().is_empty(), "no query goes to Tavily");
    let record = search_record(&w, 1);
    assert_eq!(record["provider"], "agentcore");
    assert_eq!(record["results"][0]["snippet"], "");
    let a = &audits_of(&w)[0];
    assert_eq!(
        (a["provider"].as_str(), a["upstreamRequestId"].as_str()),
        (Some("agentcore"), Some("gw-1"))
    );
    assert!(a.get("credits").is_none());
}

#[tokio::test]
async fn a_key_stored_or_cleared_switches_the_next_call_and_only_the_next_call() {
    let w = world(NO_KEY);
    w.agent.push_search(gateway_outcome());
    w.tavily.push_search(tavily_outcome());
    w.agent.push_search(gateway_outcome());
    let (_, first) = post(&w, route::SEARCH, json!({"query": "one"})).await;
    w.key.set(STORED);
    let (_, second) = post(&w, route::SEARCH, json!({"query": "two"})).await;
    w.key.set(NO_KEY);
    let (_, third) = post(&w, route::SEARCH, json!({"query": "three"})).await;
    assert_eq!(
        [&first, &second, &third].map(|v| v["provider"].as_str().unwrap().to_string()),
        ["agentcore", "tavily", "agentcore"]
    );
    assert_eq!(w.agent.asked(), ["search:one", "search:three"]);
    assert_eq!(w.tavily.asked(), ["search:two"]);
    assert_eq!(search_record(&w, 2)["provider"], "tavily");
}

#[tokio::test]
async fn the_provider_that_serves_a_call_names_every_record_of_that_call() {
    let w = world(STORED);
    w.tavily.push_search(tavily_outcome());
    w.tavily
        .push_extract(Ok(extracted(vec![page(DOC, "the guide text")])));
    assert_eq!(
        post(&w, route::SEARCH, json!({"query": "guide"})).await.0,
        200
    );
    let (status, v) = post(&w, route::FETCH, json!({"urls": [DOC]})).await;
    assert_eq!(status, 200, "{v}");
    let manifest: Value = serde_json::from_slice(
        &w.d.blobs
            .bytes(v["items"][0]["manifestKey"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["provider"], "tavily");
    for a in audits_of(&w) {
        assert_eq!(a["provider"], "tavily", "{a}");
    }
}

// ---- no fallback on errors -------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_key_that_tavily_refuses_answers_no_key_and_the_query_never_goes_to_the_gateway() {
    let w = world(STORED);
    w.tavily.push_search(Err(Fail::new(
        ResearchErrorCode::NoKey,
        "Tavily refused the stored key",
    )));
    let (status, v) = post(&w, route::SEARCH, json!({"query": "serde"})).await;
    assert_eq!((status, v["code"].as_str()), (503, Some("no_key")), "{v}");
    assert_eq!(w.tavily.asked().len(), 1);
    assert!(w.agent.asked().is_empty(), "no fallback to the gateway");
}

#[tokio::test]
async fn a_429_and_a_plan_limit_keep_their_words_and_the_gateway_is_never_asked() {
    let w = world(STORED);
    w.tavily.push_search(Err(Fail::new(
        ResearchErrorCode::UpstreamRateLimited,
        "Tavily is limiting the rate of calls",
    )
    .with_retry_after(7)));
    w.tavily
        .push_search(Err(Fail::upstream("Tavily answered 432: usage limit")));
    let (status, v) = post(&w, route::SEARCH, json!({"query": "one"})).await;
    assert_eq!(
        (status, v["code"].as_str(), v["retryAfterS"].as_u64()),
        (429, Some("upstream_rate_limited"), Some(7)),
        "{v}"
    );
    let (status, v) = post(&w, route::SEARCH, json!({"query": "two"})).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (502, Some("upstream_error")),
        "{v}"
    );
    assert!(w.agent.asked().is_empty());
}

#[tokio::test]
async fn a_gateway_failure_keeps_its_word_and_tavily_is_never_asked() {
    let w = world(NO_KEY);
    w.agent
        .push_search(Err(Fail::upstream("the gateway did not answer in time")));
    let (status, v) = post(&w, route::SEARCH, json!({"query": "serde"})).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (502, Some("upstream_error")),
        "{v}"
    );
    assert!(w.tavily.asked().is_empty());
}

#[tokio::test]
async fn a_key_store_that_cannot_be_read_fails_the_call_and_sends_nothing_anywhere() {
    let w = world(UNREADABLE);
    let (status, v) = post(&w, route::SEARCH, json!({"query": "serde"})).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (502, Some("upstream_error")),
        "{v}"
    );
    let (status, v) = post(&w, route::FETCH, json!({"urls": [DOC]})).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (502, Some("upstream_error")),
        "{v}"
    );
    assert!(w.tavily.asked().is_empty() && w.agent.asked().is_empty());
    let usage = w.d.table.usage(RUN).await.unwrap();
    assert_eq!(
        (usage.searches, usage.fetched_urls, usage.outbound_bytes),
        (0, 0, 0)
    );
    assert!(
        w.d.blobs
            .keys()
            .iter()
            .all(|k| !k.contains("/searches/") && !k.contains("/items/")),
        "nothing is stored"
    );
}

// ---- fetch ------------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_fetch_with_a_key_stored_is_served_by_tavily() {
    let w = world(STORED);
    w.tavily.push_search(tavily_outcome());
    w.tavily
        .push_extract(Ok(extracted(vec![page(DOC, "the guide text")])));
    post(&w, route::SEARCH, json!({"query": "guide"})).await;
    let (status, v) = post(&w, route::FETCH, json!({"urls": [DOC]})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["items"][0]["kind"], "page");
    assert_eq!(
        w.tavily.asked(),
        ["search:guide", format!("extract:{DOC}").as_str()]
    );
    assert!(w.agent.asked().is_empty());
}

#[tokio::test]
async fn a_fetch_with_no_key_is_disabled_before_anything_is_reserved_or_sent() {
    let w = world(NO_KEY);
    w.agent.push_search(gateway_outcome());
    post(&w, route::SEARCH, json!({"query": "guide"})).await;
    let (status, v) = post(&w, route::FETCH, json!({"urls": [DOC]})).await;
    assert_eq!((status, v["code"].as_str()), (503, Some("disabled")), "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("agentcore") && message.contains("cannot fetch pages"),
        "{message}"
    );
    assert!(w.tavily.asked().is_empty());
    assert_eq!(
        w.agent.asked(),
        ["search:guide"],
        "the gateway is not asked to fetch"
    );
    let usage = w.d.table.usage(RUN).await.unwrap();
    assert_eq!((usage.fetched_urls, usage.context_bytes), (0, 0));
    let fetch = audits_of(&w)
        .into_iter()
        .find(|a| a["op"] == "fetch")
        .expect("the refused fetch is audited");
    assert_eq!(
        (
            fetch["provider"].as_str(),
            fetch["status"].as_str(),
            fetch["code"].as_str()
        ),
        (Some("agentcore"), Some("refused"), Some("disabled"))
    );
}

#[tokio::test]
async fn a_key_stored_after_a_disabled_fetch_turns_the_next_fetch_on() {
    let w = world(NO_KEY);
    w.agent.push_search(gateway_outcome());
    post(&w, route::SEARCH, json!({"query": "guide"})).await;
    assert_eq!(post(&w, route::FETCH, json!({"urls": [DOC]})).await.0, 503);
    w.key.set(STORED);
    w.tavily
        .push_extract(Ok(extracted(vec![page(DOC, "the guide text")])));
    let (status, v) = post(&w, route::FETCH, json!({"urls": [DOC]})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(w.tavily.asked(), [format!("extract:{DOC}")]);
}

// ---- limits -----------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn limits_with_a_key_stored_report_tavily_and_the_run_limits_as_they_are() {
    let w = world(STORED);
    let (status, v) = post(&w, route::LIMITS, json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["provider"], "tavily");
    assert_eq!(
        v["limits"],
        serde_json::to_value(ResearchLimits::default()).unwrap()
    );
}

#[tokio::test]
async fn limits_with_no_key_report_agentcore_and_its_200_character_query_cap() {
    let w = world(NO_KEY);
    let (status, v) = post(&w, route::LIMITS, json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["provider"], "agentcore");
    let mut expected = serde_json::to_value(ResearchLimits::default()).unwrap();
    expected["queryChars"] = json!(200);
    assert_eq!(v["limits"], expected, "only the query cap changes");
    // The route accepts what the answer promises: 200 characters pass, 201 are refused as `usage`.
    w.agent.push_search(gateway_outcome());
    assert_eq!(
        post(&w, route::SEARCH, json!({"query": "a".repeat(201)}))
            .await
            .0,
        400
    );
    assert!(w.agent.asked().is_empty());
}

#[tokio::test]
async fn limits_never_raise_a_run_limit_that_is_already_lower_than_the_providers_cap() {
    let low = ResearchLimits {
        query_chars: 100,
        ..ResearchLimits::default()
    };
    let w = world_with(NO_KEY, low);
    let (_, v) = post(&w, route::LIMITS, json!({})).await;
    assert_eq!(v["limits"]["queryChars"], 100);
}

#[tokio::test]
async fn limits_follow_a_key_stored_or_cleared() {
    let w = world(NO_KEY);
    assert_eq!(
        post(&w, route::LIMITS, json!({})).await.1["provider"],
        "agentcore"
    );
    w.key.set(STORED);
    let (_, v) = post(&w, route::LIMITS, json!({})).await;
    assert_eq!(
        (v["provider"].as_str(), v["limits"]["queryChars"].as_u64()),
        (Some("tavily"), Some(400))
    );
    w.key.set(NO_KEY);
    assert_eq!(
        post(&w, route::LIMITS, json!({})).await.1["provider"],
        "agentcore"
    );
}

#[tokio::test]
async fn limits_still_answer_when_the_key_store_cannot_be_read_and_name_no_provider() {
    let w = world(UNREADABLE);
    let (status, v) = post(&w, route::LIMITS, json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert!(v.get("provider").is_none_or(Value::is_null), "{v}");
    assert_eq!(
        v["limits"],
        serde_json::to_value(ResearchLimits::default()).unwrap()
    );
    assert_eq!(v["enabled"], true);
}

// ---- the provider's own query cap -------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_201_character_query_is_refused_when_agentcore_serves_and_accepted_when_tavily_does() {
    let w = world(NO_KEY);
    // 205 characters of plain words: over the gateway's cap of 200, under the route's 400.
    let long = "rust ".repeat(41);
    let (status, v) = post(&w, route::SEARCH, json!({"query": long})).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("usage")), "{v}");
    assert!(v["message"].as_str().unwrap().contains("200"), "{v}");
    assert!(w.agent.asked().is_empty() && w.tavily.asked().is_empty());
    w.key.set(STORED);
    w.tavily.push_search(tavily_outcome());
    let (status, v) = post(&w, route::SEARCH, json!({"query": long})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["provider"], "tavily");
}

// ---- the stage-wide rate, repositories, and replays ------------------------------------------------------------------------------------

#[tokio::test]
async fn the_stage_wide_rate_counts_the_calls_of_both_providers() {
    let mut w = world(NO_KEY);
    w.d.cfg.stage_rate_per_min = 2;
    w.agent.push_search(gateway_outcome());
    w.tavily.push_search(tavily_outcome());
    assert_eq!(
        post(&w, route::SEARCH, json!({"query": "one"})).await.0,
        200
    );
    w.key.set(STORED);
    assert_eq!(
        post(&w, route::SEARCH, json!({"query": "two"})).await.0,
        200
    );
    let (status, v) = post(&w, route::SEARCH, json!({"query": "three"})).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (429, Some("limit_rate")),
        "{v}"
    );
    assert_eq!(w.agent.asked().len() + w.tavily.asked().len(), 2);
}

#[tokio::test]
async fn repository_calls_read_no_key_and_their_audit_object_names_the_github_archive() {
    // Even a key store that is down does not stop a repository download: it uses no search provider.
    let w = world(UNREADABLE);
    let (status, v) = post(
        &w,
        route::REPO,
        json!({"url": "https://github.com/sindresorhus/is-plain-obj", "idempotencyKey": "repo-1"}),
    )
    .await;
    assert_eq!(status, 202, "{v}");
    let job = v["jobId"].as_str().unwrap().to_string();
    let (status, v) = post(&w, route::JOB, json!({"jobId": job})).await;
    assert_eq!((status, v["state"].as_str()), (202, Some("queued")), "{v}");
    assert_eq!(
        w.key.probes(),
        0,
        "the key is not read for a repository call"
    );
    let audits = audits_of(&w);
    assert!(audits.iter().any(|a| a["op"] == "repo"));
    for a in &audits {
        assert_eq!(a["provider"], "github-archive", "{a}");
    }
}

#[tokio::test]
async fn a_retry_with_the_same_idempotency_key_across_a_switch_is_a_conflict_and_sends_nothing() {
    let w = world(NO_KEY);
    w.agent.push_search(gateway_outcome());
    let body = json!({"query": "serde", "idempotencyKey": "k-1"});
    assert_eq!(post(&w, route::SEARCH, body.clone()).await.0, 200);
    // The same request under the same key replays while the choice is the same.
    let (status, again) = post(&w, route::SEARCH, body.clone()).await;
    assert_eq!(status, 200);
    assert_eq!(again["provider"], "agentcore");
    assert_eq!(w.agent.asked().len(), 1, "a replay makes no call");
    // After a key is stored the same request would be Tavily's: the key binds the provider, so an answer of another provider is not replayed.
    w.key.set(STORED);
    let (status, v) = post(&w, route::SEARCH, body).await;
    assert_eq!((status, v["code"].as_str()), (409, Some("conflict")), "{v}");
    assert!(w.tavily.asked().is_empty());
}

// ---- a stage of one provider is unchanged ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_stage_of_one_provider_serves_itself_and_reports_the_run_limits_unchanged() {
    // The blanket stage of every provider: a plain scripted agentcore provider is a stage, and limits do not lower the query cap.
    let agent = ScriptedProvider::new(ResearchProvider::Agentcore).then_search(gateway_outcome());
    let d = Deps {
        table: MemTable::new(),
        blobs: MemBlobs::new(),
        provider: agent,
        invoker: RecordingInvoker::new(),
        clock: FixedClock::at_ms(NOW_MS),
        cfg: Config {
            worker_role_name: ROLE.into(),
            stage_rate_per_min: 80,
            stage_kind: StageKind::Official,
        },
    };
    d.table.put_policy(PolicyItem::new(
        RUN,
        true,
        ResearchProvider::Agentcore,
        ResearchLimits::default(),
        NOW_MS / 1000 + 8 * 3600,
    ));
    let resp = handle(&d, &request(route::LIMITS, &json!({}))).await;
    let v: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(v["provider"], "agentcore");
    assert_eq!(v["limits"]["queryChars"], 400);
    let resp = handle(&d, &request(route::SEARCH, &json!({"query": "serde"}))).await;
    let v: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(v["provider"], "agentcore");
}
