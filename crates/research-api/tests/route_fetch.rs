//! `POST /internal/research/fetch`: the provenance gate, the URL rules, the stored page and its manifest, the budgets. Ordinary input only.

mod support;

use harness_protocol::research::{ItemManifest, ResearchErrorCode, route};
use research_api::ops::frame_page;
use research_api::ports::{ExtractFailure, ExtractOutcome, Fail, Table};
use serde_json::json;
use support::*;

const NPM: &str = "https://www.npmjs.com/package/is-plain-obj";
const REPO: &str = "https://github.com/sindresorhus/is-plain-obj";

/// A run that has searched once, so NPM and REPO are seen.
async fn searched() -> TestDeps {
    let d = deps(fixture_provider().then_search(Ok(outcome(vec![
        result("pkg", NPM, "a package"),
        result("src", REPO, "the source"),
    ]))));
    assert_eq!(call(&d, route::SEARCH, q("is-plain-obj")).await.0, 200);
    d
}

#[tokio::test]
async fn a_seen_url_is_fetched_stored_and_described_by_a_manifest() {
    let d = searched().await;
    let text = format!("Check if a value is a plain object.\nSource: {REPO}\n\u{1b}[0m done");
    d.provider
        .push_extract(Ok(extracted(vec![page(NPM, &text)])));
    let (status, v, resp) = call(
        &d,
        route::FETCH,
        json!({"urls": [NPM], "query": "what counts as plain"}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let item = &v["items"][0];
    assert_eq!(item["id"], "i-0001");
    assert_eq!(item["kind"], "page");
    assert_eq!(item["origin"], NPM);
    assert_eq!(v["requestId"], resp.log.request_id.as_str());
    assert_eq!(v["usage"]["fetchedUrls"], 1);

    // Body first, manifest last: the manifest names the body's hash, and its own hash is the item's index hash.
    let order = d.blobs.order();
    let body_pos = order
        .iter()
        .position(|k| k.ends_with("items/i-0001/body.txt"))
        .unwrap();
    let man_pos = order
        .iter()
        .position(|k| k.ends_with("items/i-0001/manifest.json"))
        .unwrap();
    assert!(
        body_pos < man_pos,
        "the manifest is written last: {order:?}"
    );
    let manifest_bytes = d
        .blobs
        .bytes(item["manifestKey"].as_str().unwrap())
        .unwrap();
    let manifest: ItemManifest = serde_json::from_slice(&manifest_bytes).unwrap();
    manifest.validate().unwrap();
    assert_eq!(manifest.sha256(), item["manifestSha256"].as_str().unwrap());
    assert_eq!(
        manifest_bytes,
        manifest.canonical_bytes(),
        "stored in canonical form"
    );
    let body = d.blobs.bytes(item["bodyKey"].as_str().unwrap()).unwrap();
    assert_eq!(manifest.body.sha256, item["bodySha256"].as_str().unwrap());
    // The stored text is the one cleaning pass of the provider's text; the manifest keeps the hash of what the provider returned too.
    let stored = String::from_utf8(body).unwrap();
    assert!(!stored.contains('\u{1b}') && stored.contains("plain object"));
    assert_ne!(
        manifest.source_sha256.as_deref(),
        Some(manifest.body.sha256.as_str())
    );
    assert_eq!(manifest.origin, NPM);
    assert_eq!(manifest.licence.spdx, "NOASSERTION");

    // The audit object says every URL had been seen, and the page's context bytes are counted.
    let a = audits(&d, RUN);
    let fetch = a.iter().find(|x| x["op"] == "fetch").unwrap();
    assert_eq!(fetch["seenBefore"], true);
    assert_eq!(fetch["items"], json!(["i-0001"]));
    assert_eq!(fetch["input"]["urls"], json!([NPM]));
    assert_eq!(
        d.table.usage(RUN).await.unwrap().context_bytes,
        stored.len() as u64
    );
}

#[tokio::test]
async fn an_unseen_url_or_one_with_an_extra_parameter_is_refused_before_the_provider_is_asked() {
    let d = searched().await;
    for url in [
        "https://example.org/never-seen",
        &format!("{NPM}?utm=1"),
        "https://www.npmjs.com/package/other",
    ] {
        let (status, v, _) = call(&d, route::FETCH, json!({"urls": [url]})).await;
        assert_eq!(
            (status, v["code"].as_str()),
            (403, Some("refused_unseen_url")),
            "{url}"
        );
    }
    assert!(d.provider.asked().iter().all(|a| a.starts_with("search:")));
    assert_eq!(
        d.table.usage(RUN).await.unwrap().fetched_urls,
        0,
        "a refusal spends nothing"
    );
    // A URL that differs only in the case of the host or by a fragment is the same URL.
    d.provider
        .push_extract(Ok(extracted(vec![page(NPM, "text")])));
    let (status, _, _) = call(
        &d,
        route::FETCH,
        json!({"urls": ["https://WWW.NPMJS.COM/package/is-plain-obj#readme"]}),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_url_the_prompt_named_counts_as_seen_and_a_link_of_a_fetched_page_does_too() {
    let docs = "https://docs.rs/serde/latest/serde/";
    let d = deps(fixture_provider().then_extract(Ok(extracted(vec![page(
        docs,
        &format!("see {REPO} for the source"),
    )]))));
    d.table
        .put_policy(policy(RUN).with_prompt_urls(vec![docs.into()]));
    let (status, _, _) = call(&d, route::FETCH, json!({"urls": [docs]})).await;
    assert_eq!(status, 200);
    // The repository linked from that page can now be fetched as a page.
    d.provider
        .push_extract(Ok(extracted(vec![page(REPO, "readme")])));
    let (status, _, _) = call(&d, route::FETCH, json!({"urls": [REPO]})).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn url_rules_refuse_the_unacceptable_forms() {
    let d = searched().await;
    for url in [
        "http://www.npmjs.com/package/is-plain-obj",
        "https://user@www.npmjs.com/package/is-plain-obj",
        "https://203.0.113.9/package",
        "https://www.npmjs.com:8443/package/is-plain-obj",
        "ftp://example.org/x",
        "not a url",
    ] {
        let (status, v, _) = call(&d, route::FETCH, json!({"urls": [url]})).await;
        assert_eq!(
            (status, v["code"].as_str()),
            (403, Some("refused_url")),
            "{url}"
        );
    }
    let long = format!("https://example.org/{}", "a".repeat(2100));
    let (status, v, _) = call(&d, route::FETCH, json!({"urls": [long]})).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("usage")));
    let six: Vec<String> = (0..6).map(|i| format!("https://example.org/{i}")).collect();
    let (status, v, _) = call(&d, route::FETCH, json!({"urls": six})).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("usage")));
}

#[tokio::test]
async fn a_page_the_provider_could_not_extract_is_listed_and_the_others_are_stored() {
    let d = searched().await;
    d.provider.push_extract(Ok(ExtractOutcome {
        pages: vec![page(NPM, "ok")],
        failed: vec![ExtractFailure {
            url: REPO.into(),
            message: "could not extract".into(),
        }],
        credits: Some(1),
        upstream_request_id: None,
    }));
    let (status, v, _) = call(&d, route::FETCH, json!({"urls": [NPM, REPO]})).await;
    assert_eq!(status, 200);
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["failed"][0]["url"], REPO);
    assert_eq!(v["failed"][0]["code"], "upstream_error");
}

