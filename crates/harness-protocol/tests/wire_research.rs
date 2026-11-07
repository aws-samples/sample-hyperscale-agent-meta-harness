//! Protocol 1.38.0: the research route. Cline searches the web and downloads public repositories for context
//! through five private routes under `/internal/research/`, served by the `research-api` Lambda. These tests pin the request and
//! response bodies, the stored objects (the item manifest, the audit object, the policy row), the error codes with their HTTP status
//! and `ham-research` exit code, the limits, the S3 layout, the start-request member and the schema exports. Golden JSON goes in and
//! the same JSON comes out.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::INTERNAL_ROUTES;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::export::all_schemas;
use harness_protocol::research::{
    AuditObject, AuditStatus, DirectResearchCounts, DirectResearchNote, FetchRequest,
    FetchResponse, ItemKind, ItemManifest, JobRequest, JobResponse, JobState, LimitsRequest,
    LimitsResponse, PolicyItem, RepoRequest, ResearchError, ResearchErrorCode, ResearchLimits,
    ResearchOp, ResearchOptions, ResearchProvider, ResearchUsage, SearchDepth, SearchRequest,
    SearchResponse, s3,
};
use serde_json::{Value, json};

const RUN: &str = "01M4XAB0CDEFGHJKMNPQRSTVWX";
const SHA_A: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";
const SHA_B: &str = "1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e";

fn roundtrip<T>(golden: Value) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let parsed: T = serde_json::from_value(golden.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), golden);
    parsed
}

fn item_ref() -> Value {
    json!({
        "id": "i-0002", "kind": "repo",
        "origin": "https://github.com/sindresorhus/is-plain-obj",
        "manifestKey": format!("runs/{RUN}/agents/direct/research/items/i-0002/manifest.json"),
        "manifestSha256": SHA_A,
        "bodyKey": format!("runs/{RUN}/agents/direct/research/items/i-0002/body.tar.gz"),
        "bodySha256": SHA_B,
        "bytes": 3512, "licence": "MIT", "truncated": false, "files": 15, "dropped": 2
    })
}

// ---- version, routes, schemas --------------------------------------------------------------------------------------------

#[test]
fn the_protocol_is_at_its_research_version() {
    assert_eq!(PROTOCOL_VERSION, "1.56.0");
}

#[test]
fn the_five_research_routes_are_internal_post_routes_with_their_operation_ids() {
    let got: Vec<(&str, &str, &str)> = INTERNAL_ROUTES
        .iter()
        .filter(|r| r.path.starts_with("/internal/research/"))
        .map(|r| (r.method, r.path, r.operation_id))
        .collect();
    assert_eq!(
        got,
        [
            ("POST", "/internal/research/search", "researchSearch"),
            ("POST", "/internal/research/fetch", "researchFetch"),
            ("POST", "/internal/research/repo", "researchRepo"),
            ("POST", "/internal/research/job", "researchJob"),
            ("POST", "/internal/research/limits", "researchLimits"),
        ]
    );
    // The constants a client uses name the same paths.
    assert_eq!(
        harness_protocol::research::route::ALL,
        [
            "/internal/research/search",
            "/internal/research/fetch",
            "/internal/research/repo",
            "/internal/research/job",
            "/internal/research/limits"
        ]
    );
}

#[test]
fn the_internal_openapi_document_describes_every_research_route() {
    let all = all_schemas();
    let doc = &all
        .iter()
        .find(|(n, _)| *n == "openapi-internal.json")
        .unwrap()
        .1;
    for op in ["search", "fetch", "repo", "job", "limits"] {
        let path = format!("/internal/research/{op}");
        let post = &doc["paths"][path.as_str()]["post"];
        assert!(post["requestBody"].is_object(), "{path} has a request body");
        assert!(
            post["responses"]["200"]["content"]["application/json"]["schema"].is_object(),
            "{path} has a 200 schema"
        );
    }
}

#[test]
fn the_runtime_contract_carries_the_stored_research_objects() {
    let all = all_schemas();
    let runtime = &all
        .iter()
        .find(|(n, _)| *n == "runtime.schema.json")
        .unwrap()
        .1;
    let props = &runtime["properties"]["research"];
    assert!(props.is_object(), "RuntimeContract.research is exported");
    let text = runtime.to_string();
    for def in [
        "ItemManifest",
        "AuditObject",
        "PolicyItem",
        "ResearchLimits",
    ] {
        assert!(text.contains(def), "runtime.schema.json defines {def}");
    }
}

// ---- search --------------------------------------------------------------------------------------------------------------

