//! follow-up (limits audit row D26): one fetch call may name twenty addresses, the most Tavily takes in one extract request. The route
//! checks the count before any address is read, so the twenty-first address is a usage error and nothing is charged.

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::research::route;
use serde_json::json;
use support::*;

fn address(i: usize) -> String {
    format!("https://docs.example.org/guide/page-{i}")
}

/// A run that searched once and saw `n` addresses.
async fn searched(n: usize) -> TestDeps {
    let d = deps(
        fixture_provider().then_search(Ok(outcome(
            (0..n)
                .map(|i| result(&format!("page {i}"), &address(i), "a page"))
                .collect(),
        ))),
    );
    let (status, v, _) = call(
        &d,
        route::SEARCH,
        json!({"query": "the guide", "maxResults": 20}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["results"].as_array().unwrap().len(), n);
    d
}

#[tokio::test]
async fn b776w_a_fetch_of_twenty_seen_addresses_stores_twenty_pages() {
    let d = searched(20).await;
    let urls: Vec<String> = (0..20).map(address).collect();
    d.provider.push_extract(Ok(extracted(
        urls.iter()
            .map(|u| page(u, &format!("the text of {u}")))
            .collect(),
    )));
    let (status, v, _) = call(&d, route::FETCH, json!({"urls": urls})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 20, "{v}");
    // `failed` is left out of the answer when nothing failed
    assert!(v["failed"].as_array().is_none_or(Vec::is_empty), "{v}");
    assert_eq!(v["usage"]["fetchedUrls"], 20);
    // the provider got all twenty in one request
    let extracts: Vec<String> = d
        .provider
        .asked()
        .into_iter()
        .filter(|c| c.starts_with("extract:"))
        .collect();
    assert_eq!(extracts.len(), 1, "{extracts:?}");
    assert_eq!(
        extracts[0]
            .trim_start_matches("extract:")
            .split(',')
            .count(),
        20
    );
}

#[tokio::test]
async fn b776w_a_fetch_of_twenty_one_addresses_is_a_usage_error_and_charges_nothing() {
    let d = searched(20).await;
    let urls: Vec<String> = (0..21).map(address).collect();
    let (status, v, _) = call(&d, route::FETCH, json!({"urls": urls})).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("usage")), "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("urls is 1 to 20 addresses"),
        "{v}"
    );
    // nothing was asked of the provider and the run's count of fetched addresses did not move
    assert!(
        d.provider
            .asked()
            .iter()
            .all(|c| !c.starts_with("extract:"))
    );
    let (_, limits, _) = call(&d, route::LIMITS, json!({})).await;
    assert_eq!(limits["usage"]["fetchedUrls"], 0, "{limits}");
}
