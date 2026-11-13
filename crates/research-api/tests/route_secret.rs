//! The research route refuses a request that carries a credential-shaped value on every route and in every field Cline chooses:
//! the query and the domains of a search, the addresses and the query of a fetch, the address, the
//! ref and the paths of a repository start. The cases run through the real handler over the in-memory table and bucket. Each field has a
//! control that sends the same request without the value and sees it accepted, so the refusal is what stops the first one. The answer never
//! holds the text, the one audit object says `refused_secret` without it, nothing goes upstream, no job starts and the run's counters stay
//! where they were. The stand-in values are built at run time from pieces: no line of this file is a credential.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::research::route;
use research_api::ports::Table;
use serde_json::{Value, json};
use support::*;

const NPM: &str = "https://www.npmjs.com/package/is-plain-obj";
/// The access key id of every request `support::request` builds: the caller's own session key id.
const OWN_KEY: &str = "ASIA-test-session-key-id";

fn aws_key() -> String {
    ["AK", "IA", "ABCDEFGHIJKLMNOP"].concat()
}

fn github_token() -> String {
    format!("gh{}{}", "p_", "a1B2c3D4e5F6g7H8i9J0k1L2")
}

fn jwt() -> String {
    format!(
        "{}{}{}{}",
        "eyJ", "hbGciOiJub25lIn0", ".eyJzdWIiOiJ4In0", ".c2lnbmF0dXJl"
    )
}

fn slack_host() -> String {
    format!("xo{}{}.example.org", "xb-", "1234567890-abcdefghij")
}

fn pem_marker() -> String {
    ["-----BEGIN ", "RSA PRIVATE KEY-----"].concat()
}

async fn usage_of(d: &TestDeps) -> Value {
    serde_json::to_value(d.table.usage(RUN).await.unwrap()).unwrap()
}

fn refused_audits(d: &TestDeps) -> usize {
    audits(d, RUN)
        .iter()
        .filter(|a| a["code"] == "refused_secret")
        .count()
}

/// One refused call: 403 `refused_secret`, no echo of `text`, exactly one more audit object that says so without the text, and nothing
/// charged to the run.
async fn refused(d: &TestDeps, path: &str, body: Value, text: &str) {
    let (usage_before, audits_before) = (usage_of(d).await, refused_audits(d));
    let (status, v, _) = call(d, path, body).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (403, Some("refused_secret")),
        "{path} must refuse {text:?}: {v}"
    );
    assert!(
        !v.to_string().contains(text),
        "the answer echoes the text: {v}"
    );
    assert_eq!(
        refused_audits(d),
        audits_before + 1,
        "{path}: one audit object"
    );
    for a in audits(d, RUN) {
        assert!(
            !a.to_string().contains(text),
            "an audit object holds the text: {a}"
        );
    }
    assert_eq!(
        usage_of(d).await,
        usage_before,
        "{path}: a refusal charges nothing"
    );
}

#[tokio::test]
async fn a_credential_in_the_domains_of_a_search_is_refused() {
    let d = deps(fixture_provider().then_search(Ok(outcome(vec![]))));
    // The control: the same request with an ordinary domain is accepted.
    let ok = json!({"query": "is-plain-obj", "includeDomains": ["example.org"]});
    assert_eq!(call(&d, route::SEARCH, ok).await.0, 200);
    let asked = d.provider.asked().len();
    let host = slack_host();
    refused(
        &d,
        route::SEARCH,
        json!({"query": "is-plain-obj", "includeDomains": [host]}),
        &host,
    )
    .await;
    assert_eq!(d.provider.asked().len(), asked, "nothing went upstream");
}

/// A run that has searched, so every URL the results name is seen: the provenance gate cannot be what refuses a fetch of one of them.
async fn searched_with(urls: &[String]) -> TestDeps {
    let mut hits = vec![result("pkg", NPM, "a package")];
    hits.extend(urls.iter().map(|u| result("odd", u, "an address")));
    let d = deps(fixture_provider().then_search(Ok(outcome(hits))));
    assert_eq!(call(&d, route::SEARCH, q("is-plain-obj")).await.0, 200);
    d
}

fn extracts(d: &TestDeps) -> usize {
    d.provider
        .asked()
        .iter()
        .filter(|a| a.starts_with("extract:"))
        .count()
}

