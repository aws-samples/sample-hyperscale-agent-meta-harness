//! The loopback API of the research module. agentd listens on 127.0.0.1 only and offers search,
//! fetch, repository download, wait, index, limits, verify and check to whatever the engine runs. Bodies are the route's own request
//! types (`harness_protocol::research`); agentd adds the pieces that need the run's credentials and disk: it signs the call to the
//! route, retries a transport failure with the same idempotency key, reads what the route stored, checks it and places it read-only.
//! The route and the bucket are fakes here.
#![allow(clippy::unwrap_used)]

mod common;
mod research_support;

use agentd::direct::context::ContextDir;
use agentd::direct::research::api::{self, Server, Shared, Stats, is_loopback_peer};
use agentd::direct::research::client::Client;
use agentd::direct::research::extract::remove_tree;
use agentd::direct::research::place::Placer;
use agentd::direct::research::settings::Settings;
use agentd::logging::MemoryLog;
use common::scratch;
use harness_protocol::research::route as paths;
use harness_protocol::research::{
    FetchResponse, JobResponse, JobState, LimitsResponse, ResearchErrorCode, ResearchLimits,
    ResearchProvider, ResearchUsage, SearchResponse, SearchResult,
};
use research_support::*;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

struct Rig {
    data: std::path::PathBuf,
    work: std::path::PathBuf,
    route: Arc<FakeRoute>,
    store: Arc<FakeStore>,
    log: Arc<MemoryLog>,
    shared: Arc<Shared>,
    server: Option<Server>,
    addr: SocketAddr,
}

