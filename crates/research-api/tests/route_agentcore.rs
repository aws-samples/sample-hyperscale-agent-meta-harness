//! The research route over the `agentcore` search provider: the real handler, the in-memory table and bucket, the real provider
//! and a scripted fake gateway on 127.0.0.1. The service terms bar extracting, storing or reproducing search results in bulk, so what the route
//! stores of a search holds titles and URLs only: the tests give every excerpt a unique marker and look for it in every stored byte. A control
//! sees the marker in the live answer, so the negative checks can fail. No test reaches the network or AWS.
#![allow(clippy::unwrap_used)]

mod support;

#[path = "support/net_fake.rs"]
mod net_fake;
#[path = "support/net_gateway.rs"]
mod net_gateway;

use harness_protocol::research::{ResearchLimits, ResearchProvider, route};
use net_fake::{FakeServer, Reply};
use net_gateway::*;
use research_api::agentcore::AgentcoreProvider;
use research_api::fakes::{FixedClock, MemBlobs, MemTable, RecordingInvoker, ScriptedProvider};
use research_api::handler::{Config, Deps, handle};
use research_api::ports::StageKind;
use serde_json::{Value, json};
use support::{NOW_MS, ROLE, RUN, outcome, request, result};

type AgentDeps =
    Deps<MemTable, MemBlobs, AgentcoreProvider<FixedClock>, RecordingInvoker, FixedClock>;

const MARK: &str = "ZQX-MARK";

fn policy_row(table: &MemTable) {
    table.put_policy(harness_protocol::research::PolicyItem::new(
        RUN,
        true,
        ResearchProvider::Agentcore,
        ResearchLimits::default(),
        NOW_MS / 1000 + 8 * 3600,
    ));
}

fn deps_over(server: &FakeServer, table: MemTable, blobs: MemBlobs, stage_rate: u32) -> AgentDeps {
    policy_row(&table);
    Deps {
        table,
        blobs,
        provider: provider(server),
        invoker: RecordingInvoker::new(),
        clock: FixedClock::at_ms(NOW_MS),
        cfg: Config {
            worker_role_name: ROLE.into(),
            stage_rate_per_min: stage_rate,
            stage_kind: StageKind::Official,
        },
    }
}

fn deps(server: &FakeServer) -> AgentDeps {
    deps_over(server, MemTable::new(), MemBlobs::new(), 80)
}

async fn post(d: &AgentDeps, path: &str, body: Value) -> (u16, Value) {
    let resp = handle(d, &request(path, &body)).await;
    (
        resp.status,
        serde_json::from_str(&resp.body).unwrap_or(Value::Null),
    )
}

fn audits_of(d: &AgentDeps) -> Vec<Value> {
    let prefix = format!("runs/{RUN}/agents/direct/research/audit/");
    d.blobs
        .keys()
        .into_iter()
        .filter(|k| k.starts_with(&prefix))
        .map(|k| serde_json::from_slice(&d.blobs.bytes(&k).unwrap()).unwrap())
        .collect()
}

