//! `POST /internal/research/search` and the checks every call passes first: identity, the run's policy row, the rate limits, the
//! request's own bounds. Ordinary input only: oversize, wrong type, over quota, wrong owner.

mod support;

use harness_protocol::research::{ResearchErrorCode, ResearchProvider, route};
use research_api::fakes::ScriptedProvider;
use research_api::ports::Fail;
use serde_json::json;
use support::*;

fn provider_with_two_hits() -> ScriptedProvider {
    fixture_provider().then_search(Ok(outcome(vec![
        result(
            "is-plain-obj",
            "https://www.npmjs.com/package/is-plain-obj",
            "Check if a value is a plain object",
        ),
        result(
            "source",
            "https://github.com/sindresorhus/is-plain-obj",
            &"s".repeat(3000),
        ),
    ])))
}

#[tokio::test]
async fn a_search_answers_stores_its_record_and_writes_one_audit_object() {
    let d = deps(provider_with_two_hits());
    let (status, body, resp) = call(&d, route::SEARCH, q("is-plain-obj plain object")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["id"], "s-0001");
    assert_eq!(body["provider"], "fixture");
    assert_eq!(body["credits"], 1);
    assert_eq!(body["requestId"], resp.log.request_id.as_str());
    assert_eq!(body["usage"]["searches"], 1);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0]["url"],
        "https://www.npmjs.com/package/is-plain-obj"
    );
    // A snippet is cut to 1 KiB.
    assert_eq!(
        results[1]["snippet"].as_str().unwrap().chars().count(),
        1024
    );

    // The record of what Cline was shown is stored.
    let key = format!("runs/{RUN}/agents/direct/research/searches/s-0001.json");
    let stored: serde_json::Value = serde_json::from_slice(&d.blobs.bytes(&key).unwrap()).unwrap();
    assert_eq!(
        stored["results"][1]["snippet"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        1024
    );
    assert_eq!(stored["query"], "is-plain-obj plain object");

    // One audit object, with the exact query and the provider, and no header or key member.
    let a = audits(&d, RUN);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["op"], "search");
    assert_eq!(a[0]["status"], "ok");
    assert_eq!(a[0]["provider"], "fixture");
    assert_eq!(a[0]["input"]["query"], "is-plain-obj plain object");
    assert_eq!(a[0]["input"]["bytesOut"], 25);
    assert_eq!(a[0]["credits"], 1);
    assert_eq!(a[0]["upstreamRequestId"], "tvly-req-1");
    assert!(a[0].get("headers").is_none() && a[0].get("authorization").is_none());

    // The result URLs count as seen: the provenance gate of a later fetch.
    assert_eq!(d.table.seen_count(RUN), 2);
    // The provider was asked once, with the query as sent.
    assert_eq!(d.provider.asked(), vec!["search:is-plain-obj plain object"]);
}

#[tokio::test]
async fn control_characters_in_a_snippet_are_removed_before_it_is_shown_or_stored() {
    let d = deps(fixture_provider().then_search(Ok(outcome(vec![result(
        "t",
        "https://example.org/a",
        "clean\u{1b}[31m red\u{7} text",
    )]))));
    let (status, body, _) = call(&d, route::SEARCH, q("anything")).await;
    assert_eq!(status, 200);
    let snippet = body["results"][0]["snippet"].as_str().unwrap();
    assert!(
        !snippet.contains('\u{1b}') && !snippet.contains('\u{7}'),
        "{snippet:?}"
    );
}