#[tokio::test]
async fn a_credential_in_the_address_of_a_fetch_is_refused_even_when_the_run_has_seen_that_address()
{
    let token = github_token();
    // A key id with its first letter written as %41, which the scan decodes once.
    let encoded = format!("%41{}", &aws_key()[1..]);
    let in_path = format!("https://example.org/files/{token}/readme");
    let in_query = format!("https://example.org/doc?id={encoded}");
    let d = searched_with(&[in_path.clone(), in_query.clone()]).await;

    // The control: an ordinary seen address is fetched.
    d.provider
        .push_extract(Ok(extracted(vec![page(NPM, "a plain page")])));
    assert_eq!(call(&d, route::FETCH, json!({"urls": [NPM]})).await.0, 200);
    let fetched = extracts(&d);

    refused(&d, route::FETCH, json!({"urls": [in_path]}), &token).await;
    refused(&d, route::FETCH, json!({"urls": [in_query]}), &encoded).await;
    // The credential-shaped address among ordinary ones refuses the whole call.
    refused(
        &d,
        route::FETCH,
        json!({"urls": [NPM, format!("https://example.org/files/{token}/readme")]}),
        &token,
    )
    .await;
    assert_eq!(extracts(&d), fetched, "nothing went upstream");
}

#[tokio::test]
async fn a_credential_in_the_query_of_a_fetch_is_refused() {
    let d = searched_with(&[]).await;
    d.provider
        .push_extract(Ok(extracted(vec![page(NPM, "a plain page")])));
    // The control: an ordinary query is accepted.
    let ok = json!({"urls": [NPM], "query": "what counts as plain"});
    assert_eq!(call(&d, route::FETCH, ok).await.0, 200);
    let fetched = extracts(&d);

    for text in [pem_marker(), github_token(), jwt(), OWN_KEY.to_string()] {
        let body = json!({"urls": [NPM], "query": format!("explain {text} in the docs")});
        refused(&d, route::FETCH, body, &text).await;
    }
    assert_eq!(extracts(&d), fetched, "nothing went upstream");
}

#[tokio::test]
async fn a_credential_in_a_repository_request_is_refused_and_starts_no_job() {
    let d = deps(fixture_provider());
    // The control: an ordinary start is accepted and starts one job.
    let ok = json!({"url": "https://github.com/sindresorhus/is-plain-obj", "ref": "main",
        "paths": ["src"], "idempotencyKey": "ok-1"});
    assert_eq!(call(&d, route::REPO, ok).await.0, 202);
    assert_eq!((d.table.jobs(RUN).len(), d.invoker.events().len()), (1, 1));

    let url_token = github_token();
    let cases = [
        // The owner's or the repository's name.
        (
            json!({"url": format!("https://github.com/acme/{url_token}"), "idempotencyKey": "r-1"}),
            url_token.clone(),
        ),
        // The ref.
        (
            json!({"url": "https://github.com/acme/widgets", "ref": aws_key(), "idempotencyKey": "r-2"}),
            aws_key(),
        ),
        // A path.
        (
            json!({"url": "https://github.com/acme/widgets", "paths": [format!("docs/{}", jwt())], "idempotencyKey": "r-3"}),
            jwt(),
        ),
        // The caller's own session key id.
        (
            json!({"url": "https://github.com/acme/widgets", "ref": OWN_KEY, "idempotencyKey": "r-4"}),
            OWN_KEY.to_string(),
        ),
    ];
    for (body, text) in cases {
        refused(&d, route::REPO, body, &text).await;
    }
    assert_eq!(
        (d.table.jobs(RUN).len(), d.invoker.events().len()),
        (1, 1),
        "no job was created or started for a refused request"
    );
}

#[tokio::test]
async fn every_refusal_leaves_the_run_with_the_counts_it_had() {
    let d = deps(fixture_provider());
    let before = usage_of(&d).await;
    refused(
        &d,
        route::SEARCH,
        q(&format!("use {}", aws_key())),
        &aws_key(),
    )
    .await;
    refused(
        &d,
        route::FETCH,
        json!({"urls": [NPM], "query": format!("use {}", github_token())}),
        &github_token(),
    )
    .await;
    refused(
        &d,
        route::REPO,
        json!({"url": "https://github.com/acme/widgets", "ref": aws_key(), "idempotencyKey": "r-9"}),
        &aws_key(),
    )
    .await;
    assert_eq!(usage_of(&d).await, before);
    // The limits answer shows the same zeros to the caller.
    let (status, v, _) = call(&d, route::LIMITS, json!({})).await;
    assert_eq!(status, 200, "{v}");
    for member in [
        "searches",
        "fetchedUrls",
        "repos",
        "outboundBytes",
        "contextBytes",
    ] {
        assert_eq!(v["usage"][member], 0, "{member}: {v}");
    }
    assert!(d.provider.asked().is_empty());
    assert!(d.table.jobs(RUN).is_empty());
}