#[test]
fn a_search_request_and_response_round_trip() {
    let req: SearchRequest = roundtrip(json!({
        "query": "is-plain-obj npm package what counts as a plain object",
        "maxResults": 5, "depth": "basic", "includeDomains": ["npmjs.com"], "timeRange": "year",
        "idempotencyKey": "k-0001"
    }));
    assert_eq!(req.depth, Some(SearchDepth::Basic));
    let minimal: SearchRequest = roundtrip(json!({"query": "rust tar pax headers"}));
    assert!(minimal.include_domains.is_empty() && minimal.max_results.is_none());

    let resp: SearchResponse = roundtrip(json!({
        "id": "s-0001", "provider": "tavily",
        "results": [{"title": "is-plain-obj", "url": "https://www.npmjs.com/package/is-plain-obj",
                      "snippet": "Check if a value is a plain object", "published": "2024-05-01", "score": 0.5}],
        "credits": 1, "requestId": "req-7",
        "usage": {"searches": 1, "fetchedUrls": 0, "repos": 0, "outboundBytes": 61, "contextBytes": 0}
    }));
    assert_eq!(resp.provider, ResearchProvider::Tavily);
}

#[test]
fn requests_refuse_members_they_do_not_define() {
    for (what, body) in [
        ("search", json!({"query": "x", "raw_content": true})),
        (
            "fetch",
            json!({"urls": ["https://example.org/"], "extractDepth": "advanced"}),
        ),
        (
            "repo",
            json!({"url": "https://github.com/a/b", "idempotencyKey": "k", "token": "x"}),
        ),
        ("job", json!({"jobId": "j-1", "wait": true})),
        ("limits", json!({"verbose": true})),
        ("options", json!({"enabled": true, "provider": "tavily"})),
    ] {
        let refused = match what {
            "search" => serde_json::from_value::<SearchRequest>(body).is_err(),
            "fetch" => serde_json::from_value::<FetchRequest>(body).is_err(),
            "repo" => serde_json::from_value::<RepoRequest>(body).is_err(),
            "job" => serde_json::from_value::<JobRequest>(body).is_err(),
            "limits" => serde_json::from_value::<LimitsRequest>(body).is_err(),
            _ => serde_json::from_value::<ResearchOptions>(body).is_err(),
        };
        assert!(refused, "{what} refuses an unknown member");
    }
}

#[test]
fn search_validation_enforces_the_caller_visible_bounds() {
    let limits = ResearchLimits::default();
    let ok = |q: &str| SearchRequest {
        query: q.into(),
        ..Default::default()
    };
    assert!(ok("a").validate(&limits).is_ok());
    assert!(ok(&"q".repeat(400)).validate(&limits).is_ok());
    // 401 characters; the count is characters, not bytes.
    let err = ok(&"q".repeat(401)).validate(&limits).unwrap_err();
    assert_eq!(err.code, ResearchErrorCode::Usage);
    assert!(ok(&"é".repeat(400)).validate(&limits).is_ok());
    assert!(ok("").validate(&limits).is_err());
    assert!(ok("   ").validate(&limits).is_err());
    let mut r = ok("x");
    r.max_results = Some(0);
    assert!(r.validate(&limits).is_err());
    r.max_results = Some(11);
    assert!(r.validate(&limits).is_err());
    r.max_results = Some(10);
    assert!(r.validate(&limits).is_ok());
    r.include_domains = (0..11).map(|i| format!("d{i}.example")).collect();
    assert!(r.validate(&limits).is_err());
    r.include_domains = vec!["not a host".into()];
    assert!(r.validate(&limits).is_err());
    r.include_domains = vec!["docs.rs".into(), "crates.io".into()];
    assert!(r.validate(&limits).is_ok());
    let mut k = ok("x");
    k.idempotency_key = Some("has space".into());
    assert!(k.validate(&limits).is_err());
    k.idempotency_key = Some("k".repeat(129));
    assert!(k.validate(&limits).is_err());
    k.idempotency_key = Some("k-1".into());
    assert!(k.validate(&limits).is_ok());
}

// ---- fetch ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_fetch_request_and_response_round_trip() {
    let _: FetchRequest = roundtrip(json!({
        "urls": ["https://www.npmjs.com/package/is-plain-obj"], "query": "what counts as plain",
        "idempotencyKey": "k-2"
    }));
    let resp: FetchResponse = roundtrip(json!({
        "items": [item_ref()],
        "failed": [{"url": "https://example.org/gone", "code": "not_found", "message": "Tavily could not extract the page"}],
        "requestId": "req-8",
        "usage": {"searches": 1, "fetchedUrls": 2, "repos": 0, "outboundBytes": 140, "contextBytes": 3512}
    }));
    assert_eq!(resp.failed[0].code, ResearchErrorCode::NotFound);
}