#[tokio::test]
async fn a_caller_that_is_not_a_worker_session_of_this_run_is_refused() {
    let d = deps(fixture_provider());
    for arn in [
        None,
        Some("arn:aws:iam::111122223333:user/someone".to_string()),
        Some(format!(
            "arn:aws:sts::{ACCOUNT}:assumed-role/some-other-role/{RUN}.direct"
        )),
        Some(format!(
            "arn:aws:sts::999988887777:assumed-role/{ROLE}/{RUN}.direct"
        )),
        Some(format!(
            "arn:aws:sts::{ACCOUNT}:assumed-role/{ROLE}/not-a-ulid.direct"
        )),
        Some(format!(
            "arn:aws:sts::{ACCOUNT}:assumed-role/{ROLE}/{RUN}.planner"
        )),
        Some(format!("arn:aws:sts::{ACCOUNT}:assumed-role/{ROLE}/{RUN}")),
    ] {
        let mut req = request(route::SEARCH, &q("x"));
        req.caller_arn = arn.clone();
        let resp = research_api::handler::handle(&d, &req).await;
        assert_eq!(resp.status, 503, "{arn:?}");
        let v: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(v["code"], "disabled", "{arn:?}");
    }
    assert!(d.provider.asked().is_empty());
    assert!(
        d.blobs.keys().is_empty(),
        "a refused identity writes nothing"
    );
}

#[tokio::test]
async fn a_run_without_a_policy_row_or_with_research_off_or_expired_is_disabled() {
    let d = deps(provider_with_two_hits());
    // Another run has no row.
    let mut req = request(route::SEARCH, &q("x"));
    req.caller_arn = Some(caller_arn(OTHER_RUN));
    let resp = research_api::handler::handle(&d, &req).await;
    assert_eq!(
        (resp.status, resp.log.code.as_deref()),
        (503, Some("disabled"))
    );

    // Switched off in the middle of the run: the next call is refused.
    let (status, _, _) = call(&d, route::SEARCH, q("first")).await;
    assert_eq!(status, 200);
    d.table.switch_off(RUN);
    let (status, body, _) = call(&d, route::SEARCH, q("second")).await;
    assert_eq!((status, body["code"].as_str()), (503, Some("disabled")));
    assert_eq!(
        d.provider.asked().len(),
        1,
        "the second call never reached the provider"
    );

    // An expired row refuses too.
    let d = deps(provider_with_two_hits());
    d.clock.advance_s(9 * 3600);
    let (status, body, _) = call(&d, route::SEARCH, q("late")).await;
    assert_eq!((status, body["code"].as_str()), (503, Some("disabled")));
}

#[tokio::test]
async fn request_bounds_are_checked_before_anything_is_spent() {
    let d = deps(fixture_provider());
    for (body, what) in [
        (json!({"query": ""}), "empty"),
        (json!({"query": "q".repeat(401)}), "401 characters"),
        (json!({"query": "x", "maxResults": 21}), "21 results"),
        (json!({"query": "x", "extra": 1}), "unknown member"),
        (json!({"query": 5}), "wrong type"),
        (
            json!({"query": "x", "idempotencyKey": "has space"}),
            "bad key",
        ),
    ] {
        let (status, v, _) = call(&d, route::SEARCH, body).await;
        assert_eq!((status, v["code"].as_str()), (400, Some("usage")), "{what}");
    }
    let mut req = request(route::SEARCH, &q("x"));
    req.body = "not json".into();
    assert_eq!(research_api::handler::handle(&d, &req).await.status, 400);
    assert_eq!(d.table.usage(RUN).await.unwrap().searches, 0);
    assert!(d.provider.asked().is_empty());
}

use research_api::ports::Table;

#[tokio::test]
async fn a_credential_shaped_query_is_refused_and_audited_without_the_text() {
    let d = deps(fixture_provider());
    let key = ["AK", "IA", "ABCDEFGHIJKLMNOP"].concat();
    let (status, v, _) = call(
        &d,
        route::SEARCH,
        q(&format!("how to use {key} with the sdk")),
    )
    .await;
    assert_eq!((status, v["code"].as_str()), (403, Some("refused_secret")));
    assert!(
        !v.to_string().contains(&key),
        "the answer does not echo the text"
    );
    assert!(d.provider.asked().is_empty());
    let a = audits(&d, RUN);
    assert_eq!(a.len(), 1);
    assert_eq!(
        (a[0]["status"].as_str(), a[0]["code"].as_str()),
        (Some("refused"), Some("refused_secret"))
    );
    assert!(
        !a[0].to_string().contains(&key),
        "the audit object does not hold the text"
    );
    // The caller's own session key id is refused too.
    let (status, v, _) = call(
        &d,
        route::SEARCH,
        q("look at ASIA-test-session-key-id please"),
    )
    .await;
    assert_eq!((status, v["code"].as_str()), (403, Some("refused_secret")));
}