#[tokio::test]
async fn a_page_over_the_cap_is_cut_and_marked() {
    let d = searched().await;
    d.provider
        .push_extract(Ok(extracted(vec![page(NPM, &"x".repeat(600 * 1024))])));
    let (status, v, _) = call(&d, route::FETCH, json!({"urls": [NPM]})).await;
    assert_eq!(status, 200);
    assert_eq!(v["items"][0]["truncated"], true);
    // The cap cuts the page text at 512 KiB; the stored page is that text between the untrusted-text marker lines.
    let cut = "x".repeat(512 * 1024);
    let stored = frame_page(&"0".repeat(32), NPM, &cut).len() as u64;
    assert_eq!(v["items"][0]["bytes"], stored);
}

#[tokio::test]
async fn the_storage_budget_stops_a_page_before_anything_is_written() {
    let d = searched().await;
    let mut p = policy(RUN);
    // The page text is cut to the page cap of eight bytes; the budget counts the stored bytes, the text between the marker lines, and
    // it is exactly that many.
    let stored = frame_page(&"0".repeat(32), NPM, "01234567").len() as u64;
    p.limits.context_bytes = stored;
    p.limits.page_bytes = 8;
    p.limits.repo_unpacked_bytes = 8;
    d.table.put_policy(p);
    d.provider
        .push_extract(Ok(extracted(vec![page(NPM, "0123456789abcdef")])));
    let (status, v, _) = call(&d, route::FETCH, json!({"urls": [NPM]})).await;
    assert_eq!(
        (status, v["items"][0]["bytes"].as_u64()),
        (200, Some(stored))
    );
    // A second page finds the budget used up.
    d.provider
        .push_extract(Ok(extracted(vec![page(REPO, "more text")])));
    let before = d.blobs.keys().len();
    let (status, v, _) = call(&d, route::FETCH, json!({"urls": [REPO]})).await;
    assert_eq!((status, v["code"].as_str()), (429, Some("limit_bytes")));
    let stored_items = d
        .blobs
        .keys()
        .iter()
        .filter(|k| k.contains("/items/"))
        .count();
    assert_eq!(stored_items, 2, "body and manifest of the first page only");
    assert_eq!(
        d.blobs.keys().len(),
        before + 1,
        "only the audit object of the refused call"
    );
}