#[test]
fn fetch_validation_counts_urls_and_bytes() {
    let limits = ResearchLimits::default();
    let one = |u: &str| FetchRequest {
        urls: vec![u.into()],
        ..Default::default()
    };
    assert!(one("https://example.org/a").validate(&limits).is_ok());
    assert!(FetchRequest::default().validate(&limits).is_err(), "no URL");
    let six = FetchRequest {
        urls: (0..6).map(|i| format!("https://example.org/{i}")).collect(),
        ..Default::default()
    };
    assert!(six.validate(&limits).is_err());
    let five = FetchRequest {
        urls: (0..5).map(|i| format!("https://example.org/{i}")).collect(),
        ..Default::default()
    };
    assert!(five.validate(&limits).is_ok());
    let at = format!("https://example.org/{}", "a".repeat(2048 - 20));
    assert_eq!(at.len(), 2048);
    assert!(one(&at).validate(&limits).is_ok());
    assert!(one(&format!("{at}b")).validate(&limits).is_err());
    assert!(one("").validate(&limits).is_err());
}

// ---- repo and job ----------------------------------------------------------------------------------------------------------

#[test]
fn repo_requests_and_job_answers_round_trip() {
    let req: RepoRequest = roundtrip(json!({
        "url": "https://github.com/sindresorhus/is-plain-obj",
        "ref": "666df7c10035f7e26f27ec214fe5ae3173435f34",
        "paths": ["src/"], "idempotencyKey": "repo-0001"
    }));
    assert_eq!(
        req.git_ref.as_deref(),
        Some("666df7c10035f7e26f27ec214fe5ae3173435f34")
    );
    let pending: JobResponse = roundtrip(json!({
        "jobId": "j-0123456789abcdef", "state": "queued", "pollAfterMs": 2000,
        "usage": {"searches": 0, "fetchedUrls": 0, "repos": 1, "outboundBytes": 60, "contextBytes": 0}
    }));
    assert_eq!(pending.state, JobState::Queued);
    assert!(pending.item.is_none() && pending.error.is_none());
    let done: JobResponse =
        roundtrip(json!({"jobId": "j-0123456789abcdef", "state": "succeeded", "item": item_ref()}));
    assert_eq!(done.state, JobState::Succeeded);
    let failed: JobResponse = roundtrip(json!({
        "jobId": "j-0123456789abcdef", "state": "failed",
        "error": {"code": "timeout", "message": "the download did not finish in 16 minutes"}
    }));
    assert_eq!(failed.error.unwrap().code, ResearchErrorCode::Timeout);
    let _: JobRequest = roundtrip(json!({"jobId": "j-0123456789abcdef"}));
}

#[test]
fn repo_validation_requires_a_key_and_clean_refs_and_paths() {
    let limits = ResearchLimits::default();
    let base = || RepoRequest {
        url: "https://github.com/a/b".into(),
        idempotency_key: "k-1".into(),
        ..Default::default()
    };
    assert!(base().validate(&limits).is_ok());
    let mut r = base();
    r.idempotency_key = String::new();
    assert!(r.validate(&limits).is_err(), "the key is required");
    let bad_refs: Vec<String> = ["a b", "a;b", "a\nb", "-x", ""]
        .iter()
        .map(|s| s.to_string())
        .chain(["r".repeat(256)])
        .collect();
    for bad in &bad_refs {
        let mut r = base();
        r.git_ref = Some(bad.clone());
        assert!(r.validate(&limits).is_err(), "ref {bad:?}");
    }
    let good_refs: Vec<String> = ["main", "v1.2.3", "feature/x-y_z"]
        .iter()
        .map(|s| s.to_string())
        .chain(["a".repeat(40)])
        .collect();
    for good in &good_refs {
        let mut r = base();
        r.git_ref = Some(good.clone());
        assert!(r.validate(&limits).is_ok(), "ref {good:?}");
    }
    let bad_paths: Vec<String> = ["/outside", "../x", "a/../b", "a\\b", "a\0b", ""]
        .iter()
        .map(|s| s.to_string())
        .chain(["p".repeat(257)])
        .collect();
    for bad in &bad_paths {
        let mut r = base();
        r.paths = vec![bad.clone()];
        assert!(r.validate(&limits).is_err(), "path {bad:?}");
    }
    let mut r = base();
    r.paths = (0..21).map(|i| format!("p{i}/")).collect();
    assert!(r.validate(&limits).is_err(), "more than 20 paths");
    r.paths = vec!["src/".into(), "docs/readme.md".into()];
    assert!(r.validate(&limits).is_ok());
    assert!(
        JobRequest {
            job_id: "j 1".into()
        }
        .validate()
        .is_err(),
        "a job id is a token"
    );
    assert!(
        JobRequest {
            job_id: "j-0123456789abcdef".into()
        }
        .validate()
        .is_ok()
    );
}