#[tokio::test]
async fn per_run_counts_and_byte_budgets_stop_a_run_at_its_limit() {
    let d = deps(provider_with_two_hits().then_search(Ok(outcome(vec![]))));
    let mut p = policy(RUN);
    p.limits.searches = 2;
    d.table.put_policy(p);
    assert_eq!(call(&d, route::SEARCH, q("one")).await.0, 200);
    assert_eq!(call(&d, route::SEARCH, q("two")).await.0, 200);
    let (status, v, _) = call(&d, route::SEARCH, q("three")).await;
    assert_eq!((status, v["code"].as_str()), (429, Some("limit_calls")));
    assert_eq!(d.provider.asked().len(), 2);

    // The outbound text budget: 10 bytes in all.
    let d = deps(fixture_provider().then_search(Ok(outcome(vec![]))));
    let mut p = policy(RUN);
    p.limits.outbound_bytes = 10;
    d.table.put_policy(p);
    assert_eq!(call(&d, route::SEARCH, q("12345678")).await.0, 200);
    let (status, v, _) = call(&d, route::SEARCH, q("abc")).await;
    assert_eq!((status, v["code"].as_str()), (429, Some("limit_bytes")));
}

#[tokio::test]
async fn calls_per_minute_are_limited_per_run_and_the_limit_lifts_with_the_minute() {
    let mut script = fixture_provider();
    for _ in 0..4 {
        script = script.then_search(Ok(outcome(vec![])));
    }
    let d = deps(script);
    let mut p = policy(RUN);
    p.limits.calls_per_minute = 3;
    d.table.put_policy(p);
    for i in 0..3 {
        assert_eq!(call(&d, route::SEARCH, q(&format!("q{i}"))).await.0, 200);
    }
    let (status, v, _) = call(&d, route::SEARCH, q("q3")).await;
    assert_eq!((status, v["code"].as_str()), (429, Some("limit_rate")));
    assert!(v["retryAfterS"].as_u64().unwrap() >= 1);
    d.clock.advance_s(61);
    assert_eq!(call(&d, route::SEARCH, q("q4")).await.0, 200);
}

#[tokio::test]
async fn the_stage_wide_rate_of_the_key_is_shared_by_every_run() {
    let tavily = ScriptedProvider::new(ResearchProvider::Tavily)
        .then_search(Ok(outcome(vec![])))
        .then_search(Ok(outcome(vec![])));
    let mut d = deps(tavily);
    d.cfg.stage_rate_per_min = 1;
    d.table.put_policy(policy(OTHER_RUN));
    assert_eq!(call(&d, route::SEARCH, q("a")).await.0, 200);
    let mut req = request(route::SEARCH, &q("b"));
    req.caller_arn = Some(caller_arn(OTHER_RUN));
    let resp = research_api::handler::handle(&d, &req).await;
    assert_eq!(resp.status, 429);
    assert_eq!(resp.log.code.as_deref(), Some("limit_rate"));
}

#[tokio::test]
async fn provider_failures_are_answered_with_their_words_and_still_audited() {
    let d = deps(
        fixture_provider()
            .then_search(Err(Fail::new(
                ResearchErrorCode::NoKey,
                "no Tavily key is stored for this stage",
            )))
            .then_search(Err(Fail::new(
                ResearchErrorCode::UpstreamRateLimited,
                "Tavily said slow down",
            )
            .with_retry_after(12))),
    );
    let (status, v, _) = call(&d, route::SEARCH, q("a")).await;
    assert_eq!((status, v["code"].as_str()), (503, Some("no_key")));
    let (status, v, _) = call(&d, route::SEARCH, q("b")).await;
    assert_eq!(
        (status, v["code"].as_str(), v["retryAfterS"].as_u64()),
        (429, Some("upstream_rate_limited"), Some(12))
    );
    let a = audits(&d, RUN);
    assert_eq!(a.len(), 2);
    assert!(a.iter().all(|x| x["status"] == "error"));
}