#[tokio::test]
async fn fetch_counts_every_url_against_the_run_and_a_provider_failure_maps_to_its_word() {
    let d = searched().await;
    d.provider.push_extract(Err(Fail::new(
        ResearchErrorCode::UpstreamError,
        "the provider answered 502",
    )));
    let (status, v, _) = call(&d, route::FETCH, json!({"urls": [NPM, REPO]})).await;
    assert_eq!((status, v["code"].as_str()), (502, Some("upstream_error")));
    assert_eq!(d.table.usage(RUN).await.unwrap().fetched_urls, 2);
    let a = audits(&d, RUN);
    assert_eq!(
        a.iter()
            .filter(|x| x["op"] == "fetch" && x["status"] == "error")
            .count(),
        1
    );
}

#[tokio::test]
async fn fetch_with_an_idempotency_key_is_answered_once_and_a_changed_request_conflicts() {
    let d = searched().await;
    d.provider
        .push_extract(Ok(extracted(vec![page(NPM, "the page text")])));
    let body = json!({"urls": [NPM], "idempotencyKey": "fetch-1"});
    let (s1, v1, _) = call(&d, route::FETCH, body.clone()).await;
    let (s2, v2, _) = call(&d, route::FETCH, body).await;
    assert_eq!((s1, s2), (200, 200));
    assert_eq!(
        v1["items"][0]["id"], v2["items"][0]["id"],
        "the same answer, not a second item"
    );
    assert_eq!(d.table.usage(RUN).await.unwrap().fetched_urls, 1);
    assert_eq!(
        d.provider
            .asked()
            .iter()
            .filter(|a| a.starts_with("extract:"))
            .count(),
        1
    );
    let (s3, v3, _) = call(
        &d,
        route::FETCH,
        json!({"urls": [NPM, REPO], "idempotencyKey": "fetch-1"}),
    )
    .await;
    assert_eq!((s3, v3["code"].as_str()), (409, Some("conflict")));
}