// ---- limits and usage ----------------------------------------------------------------------------------------------------

#[test]
fn the_default_limits_are_the_numbers_of_the_decision() {
    let l = ResearchLimits::default();
    assert_eq!(
        serde_json::to_value(&l).unwrap(),
        json!({
            "searches": 200, "fetchedUrls": 200, "repos": 25, "callsPerMinute": 30,
            "queryChars": 400, "urlBytes": 2048, "outboundBytes": 65536,
            "pageBytes": 524288, "repoDownloadBytes": 134217728, "repoReadBytes": 1073741824,
            "repoReadEntries": 100000, "repoCompressedBytes": 67108864,
            "repoUnpackedBytes": 209715200, "repoFiles": 20000, "contextBytes": 314572800
        })
    );
    assert!(l.validate().is_ok());
    let mut zero = l.clone();
    zero.searches = 0;
    assert!(zero.validate().is_err(), "a limit of zero is not a limit");
    let mut big = l.clone();
    big.repo_unpacked_bytes = l.context_bytes + 1;
    assert!(
        big.validate().is_err(),
        "one repository may not exceed the run's context cap"
    );
}

#[test]
fn the_limits_response_pairs_use_with_the_caps() {
    let resp: LimitsResponse = roundtrip(json!({
        "enabled": true, "provider": "fixture",
        "limits": serde_json::to_value(ResearchLimits::default()).unwrap(),
        "usage": {"searches": 3, "fetchedUrls": 2, "repos": 1, "outboundBytes": 410, "contextBytes": 3512},
        "expiresAt": 1_792_000_000u64
    }));
    assert_eq!(
        resp.usage,
        ResearchUsage {
            searches: 3,
            fetched_urls: 2,
            repos: 1,
            outbound_bytes: 410,
            context_bytes: 3512
        }
    );
    let _: LimitsRequest = roundtrip(json!({}));
}

// ---- errors ----------------------------------------------------------------------------------------------------------------

#[test]
fn every_error_code_has_one_word_one_http_status_and_one_exit_code() {
    use ResearchErrorCode as C;
    let table: [(C, &str, u16, u8); 18] = [
        (C::Usage, "usage", 400, 2),
        (C::Conflict, "conflict", 409, 2),
        (C::Disabled, "disabled", 503, 3),
        (C::NoKey, "no_key", 503, 3),
        (C::RefusedSecret, "refused_secret", 403, 3),
        (C::RefusedUrl, "refused_url", 403, 3),
        (C::RefusedUnseenUrl, "refused_unseen_url", 403, 3),
        (C::RefusedHost, "refused_host", 403, 3),
        (C::LimitCalls, "limit_calls", 429, 4),
        (C::LimitBytes, "limit_bytes", 429, 4),
        (C::LimitRate, "limit_rate", 429, 4),
        (C::TooLarge, "too_large", 413, 4),
        (C::UpstreamError, "upstream_error", 502, 5),
        (C::UpstreamRateLimited, "upstream_rate_limited", 429, 5),
        (C::NotFound, "not_found", 404, 5),
        (C::NotStarted, "not_started", 502, 5),
        (C::Timeout, "timeout", 502, 5),
        (C::Internal, "internal", 500, 5),
    ];
    for (code, word, status, exit) in table {
        assert_eq!(code.as_str(), word);
        assert_eq!(code.http_status(), status, "{word}");
        assert_eq!(code.exit_code(), exit, "{word}");
        assert_eq!(serde_json::to_value(code).unwrap(), json!(word));
        assert_eq!(word.parse::<C>().unwrap(), code);
    }
    assert!("fine".parse::<C>().is_err());
    assert_eq!(C::ALL.len(), 18);
}

#[test]
fn an_error_body_is_the_api_error_shape_plus_a_retry_hint() {
    let e: ResearchError = roundtrip(json!({
        "code": "limit_rate", "message": "30 calls a minute", "retryAfterS": 12, "requestId": "req-9"
    }));
    assert_eq!(e.code, ResearchErrorCode::LimitRate);
    // The OpenAPI default response of every route is ApiError {code, message, requestId}: a ResearchError is a superset.
    let api: harness_protocol::api::ApiError =
        serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
    assert_eq!(api.code, "limit_rate");
    let plain = ResearchError::new(ResearchErrorCode::Disabled, "research is off for this run");
    assert_eq!(
        serde_json::to_value(&plain).unwrap(),
        json!({"code": "disabled", "message": "research is off for this run"})
    );
}