#[tokio::test]
async fn a_repeated_idempotency_key_replays_the_first_answer_and_a_changed_request_conflicts() {
    let d = deps(provider_with_two_hits());
    let body = json!({"query": "same", "idempotencyKey": "k-1"});
    let (s1, v1, _) = call(&d, route::SEARCH, body.clone()).await;
    let (s2, v2, _) = call(&d, route::SEARCH, body).await;
    assert_eq!((s1, s2), (200, 200));
    assert_eq!(v1["id"], v2["id"], "the same answer, not a second search");
    assert_eq!(d.provider.asked().len(), 1);
    assert_eq!(d.table.usage(RUN).await.unwrap().searches, 1);
    let (s3, v3, _) = call(
        &d,
        route::SEARCH,
        json!({"query": "different", "idempotencyKey": "k-1"}),
    )
    .await;
    assert_eq!((s3, v3["code"].as_str()), (409, Some("conflict")));
}

#[tokio::test]
async fn an_unknown_path_or_method_is_not_served() {
    let d = deps(fixture_provider());
    let (status, _, _) = call(&d, "/internal/research/other", q("x")).await;
    assert_eq!(status, 404);
    let mut req = request(route::SEARCH, &q("x"));
    req.method = "GET".into();
    assert_eq!(research_api::handler::handle(&d, &req).await.status, 405);
}

#[tokio::test]
async fn a_body_that_does_not_parse_is_answered_with_its_category_and_never_with_what_it_held() {
    let d = deps(fixture_provider());
    // An unknown member, a value of the wrong type and a body that is not JSON: none of the texts comes back.
    for (body, shown) in [
        (
            r#"{"query": "x", "member-the-route-does-not-know": 1}"#.to_string(),
            "member-the-route-does-not-know",
        ),
        (
            r#"{"query": ["a list with a distinctive phrase"]}"#.to_string(),
            "distinctive phrase",
        ),
        (
            "this body is plain text with a distinctive phrase".to_string(),
            "distinctive phrase",
        ),
    ] {
        let mut req = request(route::SEARCH, &q("x"));
        req.body = body;
        let resp = research_api::handler::handle(&d, &req).await;
        assert_eq!(resp.status, 400);
        assert!(!resp.body.contains(shown), "{}", resp.body);
        let v: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .starts_with("the request body "),
            "{v}"
        );
    }
}

#[tokio::test]
async fn a_search_record_key_that_already_holds_other_bytes_is_a_fault_not_a_success() {
    let d = deps(fixture_provider().then_search(Ok(outcome(vec![]))));
    d.blobs.force(
        &format!("runs/{RUN}/agents/direct/research/searches/s-0001.json"),
        b"other".to_vec(),
    );
    let (status, v, _) = call(&d, route::SEARCH, q("one")).await;
    assert_eq!((status, v["code"].as_str()), (500, Some("internal")));
}

#[tokio::test]
async fn provider_metadata_is_bounded_before_it_is_shown_or_stored() {
    let mut hit = result("t", "https://example.org/a", "s");
    hit.published = Some("2026-10-16".repeat(30));
    let mut found = outcome(vec![hit]);
    found.upstream_request_id = Some("r".repeat(500));
    let d = deps(fixture_provider().then_search(Ok(found)));
    let (status, v, _) = call(&d, route::SEARCH, q("one")).await;
    assert_eq!(status, 200);
    assert_eq!(
        v["results"][0]["published"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        64
    );
    assert_eq!(
        audits(&d, RUN)[0]["upstreamRequestId"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        128
    );
}