async fn marked_gateway() -> FakeServer {
    gateway(|rec| tool_ok(rec, &results_text(3, MARK))).await
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

// ---- what is shown and what is stored --------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_search_shows_the_excerpts_once_and_stores_titles_and_urls_only() {
    let server = marked_gateway().await;
    let d = deps(&server);
    let (status, body) = post(
        &d,
        route::SEARCH,
        json!({"query": "serde derive", "idempotencyKey": "k-1"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["provider"], "agentcore");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    // The control: the caller got the excerpt and the date, so a missing marker below is the route's doing.
    assert!(results[0]["snippet"].as_str().unwrap().contains(MARK));
    assert_eq!(results[0]["published"], "2025-03-01");

    // No stored byte holds an excerpt: not the bucket (the search record, the audit object), not the idempotent answer.
    let keys = d.blobs.keys();
    assert!(keys.iter().any(|k| k.contains("/searches/s-0001.json")));
    for key in &keys {
        assert!(
            !contains(&d.blobs.bytes(key).unwrap(), MARK),
            "{key} holds an excerpt"
        );
    }
    let idem = d
        .table
        .idem_answer(RUN, "k-1")
        .expect("the answer is stored");
    assert!(!idem.contains(MARK), "{idem}");

    // The search record keeps what the terms allow: the order, the titles and the URLs.
    let record: Value = serde_json::from_slice(
        &d.blobs
            .bytes(&format!(
                "runs/{RUN}/agents/direct/research/searches/s-0001.json"
            ))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(record["provider"], "agentcore");
    assert_eq!(record["query"], "serde derive");
    for (i, r) in record["results"].as_array().unwrap().iter().enumerate() {
        assert_eq!(r["title"], format!("Result {i}"));
        assert_eq!(r["url"], format!("https://docs.example.org/page-{i}"));
        assert_eq!(r["snippet"], "", "no excerpt text is kept");
        assert!(
            r.get("published").is_none() && r.get("score").is_none(),
            "only titles and URLs are kept: {r}"
        );
    }
    let stored: Value = serde_json::from_str(&idem).unwrap();
    assert_eq!(stored["results"][0]["snippet"], "");
    assert!(stored["results"][0].get("published").is_none());
}

#[tokio::test]
async fn the_audit_object_names_agentcore_the_exact_query_and_the_result_id() {
    let server = marked_gateway().await;
    let d = deps(&server);
    post(&d, route::SEARCH, json!({"query": "tokio spawn_blocking"})).await;
    let audits = audits_of(&d);
    assert_eq!(audits.len(), 1);
    let a = &audits[0];
    assert_eq!(a["provider"], "agentcore");
    assert_eq!(a["op"], "search");
    assert_eq!(a["status"], "ok");
    assert_eq!(a["input"]["query"], "tokio spawn_blocking");
    assert_eq!(a["upstreamRequestId"], "31ff0adc");
    assert!(
        a.get("credits").is_none(),
        "the Web Search Tool reports no credits"
    );
    assert!(a["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_replay_returns_titles_and_urls_with_empty_excerpts_and_makes_no_call() {
    let server = marked_gateway().await;
    let d = deps(&server);
    let first = json!({"query": "serde derive", "idempotencyKey": "k-1"});
    let (_, live) = post(&d, route::SEARCH, first.clone()).await;
    assert_eq!(calls(&server).len(), 1);
    let (status, replay) = post(&d, route::SEARCH, first).await;
    assert_eq!(status, 200);
    assert_eq!(calls(&server).len(), 1, "a replay spends no search");
    assert_eq!(replay["id"], live["id"]);
    for (a, b) in live["results"]
        .as_array()
        .unwrap()
        .iter()
        .zip(replay["results"].as_array().unwrap())
    {
        assert_eq!((&a["title"], &a["url"]), (&b["title"], &b["url"]));
        assert_eq!(b["snippet"], "");
        assert!(b.get("published").is_none());
    }
    let (_, limits) = post(&d, route::LIMITS, json!({})).await;
    assert_eq!(limits["usage"]["searches"], 1);
    // The same key with another request is still a conflict.
    let (status, body) = post(
        &d,
        route::SEARCH,
        json!({"query": "something else", "idempotencyKey": "k-1"}),
    )
    .await;
    assert_eq!((status, body["code"].as_str()), (409, Some("conflict")));
}

#[tokio::test]
async fn a_key_used_under_tavily_is_not_replayed_by_an_agentcore_stage() {
    // The table outlives a change of the stage's provider: an excerpt that Tavily stored must not be handed out by another provider's stage.
    let table = MemTable::new();
    let blobs = MemBlobs::new();
    let tavily =
        ScriptedProvider::new(ResearchProvider::Tavily).then_search(Ok(outcome(vec![result(
            "T",
            "https://docs.example.org/t",
            "a Tavily excerpt",
        )])));
    let before = Deps {
        table: table.clone(),
        blobs: blobs.clone(),
        provider: tavily,
        invoker: RecordingInvoker::new(),
        clock: FixedClock::at_ms(NOW_MS),
        cfg: Config {
            worker_role_name: ROLE.into(),
            stage_rate_per_min: 80,
            stage_kind: StageKind::Official,
        },
    };
    table.put_policy(harness_protocol::research::PolicyItem::new(
        RUN,
        true,
        ResearchProvider::Tavily,
        ResearchLimits::default(),
        NOW_MS / 1000 + 8 * 3600,
    ));
    let body = json!({"query": "serde derive", "idempotencyKey": "k-1"});
    let resp = handle(&before, &request(route::SEARCH, &body)).await;
    assert_eq!(resp.status, 200, "{}", resp.body);

    let server = marked_gateway().await;
    let after = deps_over(&server, table, blobs, 80);
    let (status, v) = post(&after, route::SEARCH, body).await;
    assert_eq!((status, v["code"].as_str()), (409, Some("conflict")), "{v}");
    assert_eq!(server.count(), 0);
}

// ---- the provider's own limits ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_query_over_the_providers_cap_is_refused_before_anything_is_spent() {
    let server = marked_gateway().await;
    let d = deps(&server);
    let (status, v) = post(
        &d,
        route::SEARCH,
        json!({"query": "a".repeat(201), "idempotencyKey": "k-9"}),
    )
    .await;
    assert_eq!((status, v["code"].as_str()), (400, Some("usage")), "{v}");
    assert!(v["message"].as_str().unwrap().contains("200"), "{v}");
    assert_eq!(server.count(), 0, "no handshake, no search");
    let (_, limits) = post(&d, route::LIMITS, json!({})).await;
    assert_eq!(limits["usage"]["searches"], 0);
    assert_eq!(limits["usage"]["outboundBytes"], 0);
    assert!(d.table.idem_answer(RUN, "k-9").is_none());
    let audits = audits_of(&d);
    assert_eq!(audits.len(), 1);
    assert_eq!(
        (audits[0]["status"].as_str(), audits[0]["code"].as_str()),
        (Some("error"), Some("usage"))
    );
    assert!(
        audits[0]["input"].get("query").is_none(),
        "a refused query is not written down"
    );
    // The route's own cap (400) is higher: 200 characters pass.
    let (status, _) = post(&d, route::SEARCH, json!({"query": "rust ".repeat(40)})).await;
    assert_eq!(status, 200, "200 characters of plain words pass");
}

#[tokio::test]
async fn fetch_is_refused_on_an_agentcore_stage_before_anything_is_spent() {
    let server = marked_gateway().await;
    let d = deps(&server);
    let (status, _) = post(&d, route::SEARCH, json!({"query": "serde derive"})).await;
    assert_eq!(status, 200);
    let before = server.count();
    let (status, v) = post(
        &d,
        route::FETCH,
        json!({"urls": ["https://docs.example.org/page-0"]}),
    )
    .await;
    assert_eq!((status, v["code"].as_str()), (503, Some("disabled")), "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("agentcore") && message.contains("tavily"),
        "{message}"
    );
    assert_eq!(server.count(), before, "the gateway is not asked");
    let (_, limits) = post(&d, route::LIMITS, json!({})).await;
    assert_eq!(limits["usage"]["fetchedUrls"], 0);
    assert_eq!(limits["usage"]["contextBytes"], 0);
    let last = audits_of(&d)
        .into_iter()
        .find(|a| a["op"] == "fetch")
        .expect("the refused fetch is audited");
    assert_eq!(
        (
            last["provider"].as_str(),
            last["status"].as_str(),
            last["code"].as_str()
        ),
        (Some("agentcore"), Some("refused"), Some("disabled"))
    );
}

#[tokio::test]
async fn the_stage_wide_rate_applies_to_the_gateway_too() {
    let server = marked_gateway().await;
    let d = deps_over(&server, MemTable::new(), MemBlobs::new(), 2);
    for _ in 0..2 {
        let (status, _) = post(&d, route::SEARCH, json!({"query": "serde"})).await;
        assert_eq!(status, 200);
    }
    let (status, v) = post(&d, route::SEARCH, json!({"query": "serde"})).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (429, Some("limit_rate")),
        "{v}"
    );
    assert_eq!(
        calls(&server).len(),
        2,
        "the third search never reached the gateway"
    );
}

// ---- failures and audit ----------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_throttled_gateway_is_the_routes_rate_limit_word_with_the_retry_hint_and_is_still_counted()
 {
    let server = gateway(|_| Reply::bytes(429, &[("Retry-After", "7")], b"{}")).await;
    let d = deps(&server);
    let (status, v) = post(&d, route::SEARCH, json!({"query": "serde"})).await;
    assert_eq!(
        (status, v["code"].as_str(), v["retryAfterS"].as_u64()),
        (429, Some("upstream_rate_limited"), Some(7)),
        "{v}"
    );
    let (_, limits) = post(&d, route::LIMITS, json!({})).await;
    assert_eq!(
        limits["usage"]["searches"], 1,
        "a failing upstream call stays counted"
    );
    let a = &audits_of(&d)[0];
    assert_eq!(
        (
            a["provider"].as_str(),
            a["status"].as_str(),
            a["code"].as_str()
        ),
        (
            Some("agentcore"),
            Some("error"),
            Some("upstream_rate_limited")
        )
    );
}

#[tokio::test]
async fn every_audit_object_of_an_agentcore_stage_names_agentcore() {
    let server = marked_gateway().await;
    let d = deps(&server);
    let leaked = format!("gh{}{}", "p_", "a1B2c3D4e5F6g7H8i9J0k1L2");
    post(&d, route::SEARCH, json!({"query": "serde"})).await;
    post(&d, route::SEARCH, json!({"query": leaked})).await;
    post(&d, route::SEARCH, json!({"query": "a".repeat(201)})).await;
    post(
        &d,
        route::FETCH,
        json!({"urls": ["https://docs.example.org/page-0"]}),
    )
    .await;
    let audits = audits_of(&d);
    assert_eq!(audits.len(), 4);
    for a in &audits {
        assert_eq!(a["provider"], "agentcore", "{a}");
    }
}