// ---- stored objects --------------------------------------------------------------------------------------------------------

fn manifest_json() -> Value {
    json!({
        "schema": "hamh-research-item/1", "itemId": "i-0002", "kind": "repo", "run": RUN, "agent": "direct",
        "provider": "github-archive", "requestId": "req-10",
        "origin": "https://github.com/sindresorhus/is-plain-obj",
        "commit": "666df7c10035f7e26f27ec214fe5ae3173435f34", "ref": "main",
        "retrievedAt": "2026-10-16T14:30:02Z",
        "licence": {"spdx": "MIT", "source": "file:license", "evidenceSha256": SHA_A},
        "body": {"key": format!("runs/{RUN}/agents/direct/research/items/i-0002/body.tar.gz"), "sha256": SHA_B, "bytes": 2101},
        "files": [{"path": "index.js", "sha256": SHA_A, "bytes": 312}, {"path": "license", "sha256": SHA_B, "bytes": 1109}],
        "dropped": [{"path": ".github/", "reason": "excluded-dir"}],
        "caps": {"maxFiles": 20000, "maxUnpackedBytes": 209715200},
        "truncated": false
    })
}

#[test]
fn an_item_manifest_round_trips_and_has_one_canonical_byte_form() {
    let m: ItemManifest = roundtrip(manifest_json());
    assert_eq!(m.kind, ItemKind::Repo);
    let bytes = m.canonical_bytes();
    // Compact JSON, keys sorted, no trailing newline: the sha256 of these bytes is the item's index hash.
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(
        text.starts_with(r#"{"agent":"direct","body":{"bytes":2101,"#),
        "{text}"
    );
    assert!(!text.contains(": ") && !text.ends_with('\n'));
    // Reading it back and writing it again gives the same bytes.
    let again: ItemManifest = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(again.canonical_bytes(), bytes);
    assert_eq!(m.sha256().len(), 64);
    assert_eq!(m.sha256(), again.sha256());
    // A change of one byte of any member changes the hash.
    let mut other = m.clone();
    other.origin.push('x');
    assert_ne!(other.sha256(), m.sha256());
}

#[test]
fn an_item_manifest_validates_its_shape() {
    let good: ItemManifest = serde_json::from_value(manifest_json()).unwrap();
    assert!(good.validate().is_ok());
    let mut bad = good.clone();
    bad.schema = "hamh-research-item/2".into();
    assert!(bad.validate().is_err(), "schema");
    let mut bad = good.clone();
    bad.item_id = "item-2".into();
    assert!(bad.validate().is_err(), "item id");
    let mut bad = good.clone();
    bad.body.sha256 = "ABC".into();
    assert!(bad.validate().is_err(), "sha256 is 64 lower-case hex");
    let mut bad = good.clone();
    bad.commit = Some("main".into());
    assert!(bad.validate().is_err(), "commit is 40 hex");
    let mut bad = good.clone();
    bad.body.key = "runs/other/agents/direct/research/items/i-0002/body.tar.gz".into();
    assert!(
        bad.validate().is_err(),
        "the body key is under this run's research prefix"
    );
    let mut bad = good.clone();
    bad.files[0].path = "../escape".into();
    assert!(bad.validate().is_err(), "file paths stay inside");
    let mut bad = good.clone();
    bad.agent = "other".into();
    assert!(bad.validate().is_err(), "agent is direct");
    let mut page = good.clone();
    page.kind = ItemKind::Page;
    page.provider = ResearchProvider::Tavily;
    page.commit = None;
    page.files.clear();
    page.dropped.clear();
    page.caps = None;
    page.source_sha256 = Some(SHA_A.into());
    assert!(page.validate().is_ok());
}

#[test]
fn an_audit_object_round_trips_and_never_has_a_header_or_key_member() {
    let a: AuditObject = roundtrip(json!({
        "schema": "hamh-research-audit/1", "run": RUN, "agent": "direct", "op": "fetch", "provider": "tavily",
        "at": "2026-10-16T14:29:40Z", "requestId": "req-11", "status": "ok", "ms": 1310,
        "input": {"urls": ["https://www.npmjs.com/package/is-plain-obj"], "query": "plain objects", "bytesOut": 214},
        "credits": 1, "items": ["i-0001"], "seenBefore": true
    }));
    assert_eq!(a.op, ResearchOp::Fetch);
    assert_eq!(a.status, AuditStatus::Ok);
    let refused: AuditObject = roundtrip(json!({
        "schema": "hamh-research-audit/1", "run": RUN, "agent": "direct", "op": "search", "provider": "fixture",
        "at": "2026-10-16T14:29:41Z", "requestId": "req-12", "status": "refused", "code": "refused_secret", "ms": 3,
        "input": {"bytesOut": 0}, "items": []
    }));
    assert_eq!(refused.code, Some(ResearchErrorCode::RefusedSecret));
    // No `headers`, `authorization` or `key` member exists on the type.
    let all = all_schemas();
    let runtime = &all
        .iter()
        .find(|(n, _)| *n == "runtime.schema.json")
        .unwrap()
        .1;
    let members: Vec<&str> = runtime["$defs"]["AuditObject"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for word in [
        "headers",
        "authorization",
        "apiKey",
        "key",
        "token",
        "secret",
    ] {
        assert!(
            !members.contains(&word),
            "AuditObject has no {word} member: {members:?}"
        );
    }
}

#[test]
fn a_policy_item_is_keyed_by_run_and_expires() {
    let item = PolicyItem::new(
        RUN,
        true,
        ResearchProvider::Fixture,
        ResearchLimits::default(),
        1_792_000_000,
    );
    let v = serde_json::to_value(&item).unwrap();
    assert_eq!(v["PK"], format!("run#{RUN}"));
    assert_eq!(v["SK"], "cfg");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["provider"], "fixture");
    assert_eq!(v["expiresAt"], 1_792_000_000u64);
    // DynamoDB deletes the item some time after this epoch second.
    assert!(v["ttl"].as_u64().unwrap() > 1_792_000_000);
    assert!(v.get("promptUrls").is_none(), "left out when empty");
    let back: PolicyItem = serde_json::from_value(v).unwrap();
    assert_eq!(back, item);
    assert!(item.validate().is_ok());
    // The URLs the user's prompt named count as seen for the provenance gate (at most 50, each within the URL limit).
    let with = item
        .clone()
        .with_prompt_urls(vec!["https://docs.rs/serde/latest/serde/".into()]);
    let v = serde_json::to_value(&with).unwrap();
    assert_eq!(v["promptUrls"][0], "https://docs.rs/serde/latest/serde/");
    assert!(with.validate().is_ok());
    let many = item.clone().with_prompt_urls(
        (0..51)
            .map(|i| format!("https://example.org/{i}"))
            .collect(),
    );
    assert!(many.validate().is_err(), "more than 50 prompt URLs");
    let long = item
        .clone()
        .with_prompt_urls(vec![format!("https://example.org/{}", "a".repeat(2100))]);
    assert!(long.validate().is_err(), "a prompt URL over the URL limit");
    let mut bad = item.clone();
    bad.pk = "run#nope".into();
    assert!(bad.validate().is_err(), "the run id is a ULID");
}

// ---- S3 layout -------------------------------------------------------------------------------------------------------------

#[test]
fn the_research_keys_sit_under_one_prefix_of_the_direct_agent() {
    assert_eq!(
        s3::prefix(RUN),
        format!("runs/{RUN}/agents/direct/research/")
    );
    assert_eq!(
        s3::manifest_key(RUN, "i-0002"),
        format!("runs/{RUN}/agents/direct/research/items/i-0002/manifest.json")
    );
    assert_eq!(
        s3::body_key(RUN, "i-0002", ItemKind::Repo),
        format!("runs/{RUN}/agents/direct/research/items/i-0002/body.tar.gz")
    );
    assert_eq!(
        s3::body_key(RUN, "i-0001", ItemKind::Page),
        format!("runs/{RUN}/agents/direct/research/items/i-0001/body.txt")
    );
    assert_eq!(
        s3::audit_key(RUN, "20261016T142940Z", "req-11"),
        format!("runs/{RUN}/agents/direct/research/audit/20261016T142940Z-req-11.json")
    );
    assert_eq!(
        s3::search_key(RUN, "s-0001"),
        format!("runs/{RUN}/agents/direct/research/searches/s-0001.json")
    );
    assert!(s3::is_item_id("i-0002") && s3::is_item_id("i-12345"));
    assert!(!s3::is_item_id("i-2") && !s3::is_item_id("x-0002") && !s3::is_item_id("i-00a2"));
    assert!(s3::is_search_id("s-0001") && !s3::is_search_id("s-1"));
    // The worker role may not write here: every key starts with the prefix the bucket policy denies.
    for k in [
        s3::manifest_key(RUN, "i-0002"),
        s3::body_key(RUN, "i-0002", ItemKind::Repo),
        s3::audit_key(RUN, "20261016T142940Z", "r"),
        s3::search_key(RUN, "s-0001"),
    ] {
        assert!(k.starts_with(&s3::prefix(RUN)), "{k}");
    }
}

// ---- start request and summary note ------------------------------------------------------------------------------------------

#[test]
fn the_start_request_carries_an_optional_research_switch() {
    let base = json!({
        "prompt": "add a function", "target": {"mode": "s3-only"}, "model": "us.openai.gpt-6.1-sol", "rules": []
    });
    // Absent: the member is left out, so a request written before 1.38.0 keeps its bytes.
    let plain: StartDirectRunRequest = serde_json::from_value(base.clone()).unwrap();
    assert!(plain.research.is_none());
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("research")
            .is_none()
    );
    let mut off = base.clone();
    off["research"] = json!({"enabled": false});
    let req: StartDirectRunRequest = serde_json::from_value(off.clone()).unwrap();
    assert_eq!(req.research, Some(ResearchOptions { enabled: false }));
    assert_eq!(serde_json::to_value(&req).unwrap(), off);
}