impl Rig {
    async fn start(settings: Settings) -> Self {
        let data = scratch("api");
        let work = scratch("api-work");
        let ctx = ContextDir::create(&data).unwrap();
        let route = Arc::new(FakeRoute::default());
        let store = Arc::new(FakeStore::default());
        let log = MemoryLog::new();
        let placer = Arc::new(Placer::open(&ctx, RUN, settings.clone()).unwrap());
        let shared = Shared::new(
            Client::new(route.clone(), &settings),
            store.clone(),
            placer,
            settings,
            work.clone(),
            log.clone(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = api::start(listener, shared.clone());
        let addr = server.addr();
        Self {
            data,
            work,
            route,
            store,
            log,
            shared,
            server: Some(server),
            addr,
        }
    }

    fn done(self) {
        remove_tree(&self.data);
        remove_tree(&self.work);
    }
}

fn quick() -> Settings {
    Settings {
        min_free_bytes: 0,
        retry_pause: Duration::from_millis(5),
        poll_every: Duration::from_millis(10),
        wait: Duration::from_secs(5),
        ..Settings::default()
    }
}

fn search_answer() -> SearchResponse {
    SearchResponse {
        id: "s-0001".into(),
        provider: ResearchProvider::Fixture,
        results: vec![SearchResult {
            title: "What counts as a plain object".into(),
            url: "https://example.org/plain-object".into(),
            snippet: "A plain object has Object.prototype as its prototype.".into(),
            published: None,
            score: Some(0.9),
        }],
        credits: Some(1),
        request_id: "req-s1".into(),
        usage: None,
    }
}

fn job(state: JobState, id: &str) -> JobResponse {
    JobResponse {
        job_id: id.into(),
        state,
        item: None,
        error: None,
        poll_after_ms: None,
        usage: None,
    }
}

fn no_text_of(rig: &Rig, markers: &[&str]) {
    for line in rig.log.lines() {
        for m in markers {
            assert!(!line.contains(m), "the log carries {m:?}: {line}");
        }
    }
}

// ---- basics -----------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_server_listens_on_the_loopback_address_only_and_answers_health() {
    let rig = Rig::start(quick()).await;
    assert!(rig.addr.ip().is_loopback(), "{}", rig.addr);
    assert!(rig.addr.ip().is_ipv4());
    let (status, body) = http(rig.addr, "GET", "/healthz", None).await;
    assert_eq!((status, body.trim()), (200, "ok"));
    rig.done();
}

#[test]
fn only_loopback_peers_are_served() {
    for ok in ["127.0.0.1:5000", "127.9.9.9:1", "[::1]:80"] {
        assert!(is_loopback_peer(&ok.parse().unwrap()), "{ok}");
    }
    for bad in [
        "10.0.0.5:5000",
        "192.168.1.9:1",
        "172.16.0.9:80",
        "[fe80::1]:80",
        "0.0.0.0:1",
    ] {
        assert!(!is_loopback_peer(&bad.parse().unwrap()), "{bad}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_path_or_method_is_a_json_error() {
    let rig = Rig::start(quick()).await;
    let (status, body) = get_json(rig.addr, "/v1/nothing").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "usage");
    let (status, body) = get_json(rig.addr, "/v1/search").await;
    assert_eq!(status, 405, "{body}");
    assert_eq!(body["error"]["code"], "usage");
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_server_refuses_connections() {
    let mut rig = Rig::start(quick()).await;
    let addr = rig.addr;
    rig.server.take().unwrap().stop().await;
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
    rig.done();
}

// ---- search -----------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_search_goes_to_the_route_with_a_key_and_comes_back_marked_as_third_party_text() {
    let rig = Rig::start(quick()).await;
    rig.route.push_ok(paths::SEARCH, &search_answer());
    let (status, body) = post_json(
        rig.addr,
        "/v1/search",
        &json!({"query": "is-plain-obj what counts as a plain object", "maxResults": 3}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["id"], "s-0001");
    assert_eq!(
        body["results"][0]["url"],
        "https://example.org/plain-object"
    );
    assert_eq!(body["untrusted"], true);
    assert!(body["notice"].as_str().unwrap().contains("third-party"));
    let sent = rig.route.calls_to(paths::SEARCH);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["query"],
        "is-plain-obj what counts as a plain object"
    );
    assert_eq!(sent[0]["maxResults"], 3);
    let key = sent[0]["idempotencyKey"].as_str().unwrap();
    assert!((20..=40).contains(&key.len()), "{key}");
    // The query and the snippet are not in the log.
    no_text_of(&rig, &["plain object", "Object.prototype", "is-plain-obj"]);
    assert!(
        rig.log
            .lines()
            .iter()
            .any(|l| l.contains("research search")),
        "{:?}",
        rig.log.lines()
    );
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_the_caller_sends_is_kept() {
    let rig = Rig::start(quick()).await;
    rig.route.push_ok(paths::SEARCH, &search_answer());
    post_json(
        rig.addr,
        "/v1/search",
        &json!({"query": "q", "idempotencyKey": "my-key-1"}),
    )
    .await;
    assert_eq!(
        rig.route.calls_to(paths::SEARCH)[0]["idempotencyKey"],
        "my-key-1"
    );
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_a_request_is_a_usage_error_and_the_route_is_not_asked() {
    let rig = Rig::start(quick()).await;
    for (what, body) in [
        ("unknown member", json!({"query": "q", "extra": 1})),
        ("no query", json!({"maxResults": 3})),
        ("empty query", json!({"query": "  "})),
        ("too many results", json!({"query": "q", "maxResults": 99})),
    ] {
        let (status, v) = post_json(rig.addr, "/v1/search", &body).await;
        assert_eq!(status, 400, "{what}: {v}");
        assert_eq!(v["error"]["code"], "usage", "{what}");
        assert_eq!(v["error"]["exit"], 2, "{what}");
    }
    let (status, text) = http(rig.addr, "POST", "/v1/search", Some("{not json")).await;
    assert_eq!(status, 400, "{text}");
    let (status, _) = http(rig.addr, "POST", "/v1/search", Some("")).await;
    assert_eq!(status, 400);
    assert!(rig.route.calls.lock().unwrap().is_empty());
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_over_64_kib_is_refused() {
    let rig = Rig::start(quick()).await;
    let big = format!("{{\"query\": \"{}\"}}", "x".repeat(70 * 1024));
    let (status, text) = http(rig.addr, "POST", "/v1/search", Some(&big)).await;
    assert_eq!(status, 413, "{text}");
    assert!(text.contains("too_large"));
    assert!(rig.route.calls.lock().unwrap().is_empty());
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_routes_refusals_keep_their_words_status_and_exit_codes() {
    let rig = Rig::start(quick()).await;
    for (code, status, exit) in [
        (ResearchErrorCode::RefusedSecret, 403, 3),
        (ResearchErrorCode::RefusedUnseenUrl, 403, 3),
        (ResearchErrorCode::Disabled, 503, 3),
        (ResearchErrorCode::NoKey, 503, 3),
        (ResearchErrorCode::LimitCalls, 429, 4),
        (ResearchErrorCode::TooLarge, 413, 4),
        (ResearchErrorCode::UpstreamError, 502, 5),
        (ResearchErrorCode::NotFound, 404, 5),
    ] {
        rig.route
            .push_fail(paths::SEARCH, code, "the route says no");
        let (got, v) = post_json(rig.addr, "/v1/search", &json!({"query": "q"})).await;
        assert_eq!(got, status, "{code}: {v}");
        assert_eq!(v["error"]["code"], code.as_str());
        assert_eq!(v["error"]["exit"], exit, "{code}");
        assert_eq!(v["error"]["message"], "the route says no");
    }
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_refusal_is_asked_once_and_a_transport_failure_is_asked_again_with_the_same_key() {
    let rig = Rig::start(quick()).await;
    rig.route
        .push_fail(paths::SEARCH, ResearchErrorCode::RefusedUrl, "no");
    post_json(rig.addr, "/v1/search", &json!({"query": "q"})).await;
    assert_eq!(rig.route.calls_to(paths::SEARCH).len(), 1);

    rig.route.push_transient(paths::SEARCH);
    rig.route.push_transient(paths::SEARCH);
    rig.route.push_ok(paths::SEARCH, &search_answer());
    let (status, _) = post_json(rig.addr, "/v1/search", &json!({"query": "q2"})).await;
    assert_eq!(status, 200);
    let calls = rig.route.calls_to(paths::SEARCH);
    assert_eq!(calls.len(), 1 + 3);
    let keys: Vec<&str> = calls[1..]
        .iter()
        .map(|c| c["idempotencyKey"].as_str().unwrap())
        .collect();
    assert_eq!(keys[0], keys[1]);
    assert_eq!(keys[1], keys[2]);
    assert_ne!(keys[0], calls[0]["idempotencyKey"].as_str().unwrap());
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_stop_after_the_set_number_and_the_last_failure_is_returned() {
    let rig = Rig::start(Settings {
        retries: 2,
        ..quick()
    })
    .await;
    for _ in 0..5 {
        rig.route.push_transient(paths::SEARCH);
    }
    let (status, v) = post_json(rig.addr, "/v1/search", &json!({"query": "q"})).await;
    assert_eq!(status, 500, "{v}");
    assert_eq!(v["error"]["code"], "internal");
    assert_eq!(v["error"]["exit"], 5);
    assert_eq!(
        rig.route.calls_to(paths::SEARCH).len(),
        3,
        "one try and two more"
    );
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_that_is_not_there_reads_as_research_off() {
    let rig = Rig::start(quick()).await;
    rig.route.push(
        paths::SEARCH,
        Reply::Fail(agentd::direct::research::route::RouteFailure::absent()),
    );
    let (status, v) = post_json(rig.addr, "/v1/search", &json!({"query": "q"})).await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["error"]["code"], "disabled");
    assert_eq!(
        rig.route.calls_to(paths::SEARCH).len(),
        1,
        "not asked again"
    );
    rig.done();
}

// ---- fetch ------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_places_each_page_it_names_and_answers_with_paths_and_hashes_never_with_text() {
    let rig = Rig::start(quick()).await;
    let body = text(1500, "the page text that must not travel");
    let m = put_page(
        &rig.store,
        "i-0001",
        "https://example.org/plain-object",
        &body,
    );
    rig.route.push_ok(
        paths::FETCH,
        &FetchResponse {
            items: vec![item_ref(&m)],
            failed: Vec::new(),
            request_id: "req-f1".into(),
            usage: None,
        },
    );
    let (status, v) = post_json(
        rig.addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/plain-object"]}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let item = &v["items"][0];
    assert_eq!(item["id"], "i-0001");
    assert_eq!(item["manifestSha256"], m.sha256());
    let path = item["path"].as_str().unwrap();
    assert!(path.ends_with("research/pages/i-0001.txt"), "{path}");
    assert_eq!(std::fs::read(path).unwrap(), body);
    assert_eq!(item["rel"], "research/pages/i-0001.txt");
    assert_eq!(v["unplaced"], json!([]));
    assert!(!v.to_string().contains("must not travel"));
    no_text_of(&rig, &["must not travel", "example.org"]);
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_the_route_names_twice_is_placed_once() {
    let rig = Rig::start(quick()).await;
    let m = put_page(
        &rig.store,
        "i-0001",
        "https://example.org/a",
        &text(1500, "p"),
    );
    for _ in 0..2 {
        rig.route.push_ok(
            paths::FETCH,
            &FetchResponse {
                items: vec![item_ref(&m)],
                failed: Vec::new(),
                request_id: "r".into(),
                usage: None,
            },
        );
    }
    post_json(
        rig.addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/a"]}),
    )
    .await;
    let reads = rig.store.gets.lock().unwrap().len();
    let (status, _) = post_json(
        rig.addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/a"]}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(rig.store.gets.lock().unwrap().len(), reads);
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_item_that_fails_its_checks_is_reported_beside_the_ones_that_were_placed() {
    let rig = Rig::start(quick()).await;
    let good = put_page(
        &rig.store,
        "i-0001",
        "https://example.org/a",
        &text(1500, "good"),
    );
    let bad = put_page(
        &rig.store,
        "i-0002",
        "https://example.org/b",
        &text(1500, "bad"),
    );
    rig.store.put(
        "items/i-0002/body.txt",
        b"not what the manifest says".to_vec(),
    );
    rig.route.push_ok(
        paths::FETCH,
        &FetchResponse {
            items: vec![item_ref(&good), item_ref(&bad)],
            failed: Vec::new(),
            request_id: "r".into(),
            usage: None,
        },
    );
    let (status, v) = post_json(
        rig.addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/a", "https://example.org/b"]}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["items"][0]["id"], "i-0001");
    assert_eq!(v["unplaced"][0]["id"], "i-0002");
    assert_eq!(v["unplaced"][0]["code"], "integrity");
    assert_eq!(v["unplaced"][0]["exit"], 7);
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_that_breaks_the_request_rules_is_a_usage_error_before_the_route() {
    let rig = Rig::start(quick()).await;
    let over: Vec<String> = (0..21)
        .map(|i| format!("https://example.org/{i}"))
        .collect();
    for (what, body) in [
        ("none", json!({"urls": []})),
        ("twenty-one", json!({"urls": over})),
        ("a URL that is empty", json!({"urls": [""]})),
        (
            "a member that is not defined",
            json!({"urls": ["https://example.org/a"], "depth": 2}),
        ),
    ] {
        let (status, v) = post_json(rig.addr, "/v1/fetch", &body).await;
        assert_eq!(status, 400, "{what}: {v}");
        assert_eq!(v["error"]["code"], "usage", "{what}");
    }
    assert!(rig.route.calls.lock().unwrap().is_empty());
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn urls_the_route_could_not_fetch_are_passed_on() {
    let rig = Rig::start(quick()).await;
    rig.route.push(
        paths::FETCH,
        Reply::Ok(json!({
            "items": [],
            "failed": [{"url": "https://example.org/x", "code": "not_found", "message": "gone"}],
            "requestId": "r"
        })),
    );
    let (status, v) = post_json(
        rig.addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/x"]}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["failed"][0]["code"], "not_found");
    assert_eq!(v["failed"][0]["exit"], 5);
    rig.done();
}

// ---- repositories and jobs --------------------------------------------------------------------------------------------------

fn repo_request() -> Value {
    json!({"url": "https://github.com/acme/widgets", "ref": COMMIT})
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_the_route_has_ready_is_placed_at_once() {
    let rig = Rig::start(quick()).await;
    let lib = text(2500, "lib");
    let m = put_repo(
        &rig.store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("index.js", &lib)],
    );
    let mut done = job(JobState::Succeeded, "job-1");
    done.item = Some(item_ref(&m));
    rig.route.push_ok(paths::REPO, &done);
    let (status, v) = post_json(rig.addr, "/v1/repo", &repo_request()).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["state"], "succeeded");
    assert_eq!(v["jobId"], "job-1");
    let dir = v["item"]["path"].as_str().unwrap();
    assert!(
        dir.ends_with("research/repos/acme__widgets__666df7c10035"),
        "{dir}"
    );
    assert_eq!(v["item"]["commit"], COMMIT);
    assert_eq!(v["item"]["retrievedAt"], "2026-10-16T14:30:02Z");
    assert_eq!(v["item"]["licenceSource"], "file:license");
    assert_eq!(std::fs::read(format!("{dir}/index.js")).unwrap(), lib);
    assert!(rig.route.calls_to(paths::JOB).is_empty());
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_key_of_a_repository_request_is_made_from_its_arguments() {
    // No wait: each call answers pending at once, which is enough to see the key it sent.
    let rig = Rig::start(Settings {
        wait: Duration::ZERO,
        ..quick()
    })
    .await;
    for _ in 0..3 {
        rig.route
            .push_ok(paths::REPO, &job(JobState::Queued, "job-1"));
    }
    let a = repo_request();
    let b = json!({"url": "https://github.com/acme/widgets", "ref": COMMIT, "paths": ["src"]});
    for body in [&a, &a, &b] {
        let (status, _) = post_json(rig.addr, "/v1/repo", body).await;
        assert_eq!(status, 202);
    }
    let keys: Vec<String> = rig
        .route
        .calls_to(paths::REPO)
        .iter()
        .map(|c| c["idempotencyKey"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys[0], keys[1], "the same arguments, the same key");
    assert_ne!(keys[0], keys[2]);
    assert!((1..=128).contains(&keys[0].len()));
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_job_is_waited_for_and_then_placed() {
    let rig = Rig::start(quick()).await;
    let lib = text(2500, "lib");
    let m = put_repo(
        &rig.store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("index.js", &lib)],
    );
    rig.route
        .push_ok(paths::REPO, &job(JobState::Queued, "job-1"));
    rig.route
        .push_ok(paths::JOB, &job(JobState::Running, "job-1"));
    let mut done = job(JobState::Succeeded, "job-1");
    done.item = Some(item_ref(&m));
    rig.route.push_ok(paths::JOB, &done);
    let (status, v) = post_json(rig.addr, "/v1/repo", &repo_request()).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["state"], "succeeded");
    assert_eq!(rig.route.calls_to(paths::JOB).len(), 2);
    assert_eq!(rig.route.calls_to(paths::JOB)[0]["jobId"], "job-1");
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_that_outlasts_the_wait_answers_pending_and_wait_picks_it_up() {
    let rig = Rig::start(Settings {
        wait: Duration::from_millis(60),
        ..quick()
    })
    .await;
    let lib = text(2500, "lib");
    let m = put_repo(
        &rig.store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("index.js", &lib)],
    );
    rig.route
        .push_ok(paths::REPO, &job(JobState::Queued, "job-9"));
    rig.route
        .always(paths::JOB, &job(JobState::Running, "job-9"));
    let (status, v) = post_json(rig.addr, "/v1/repo", &repo_request()).await;
    assert_eq!(status, 202, "{v}");
    assert_eq!(v["state"], "pending");
    assert_eq!(v["jobId"], "job-9");
    assert!(v["pollAfterMs"].as_u64().unwrap() >= 1);

    // The job ends; wait places the repository once, however often it is asked.
    let mut done = job(JobState::Succeeded, "job-9");
    done.item = Some(item_ref(&m));
    rig.route.always(paths::JOB, &done);
    for _ in 0..2 {
        let (status, v) = post_json(rig.addr, "/v1/wait", &json!({"jobId": "job-9"})).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["state"], "succeeded");
        assert_eq!(v["item"]["id"], "i-0002");
    }
    assert_eq!(rig.store.get_count("i-0002/body.tar.gz"), 1);
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_job_is_an_answer_not_an_error() {
    let rig = Rig::start(quick()).await;
    rig.route
        .push_ok(paths::REPO, &job(JobState::Queued, "job-1"));
    let mut failed = job(JobState::Failed, "job-1");
    failed.error = Some(harness_protocol::research::ResearchError::new(
        ResearchErrorCode::NotFound,
        "no such repository",
    ));
    rig.route.push_ok(paths::JOB, &failed);
    let (status, v) = post_json(rig.addr, "/v1/repo", &repo_request()).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["state"], "failed");
    assert_eq!(v["error"]["code"], "not_found");
    assert_eq!(v["error"]["exit"], 5);
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_for_a_job_the_route_does_not_know_is_not_found() {
    let rig = Rig::start(quick()).await;
    rig.route
        .push_fail(paths::JOB, ResearchErrorCode::NotFound, "no such job");
    let (status, v) = post_json(rig.addr, "/v1/wait", &json!({"jobId": "job-x"})).await;
    assert_eq!(status, 404, "{v}");
    assert_eq!(v["error"]["exit"], 5);
    let (status, v) = post_json(rig.addr, "/v1/wait", &json!({"jobId": "bad id!"})).await;
    assert_eq!(status, 400, "{v}");
    rig.done();
}

// ---- index, limits, verify, check -------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_index_is_served_as_json_and_as_the_table_for_the_context_file() {
    let rig = Rig::start(quick()).await;
    let m = put_page(
        &rig.store,
        "i-0001",
        "https://example.org/a",
        &text(1500, "p"),
    );
    rig.route.push_ok(
        paths::FETCH,
        &FetchResponse {
            items: vec![item_ref(&m)],
            failed: Vec::new(),
            request_id: "r".into(),
            usage: None,
        },
    );
    post_json(
        rig.addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/a"]}),
    )
    .await;
    let (status, v) = get_json(rig.addr, "/v1/index").await;
    assert_eq!(status, 200);
    assert_eq!(v["items"][0]["item_id"], "i-0001");
    assert_eq!(v["items"][0]["manifest_sha256"], m.sha256());
    let (status, md) = http(rig.addr, "GET", "/v1/index?format=md", None).await;
    assert_eq!(status, 200);
    assert!(
        md.starts_with("| Origin | Date read | sha256 | Licence | Why it matters |"),
        "{md}"
    );
    assert!(md.contains(&m.sha256()));
    let (status, _) = get_json(rig.addr, "/v1/index?format=xml").await;
    assert_eq!(status, 400);
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn limits_come_from_the_route_with_what_is_stored_here() {
    let rig = Rig::start(quick()).await;
    rig.route.push_ok(
        paths::LIMITS,
        &LimitsResponse {
            enabled: true,
            provider: Some(ResearchProvider::Fixture),
            limits: ResearchLimits::default(),
            usage: ResearchUsage {
                searches: 3,
                ..ResearchUsage::default()
            },
            expires_at: Some(1_900_000_000),
        },
    );
    let (status, v) = get_json(rig.addr, "/v1/limits").await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["usage"]["searches"], 3);
    assert_eq!(v["limits"]["searches"], 200);
    assert_eq!(v["context"]["usedBytes"], 0);
    assert_eq!(v["context"]["maxBytes"], 300 * 1024 * 1024);
    rig.route.push_fail(
        paths::LIMITS,
        ResearchErrorCode::Disabled,
        "research is off for this run",
    );
    let (status, v) = get_json(rig.addr, "/v1/limits").await;
    assert_eq!(status, 503, "{v}");
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn verify_says_ok_for_what_was_placed_and_names_what_changed() {
    let rig = Rig::start(quick()).await;
    let m = put_page(
        &rig.store,
        "i-0001",
        "https://example.org/a",
        &text(1500, "p"),
    );
    rig.route.push_ok(
        paths::FETCH,
        &FetchResponse {
            items: vec![item_ref(&m)],
            failed: Vec::new(),
            request_id: "r".into(),
            usage: None,
        },
    );
    post_json(
        rig.addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/a"]}),
    )
    .await;
    let (status, v) = get_json(rig.addr, "/v1/verify").await;
    assert_eq!((status, &v["ok"]), (200, &json!(true)), "{v}");
    let page = rig.data.join("context/research/pages/i-0001.txt");
    std::fs::set_permissions(&page, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    std::fs::write(&page, b"changed").unwrap();
    let (status, v) = get_json(rig.addr, "/v1/verify").await;
    assert_eq!(status, 200);
    assert_eq!(v["ok"], false);
    assert_eq!(v["problems"][0]["itemId"], "i-0001");
    assert_eq!(v["problems"][0]["path"], "body");
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn check_lists_work_tree_files_that_equal_stored_files_and_refuses_other_directories() {
    let rig = Rig::start(quick()).await;
    let lib = text(2500, "lib");
    let m = put_repo(
        &rig.store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("lib/util.js", &lib)],
    );
    let mut done = job(JobState::Succeeded, "job-1");
    done.item = Some(item_ref(&m));
    rig.route.push_ok(paths::REPO, &done);
    post_json(rig.addr, "/v1/repo", &repo_request()).await;

    std::fs::create_dir_all(rig.work.join("src")).unwrap();
    std::fs::write(rig.work.join("src/copied.js"), &lib).unwrap();
    std::fs::write(rig.work.join("src/own.js"), text(2500, "mine")).unwrap();
    let (status, v) = post_json(rig.addr, "/v1/check", &json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["matches"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["matches"][0]["path"], "src/copied.js");
    assert_eq!(v["matches"][0]["itemId"], "i-0002");
    assert_eq!(v["matches"][0]["licence"], "MIT");
    assert_eq!(v["matches"][0]["kind"], "exact");
    assert_eq!(v["floorBytes"], 1024);
    let (status, v) = post_json(rig.addr, "/v1/check", &json!({"dir": "src"})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["matches"].as_array().unwrap().len(), 1);
    // A directory that is not inside the work tree is not read.
    let elsewhere = rig.data.display().to_string();
    let (status, v) = post_json(rig.addr, "/v1/check", &json!({"dir": elsewhere})).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "usage");
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_check_answer_names_the_runs_third_party_mode() {
    // `thirdParty` of the start request: the default is block, and `allow` is the user's per-run setting. The command prints the
    // consequence that is true for the run.
    let rig = Rig::start(quick()).await;
    let (status, v) = post_json(rig.addr, "/v1/check", &json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["thirdParty"], "block", "{v}");
    rig.shared.set_third_party(true);
    let (status, v) = post_json(rig.addr, "/v1/check", &json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["thirdParty"], "allow", "{v}");
    rig.done();
}

// ---- load -------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn calls_past_the_number_served_at_once_are_told_to_come_back() {
    let rig = Rig::start(Settings {
        max_inflight: 2,
        ..quick()
    })
    .await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *rig.route.gate.lock().unwrap() = Some(gate.clone());
    rig.route.always(paths::SEARCH, &search_answer());
    let addr = rig.addr;
    let held: Vec<_> = (0..2)
        .map(|_| {
            tokio::spawn(async move { post_json(addr, "/v1/search", &json!({"query": "q"})).await })
        })
        .collect();
    // Wait until both are inside the route.
    let started = std::time::Instant::now();
    while rig.route.calls.lock().unwrap().len() < 2 {
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (status, v) = post_json(rig.addr, "/v1/search", &json!({"query": "q"})).await;
    assert_eq!(status, 429, "{v}");
    assert_eq!(v["error"]["code"], "limit_rate");
    gate.add_permits(10);
    for h in held {
        assert_eq!(h.await.unwrap().0, 200);
    }
    let (status, _) = post_json(rig.addr, "/v1/search", &json!({"query": "q"})).await;
    assert_eq!(status, 200);
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_counters_say_what_the_run_did_and_hold_no_text() {
    let rig = Rig::start(quick()).await;
    rig.route.push_ok(paths::SEARCH, &search_answer());
    post_json(
        rig.addr,
        "/v1/search",
        &json!({"query": "secret topic words"}),
    )
    .await;
    rig.route
        .push_fail(paths::SEARCH, ResearchErrorCode::RefusedUrl, "no");
    post_json(rig.addr, "/v1/search", &json!({"query": "another"})).await;
    let s = rig.shared.stats();
    assert_eq!((s.searches, s.refused), (2, 1));
    assert!(!format!("{s:?}").contains("secret topic"));
    rig.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_counters_behind_the_summary_note_count_what_the_calls_did() {
    // These are the numbers the research module copies into `summary.json` at the end of the run (`DirectResearchCounts`).
    let rig = Rig::start(quick()).await;
    let body = text(1500, "the page");
    let m = put_page(&rig.store, "i-0001", "https://example.org/a", &body);
    rig.route.always(paths::SEARCH, &search_answer());
    rig.route.push_ok(
        paths::FETCH,
        &FetchResponse {
            items: vec![item_ref(&m)],
            failed: Vec::new(),
            request_id: "req-f1".into(),
            usage: None,
        },
    );
    rig.route.push_fail(
        paths::FETCH,
        ResearchErrorCode::RefusedUrl,
        "the address is not one this run saw",
    );
    rig.route.push_fail(
        paths::REPO,
        ResearchErrorCode::UpstreamError,
        "the upstream failed",
    );
    assert_eq!(rig.shared.stats(), Stats::default());
    post_json(rig.addr, "/v1/search", &json!({"query": "q"})).await;
    post_json(
        rig.addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/a"]}),
    )
    .await;
    post_json(
        rig.addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/b"]}),
    )
    .await;
    post_json(
        rig.addr,
        "/v1/repo",
        &json!({"url": "https://github.com/acme/widgets", "ref": COMMIT}),
    )
    .await;
    assert_eq!(
        rig.shared.stats(),
        Stats {
            searches: 1,
            fetches: 2,
            repos: 1,
            waits: 0,
            placed: 1,
            refused: 1,
            errors: 1,
        }
    );
    assert_eq!(rig.shared.placer().used_bytes(), body.len() as u64);
    rig.done();
}