#[test]
fn the_summary_note_names_provider_and_counters() {
    let n: DirectResearchNote = roundtrip(json!({
        "enabled": true, "provider": "tavily",
        "usage": {"searches": 4, "fetchedUrls": 3, "repos": 1, "outboundBytes": 800, "contextBytes": 52_000}
    }));
    assert!(n.enabled);
    let off: DirectResearchNote =
        roundtrip(json!({"enabled": false, "usage": ResearchUsage::default()}));
    assert!(off.provider.is_none());
}

/// `fixtures/research/summary-research.json`: the `research` member of `summary.json` for a run that searched twice, fetched twice (the route stored one
/// page that agentd could not place; one address was refused) and asked for one repository (the upstream failed). The route counted the page (`usage`), agentd's
/// own counts do not: the two are different quantities. The one set of bytes agentd's writer
/// (`crates/agentd/tests/direct_research_hook.rs`) and every reader (`run-session`, `ham-core`) are tested against.
const SUMMARY_RESEARCH: &str = include_str!("../../../fixtures/research/summary-research.json");

#[test]
fn the_research_member_of_the_summary_writes_back_the_golden_bytes() {
    let golden: Value = serde_json::from_str(SUMMARY_RESEARCH).unwrap();
    let note: DirectResearchNote = roundtrip(golden["research"].clone());
    assert!(note.enabled);
    assert_eq!(note.provider, Some(ResearchProvider::Fixture));
    assert_eq!(note.usage.unwrap().context_bytes, 1380);
    let c = note.counts.unwrap();
    assert_eq!((c.searches, c.fetches, c.repos, c.waits), (2, 2, 1, 0));
    assert_eq!(
        (c.placed, c.refused, c.errors, c.placed_bytes),
        (0, 1, 2, 0)
    );
}

#[test]
fn a_run_that_placed_pages_and_repositories_writes_the_bytes_it_placed() {
    let note: DirectResearchNote = roundtrip(json!({
        "enabled": true, "provider": "tavily",
        "usage": {"searches": 4, "fetchedUrls": 3, "repos": 1, "outboundBytes": 800, "contextBytes": 52_000},
        "counts": {"searches": 4, "fetches": 3, "repos": 1, "waits": 2, "placed": 4, "refused": 0, "errors": 0, "placedBytes": 51_200}
    }));
    let c = note.counts.unwrap();
    assert_eq!((c.placed, c.placed_bytes, c.waits), (4, 51_200, 2));
}

#[test]
fn what_agentd_did_not_learn_is_left_out_not_written_as_zeros() {
    // A run whose final read of the route's usage failed: the counts are there, the usage is not (a zero would claim the route counted nothing).
    let counts = json!({"searches": 3, "fetches": 0, "repos": 0, "waits": 0, "placed": 0, "refused": 0, "errors": 0, "placedBytes": 0});
    let note: DirectResearchNote =
        roundtrip(json!({"enabled": true, "provider": "tavily", "counts": counts}));
    assert!(note.usage.is_none());
    assert_eq!(serde_json::to_value(&note).unwrap().get("usage"), None);
    // A run whose research was switched off carries the word and nothing else.
    let off: DirectResearchNote = roundtrip(json!({"enabled": false}));
    assert!(off.provider.is_none() && off.usage.is_none() && off.counts.is_none());
    // The note written before agentd counted (usage required, no counts) still reads.
    let old: DirectResearchNote =
        roundtrip(json!({"enabled": false, "usage": ResearchUsage::default()}));
    assert!(old.counts.is_none());
}

#[test]
fn the_counts_are_eight_numbers_with_the_names_of_the_cli() {
    let c = DirectResearchCounts::default();
    let v = serde_json::to_value(c).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "errors",
            "fetches",
            "placed",
            "placedBytes",
            "refused",
            "repos",
            "searches",
            "waits"
        ]
    );
    assert!(v.as_object().unwrap().values().all(|n| n == 0));
}

// ---- the agentcore provider ---------------------------------------------------------------

#[test]
fn agentcore_is_a_kebab_case_provider_word() {
    assert_eq!(
        serde_json::to_value(ResearchProvider::Agentcore).unwrap(),
        json!("agentcore")
    );
    let back: ResearchProvider = serde_json::from_value(json!("agentcore")).unwrap();
    assert_eq!(back, ResearchProvider::Agentcore);
    // The words that were there before keep their spelling.
    for (provider, word) in [
        (ResearchProvider::Tavily, "tavily"),
        (ResearchProvider::TavilyKeyless, "tavily-keyless"),
        (ResearchProvider::Fixture, "fixture"),
        (ResearchProvider::GithubArchive, "github-archive"),
        (ResearchProvider::User, "user"),
    ] {
        assert_eq!(serde_json::to_value(provider).unwrap(), json!(word));
    }
}

#[test]
fn agentcore_searches_but_does_not_fetch() {
    for provider in [
        ResearchProvider::Tavily,
        ResearchProvider::TavilyKeyless,
        ResearchProvider::Fixture,
    ] {
        assert!(provider.is_search_provider(), "{provider:?} searches");
        assert!(provider.supports_fetch(), "{provider:?} fetches");
    }
    assert!(ResearchProvider::Agentcore.is_search_provider());
    assert!(
        !ResearchProvider::Agentcore.supports_fetch(),
        "the Web Search Tool returns excerpts and cannot fetch a page"
    );
    for provider in [ResearchProvider::GithubArchive, ResearchProvider::User] {
        assert!(!provider.is_search_provider(), "{provider:?}");
        assert!(!provider.supports_fetch(), "{provider:?}");
    }
}

#[test]
fn a_policy_row_may_name_agentcore() {
    let item = PolicyItem::new(
        RUN,
        true,
        ResearchProvider::Agentcore,
        ResearchLimits::default(),
        1_792_000_000,
    );
    assert!(item.validate().is_ok());
    let v = serde_json::to_value(&item).unwrap();
    assert_eq!(v["provider"], "agentcore");
    let back: PolicyItem = serde_json::from_value(v).unwrap();
    assert_eq!(back, item);
}

#[test]
fn a_search_response_and_an_audit_object_may_name_agentcore() {
    let resp: SearchResponse = roundtrip(json!({
        "id": "s-0001", "provider": "agentcore",
        "results": [{"title": "serde", "url": "https://serde.rs/", "snippet": "", "published": "2025-01-02"}],
        "requestId": "req-1"
    }));
    assert_eq!(resp.provider, ResearchProvider::Agentcore);
    let audit: AuditObject = roundtrip(json!({
        "schema": "hamh-research-audit/1", "run": RUN, "agent": "direct", "op": "search", "provider": "agentcore",
        "at": "2026-10-25T12:00:00Z", "requestId": "req-2", "status": "ok", "ms": 480,
        "input": {"query": "serde derive", "bytesOut": 12}, "items": [], "upstreamRequestId": "31ff0adc"
    }));
    assert_eq!(audit.provider, ResearchProvider::Agentcore);
}

#[test]
fn a_page_manifest_cannot_name_agentcore() {
    // A fetched page is stored with the provider that fetched it. agentcore cannot fetch, so no page can carry its word.
    let mut page: ItemManifest = serde_json::from_value(manifest_json()).unwrap();
    page.kind = ItemKind::Page;
    page.provider = ResearchProvider::Tavily;
    page.commit = None;
    page.files.clear();
    page.dropped.clear();
    page.caps = None;
    page.source_sha256 = Some(SHA_A.into());
    assert!(page.validate().is_ok());
    page.provider = ResearchProvider::Agentcore;
    let err = page.validate().unwrap_err();
    assert!(
        err.message.contains("provider does not fit the kind"),
        "{err}"
    );
}

#[test]
fn the_exported_schemas_list_the_agentcore_word() {
    let all = all_schemas();
    for file in ["openapi-internal.json", "runtime.schema.json"] {
        let doc = &all.iter().find(|(n, _)| *n == file).unwrap().1;
        let text = doc.to_string();
        assert!(
            text.contains("\"agentcore\""),
            "{file} lists the agentcore provider word"
        );
    }
}
