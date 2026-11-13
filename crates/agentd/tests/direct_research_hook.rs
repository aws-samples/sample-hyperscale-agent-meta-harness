// COMPAT-CAH-DATA: this file names the earlier variable spellings that the engine environment carries beside the new ones until VERSION 6.00.
//! The research module inside a whole run: the module is registered through `Deps.hooks`, the
//! agent runs in process against the fake AWS and the fake ACP engine, and the route and the bucket are fakes. The tests pin what the
//! engine sees (an address and a directory in its environment, a server that answers while it works), what happens when research is off or
//! not there (nothing: no variable, no server, no log line for a missing route), that the run is the run it would have been, and that the
//! module leaves counts and no text behind.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;
mod research_support;

use agentd::BoxFuture;
use agentd::direct::Deps;
use agentd::direct::hooks::{DirectHook, Hooks};
use agentd::direct::research::api::Stats;
use agentd::direct::research::counts_of;
use agentd::direct::research::route::{Route, RouteFailure};
use agentd::direct::research::settings::Settings;
use agentd::direct::research::{Parts, ResearchHook};
use direct_support::{Direct, Opts, RUN, sha};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::envelope::EventType;
use harness_protocol::research::route as paths;
use harness_protocol::research::{
    DirectResearchCounts, DirectResearchNote, FetchResponse, ItemKind, ItemRef, LimitsResponse,
    ResearchErrorCode, ResearchLimits, ResearchProvider, ResearchUsage, SearchResponse,
    SearchResult, s3,
};
use research_support::{FakeRoute, FakeStore, Reply, post_json};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn limits_answer() -> LimitsResponse {
    LimitsResponse {
        enabled: true,
        provider: Some(ResearchProvider::Fixture),
        limits: ResearchLimits::default(),
        usage: ResearchUsage::default(),
        expires_at: None,
    }
}

fn search_answer() -> SearchResponse {
    SearchResponse {
        id: "s-0001".into(),
        provider: ResearchProvider::Fixture,
        results: vec![SearchResult {
            title: "Plain objects".into(),
            url: "https://example.org/plain".into(),
            snippet: "A short snippet from the page.".into(),
            published: None,
            score: None,
        }],
        credits: Some(1),
        request_id: "req-1".into(),
        usage: None,
    }
}

fn module(route: Arc<FakeRoute>) -> Hooks {
    module_on(route, 0)
}

fn module_on(route: Arc<FakeRoute>, port: u16) -> Hooks {
    let settings = Settings {
        port,
        retry_pause: Duration::from_millis(5),
        ..Settings::default()
    };
    let hook = ResearchHook::with_parts(
        settings,
        Parts {
            route,
            store: Arc::new(FakeStore::default()),
        },
    );
    Hooks::new(vec![Arc::new(hook) as Arc<dyn DirectHook>])
}

/// The environment line `name=value` the fake engine wrote down when it started.
fn env_of(d: &Direct, name: &str) -> Option<String> {
    d.engine_probe()
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .map(str::to_string)
}

fn silent_engine() -> Opts {
    Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "3".to_string())],
        ..Opts::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_gets_the_address_and_the_directory_and_the_server_answers_while_it_works() {
    let d = Direct::start(silent_engine()).await;
    let route = Arc::new(FakeRoute::default());
    route.always(paths::LIMITS, &limits_answer());
    route.always(paths::SEARCH, &search_answer());
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    let (app, req) = (d.app.clone(), d.req.clone());
    let run = tokio::spawn(async move { agentd::direct::run_with(&app, req, &deps).await });

    let started = Instant::now();
    let url = loop {
        if let Some(u) = env_of(&d, "HAMH_RESEARCH_URL") {
            break u;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{}",
            d.engine_probe()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let addr: SocketAddr = url.strip_prefix("http://").unwrap().parse().unwrap();
    assert!(addr.ip().is_loopback());
    assert_eq!(
        env_of(&d, "HAMH_CONTEXT_DIR").unwrap(),
        d.agent_data().join("context").display().to_string()
    );
    assert_eq!(env_of(&d, "HAMH_RESEARCH_WAIT_S").unwrap(), "90");
    // COMPAT-CAH-DATA: rules the user edited before the rename still name the earlier variables: the engine gets both spellings, with the same values.
    for name in ["RESEARCH_URL", "RESEARCH_WAIT_S", "CONTEXT_DIR"] {
        assert_eq!(
            env_of(&d, &format!("CAH_{name}")),
            env_of(&d, &format!("HAMH_{name}")),
            "{name}"
        ); // COMPAT-CAH-DATA
        assert!(env_of(&d, &format!("HAMH_{name}")).is_some(), "{name}");
    }
    assert!(d.agent_data().join("context/research/pages").is_dir());

    let (status, body) = post_json(
        addr,
        "/v1/search",
        &json!({"query": "what counts as a plain object"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["results"][0]["url"], "https://example.org/plain");

    let resp = run.await.unwrap();
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone, "{resp:?}");
    // The server went with the engine.
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
    // The route was asked whether research is on before anything else.
    assert_eq!(route.calls.lock().unwrap()[0].0, paths::LIMITS);

    // Counts, no text: one progress event and one log line.
    let progress: Vec<Value> = d
        .events_of(EventType::TurnProgress)
        .into_iter()
        .filter(|v| v["kind"] == "research")
        .collect();
    assert_eq!(progress.len(), 1, "{progress:?}");
    assert_eq!(progress[0]["searches"], 1);
    let everything = format!("{}{}", d.log.lines().join("\n"), progress[0]);
    for text in ["plain object", "example.org", "short snippet"] {
        assert!(
            !everything.contains(text),
            "{text} is in the log or the event"
        );
    }
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct research loopback API on 127.0.0.1"))
    );
    direct_support::no_secrets(&everything);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_whose_policy_has_research_off_gets_no_server_and_no_variable() {
    let d = Direct::start(Opts::default()).await;
    let route = Arc::new(FakeRoute::default());
    route.push_fail(
        paths::LIMITS,
        ResearchErrorCode::Disabled,
        "research is off for this run",
    );
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(env_of(&d, "HAMH_RESEARCH_URL").is_none());
    assert!(env_of(&d, "HAMH_CONTEXT_DIR").is_none());
    assert!(!d.agent_data().join("context/research").exists());
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct research off")),
        "{:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stage_without_the_route_changes_nothing_and_says_nothing() {
    let d = Direct::start(Opts::default()).await;
    let route = Arc::new(FakeRoute::default());
    route.push(paths::LIMITS, Reply::Fail(RouteFailure::absent()));
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(env_of(&d, "HAMH_RESEARCH_URL").is_none());
    assert!(
        !d.log.lines().iter().any(|l| l.contains("research")),
        "{:?}",
        d.log.lines()
    );
    assert!(
        d.events_of(EventType::TurnProgress)
            .iter()
            .all(|v| v["kind"] != "research")
    );
    assert_eq!(
        route.calls.lock().unwrap().len(),
        1,
        "asked once, not again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_users_switch_means_the_route_is_not_even_asked() {
    let d = Direct::start(Opts::default()).await;
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    v["research"] = json!({"enabled": false});
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    let mut req = d.req.clone();
    req.request.sha256 = sha(&bytes);
    let route = Arc::new(FakeRoute::default());
    route.always(paths::LIMITS, &limits_answer());
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, req, &deps).await;
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(route.calls.lock().unwrap().is_empty());
    assert!(env_of(&d, "HAMH_RESEARCH_URL").is_none());
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("switched it off for this run")),
        "{:?}",
        d.log.lines()
    );
    // The card shows it: one `research` event of phase `off`. A stage without the route stays silent (see above).
    let ev = d.aws.progress("research");
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].1["phase"], "off");
    let text = ev[0].1["text"].as_str().unwrap();
    assert!(
        text.starts_with("Research off:") && text.contains("you switched it off"),
        "{text}"
    );
    println!(
        "RESEARCH-OFF TRIGGER PRESENT: request research.enabled=false -> route asked 0 times, one research event phase=off"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_that_does_not_answer_the_first_question_does_not_keep_research_from_starting() {
    let d = Direct::start(Opts::default()).await;
    let route = Arc::new(FakeRoute::default());
    for _ in 0..8 {
        route.push_transient(paths::LIMITS);
    }
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(
        env_of(&d, "HAMH_RESEARCH_URL").is_some(),
        "{}",
        d.engine_probe()
    );
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("starting without an answer from the route")),
        "{:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_with_the_module_ends_as_a_run_without_it_would() {
    let with = Direct::start(Opts::default()).await;
    let route = Arc::new(FakeRoute::default());
    route.always(paths::LIMITS, &limits_answer());
    let deps = Deps {
        hooks: Some(module(route)),
        ..Deps::default()
    };
    agentd::direct::run_with(&with.app, with.req.clone(), &deps).await;
    let without = Direct::start(Opts::default()).await;
    let none = Deps {
        hooks: Some(Hooks::none()),
        ..Deps::default()
    };
    agentd::direct::run_with(&without.app, without.req.clone(), &none).await;
    let (a, b) = (with.summary(), without.summary());
    assert_eq!(a.outcome, b.outcome);
    assert_eq!(a.attempts.len(), b.attempts.len());
    assert_eq!(a.cline_message, b.cline_message);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_that_refuses_for_another_reason_keeps_research_off_and_names_the_word() {
    let d = Direct::start(Opts::default()).await;
    let route = Arc::new(FakeRoute::default());
    route.push_fail(
        paths::LIMITS,
        ResearchErrorCode::NoKey,
        "the stage has no search key",
    );
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(env_of(&d, "HAMH_RESEARCH_URL").is_none());
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct research off: the route answered no_key")),
        "{:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_that_is_taken_gives_the_engine_another_loopback_port() {
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let busy = taken.local_addr().unwrap().port();
    let d = Direct::start(silent_engine()).await;
    let route = Arc::new(FakeRoute::default());
    route.always(paths::LIMITS, &limits_answer());
    let deps = Deps {
        hooks: Some(module_on(route.clone(), busy)),
        ..Deps::default()
    };
    let (app, req) = (d.app.clone(), d.req.clone());
    let run = tokio::spawn(async move {
        agentd::direct::run_with(&app, req, &deps).await;
    });
    let started = Instant::now();
    let url = loop {
        if let Some(u) = env_of(&d, "HAMH_RESEARCH_URL") {
            break u;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{}",
            d.engine_probe()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let addr: SocketAddr = url.strip_prefix("http://").unwrap().parse().unwrap();
    assert!(addr.ip().is_loopback());
    assert_ne!(addr.port(), busy, "{url}");
    run.await.unwrap();
    drop(taken);
}

// ---- the counters in summary.json ----------------------------------------------------------------------------------

/// `fixtures/research/summary-research.json`: the `research` member the writer below must produce for the run of the first test, and the
/// bytes `harness-protocol`, `run-session` and `ham-core` are tested against.
const SUMMARY_RESEARCH: &str = include_str!("../../../fixtures/research/summary-research.json");

/// The route's answer names a page for the run `Direct` starts that the bucket does not hold (this harness's run id is not a ULID, so a stored
/// item could not pass the manifest check anyway): the placement fails, which is one of the ways a call ends in the `errors` count.
fn page_the_bucket_lacks(id: &str, origin: &str) -> ItemRef {
    let body = "plain object page text\n".repeat(60);
    let m = research_support::manifest(id, ItemKind::Page, origin, "body.txt", body.as_bytes());
    ItemRef {
        manifest_key: s3::manifest_key(RUN, id),
        body_key: format!("{}items/{id}/body.txt", s3::prefix(RUN)),
        ..research_support::item_ref(&m)
    }
}

fn module_over(route: Arc<dyn Route>, store: Arc<FakeStore>) -> Hooks {
    let settings = Settings {
        port: 0,
        min_free_bytes: 0,
        retry_pause: Duration::from_millis(5),
        ..Settings::default()
    };
    let hook = ResearchHook::with_parts(settings, Parts { route, store });
    Hooks::new(vec![Arc::new(hook) as Arc<dyn DirectHook>])
}

/// The address of the module's server once the engine has been started, and the join handle of the run.
async fn running(d: &Direct, deps: Deps) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let (app, req) = (d.app.clone(), d.req.clone());
    let run = tokio::spawn(async move {
        agentd::direct::run_with(&app, req, &deps).await;
    });
    let started = Instant::now();
    let url = loop {
        if let Some(u) = env_of(d, "HAMH_RESEARCH_URL") {
            break u;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{}",
            d.engine_probe()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    (url.strip_prefix("http://").unwrap().parse().unwrap(), run)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_summary_carries_what_the_module_counted_and_what_the_route_counted_at_the_end() {
    let d = Direct::start(silent_engine()).await;
    let route = Arc::new(FakeRoute::default());
    let store = Arc::new(FakeStore::default());
    let item = page_the_bucket_lacks("i-0001", "https://example.org/plain");
    // The first answer decides that research is on; the second, read when the run ends, is the route's own count.
    route.push_ok(paths::LIMITS, &limits_answer());
    let mut last = limits_answer();
    last.usage = ResearchUsage {
        searches: 2,
        fetched_urls: 1,
        repos: 0,
        outbound_bytes: 412,
        context_bytes: 1380,
    };
    route.push_ok(paths::LIMITS, &last);
    route.always(paths::SEARCH, &search_answer());
    route.push_ok(
        paths::FETCH,
        &FetchResponse {
            items: vec![item],
            failed: Vec::new(),
            request_id: "req-f1".into(),
            usage: None,
        },
    );
    route.push_fail(
        paths::FETCH,
        ResearchErrorCode::RefusedUrl,
        "the address is not one this run saw",
    );
    route.push_fail(
        paths::REPO,
        ResearchErrorCode::UpstreamError,
        "the upstream failed",
    );
    let deps = Deps {
        hooks: Some(module_over(route.clone(), store)),
        ..Deps::default()
    };
    let (addr, run) = running(&d, deps).await;

    for q in ["one", "two"] {
        let (status, body) = post_json(addr, "/v1/search", &json!({"query": q})).await;
        assert_eq!(status, 200, "{body}");
    }
    let (status, body) = post_json(
        addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/plain"]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["unplaced"][0]["id"], "i-0001",
        "the page is not in the bucket: {body}"
    );
    let (status, body) = post_json(
        addr,
        "/v1/fetch",
        &json!({"urls": ["https://example.org/other"]}),
    )
    .await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(body["error"]["code"], "refused_url", "{body}");
    let (status, body) = post_json(
        addr,
        "/v1/repo",
        &json!({"url": "https://github.com/acme/widgets", "ref": "666df7c10035f7e26f27ec214fe5ae3173435f34"}),
    )
    .await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(body["error"]["code"], "upstream_error", "{body}");
    run.await.unwrap();

    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "the label is the run's"
    );
    let note = s
        .research
        .clone()
        .expect("a lane that started leaves its note");
    let golden: Value = serde_json::from_str(SUMMARY_RESEARCH).unwrap();
    assert_eq!(serde_json::to_value(&note).unwrap(), golden["research"]);
    assert_eq!(note.usage, Some(last.usage), "the route's count at the end");
    // The same numbers as the module's own event, and no text in either.
    let progress: Vec<Value> = d
        .events_of(EventType::TurnProgress)
        .into_iter()
        .filter(|v| v["kind"] == "research")
        .collect();
    assert_eq!(progress.len(), 1, "{progress:?}");
    let c = note.counts.unwrap();
    for (key, want) in [
        ("searches", c.searches),
        ("fetches", c.fetches),
        ("repos", c.repos),
        ("placed", c.placed),
        ("refused", c.refused),
        ("errors", c.errors),
        ("placedBytes", c.placed_bytes),
    ] {
        assert_eq!(progress[0][key], want, "{key}: {progress:?}");
    }
    let text = serde_json::to_string(&s).unwrap();
    assert!(!text.contains("example.org") && !text.contains("plain object"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lane_that_started_and_was_never_called_still_leaves_its_note() {
    let d = Direct::start(Opts::default()).await;
    let route = Arc::new(FakeRoute::default());
    route.always(paths::LIMITS, &limits_answer());
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        s.research,
        Some(DirectResearchNote {
            enabled: true,
            provider: Some(ResearchProvider::Fixture),
            usage: Some(ResearchUsage::default()),
            counts: Some(DirectResearchCounts::default()),
        })
    );
    // The progress event stays for runs that used the lane.
    assert!(
        d.events_of(EventType::TurnProgress)
            .iter()
            .all(|v| v["kind"] != "research")
    );
}

fn off_note() -> Option<DirectResearchNote> {
    Some(DirectResearchNote {
        enabled: false,
        provider: None,
        usage: None,
        counts: None,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_users_switch_and_a_route_that_refuses_leave_a_note_that_says_research_was_off() {
    // The user switched it off: the route is not asked.
    let d = Direct::start(Opts::default()).await;
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    v["research"] = json!({"enabled": false});
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    let mut req = d.req.clone();
    req.request.sha256 = sha(&bytes);
    let route = Arc::new(FakeRoute::default());
    route.always(paths::LIMITS, &limits_answer());
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, req, &deps).await;
    assert_eq!(d.summary().research, off_note());
    assert!(route.calls.lock().unwrap().is_empty());

    // The run's policy has it off, or the stage has no key: the route said so.
    for code in [ResearchErrorCode::Disabled, ResearchErrorCode::NoKey] {
        let d = Direct::start(Opts::default()).await;
        let route = Arc::new(FakeRoute::default());
        route.push_fail(paths::LIMITS, code, "off");
        let deps = Deps {
            hooks: Some(module(route.clone())),
            ..Deps::default()
        };
        agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
        assert_eq!(d.summary().research, off_note(), "{code}");
        assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stage_without_the_route_writes_the_summary_it_wrote_before() {
    let d = Direct::start(Opts::default()).await;
    let route = Arc::new(FakeRoute::default());
    route.push(paths::LIMITS, Reply::Fail(RouteFailure::absent()));
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    let s = d.summary();
    assert_eq!(s.research, None);
    assert!(serde_json::to_value(&s).unwrap().get("research").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_final_read_of_the_route_that_fails_loses_the_usage_and_keeps_the_counts() {
    let d = Direct::start(silent_engine()).await;
    let route = Arc::new(FakeRoute::default());
    route.push_ok(paths::LIMITS, &limits_answer());
    route.always(paths::SEARCH, &search_answer());
    // No second answer: the read at the end fails.
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    let (addr, run) = running(&d, deps).await;
    let (status, body) = post_json(addr, "/v1/search", &json!({"query": "one"})).await;
    assert_eq!(status, 200, "{body}");
    run.await.unwrap();
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    let note = s.research.unwrap();
    assert!(note.enabled);
    assert_eq!(note.usage, None, "unknown is not zero");
    assert_eq!(note.counts.unwrap().searches, 1);
}

/// A route whose first `limits` answer comes at once and whose next one never comes.
struct HangsAtTheEnd {
    asked: AtomicBool,
    inner: FakeRoute,
}

impl Route for HangsAtTheEnd {
    fn call<'a>(
        &'a self,
        path: &'static str,
        body: &'a Value,
    ) -> BoxFuture<'a, Result<Value, RouteFailure>> {
        Box::pin(async move {
            if path == paths::LIMITS && self.asked.swap(true, Ordering::SeqCst) {
                return std::future::pending::<Result<Value, RouteFailure>>().await;
            }
            self.inner.call(path, body).await
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_final_read_that_never_answers_does_not_hold_the_run_and_does_not_lose_the_counts() {
    let d = Direct::start(silent_engine()).await;
    let inner = FakeRoute::default();
    inner.always(paths::LIMITS, &limits_answer());
    inner.always(paths::SEARCH, &search_answer());
    let route = Arc::new(HangsAtTheEnd {
        asked: AtomicBool::new(false),
        inner,
    });
    let deps = Deps {
        hooks: Some(module_over(route, Arc::new(FakeStore::default()))),
        ..Deps::default()
    };
    let (addr, run) = running(&d, deps).await;
    let (status, body) = post_json(addr, "/v1/search", &json!({"query": "one"})).await;
    assert_eq!(status, 200, "{body}");
    let ended = Instant::now();
    run.await.unwrap();
    assert!(
        ended.elapsed() < Duration::from_secs(14),
        "the read at the end is bounded (3 s), took {:?}",
        ended.elapsed()
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    let note = s.research.expect("the module was not switched off");
    assert_eq!(note.usage, None);
    assert_eq!(note.counts.unwrap().searches, 1);
}

#[test]
fn the_counts_of_the_note_are_the_counters_of_the_api_one_for_one() {
    // Eight different numbers, so that two swapped fields cannot give the same note.
    let s = Stats {
        searches: 1,
        fetches: 2,
        repos: 3,
        waits: 4,
        placed: 5,
        refused: 6,
        errors: 7,
    };
    assert_eq!(
        counts_of(s, 8),
        DirectResearchCounts {
            searches: 1,
            fetches: 2,
            repos: 3,
            waits: 4,
            placed: 5,
            refused: 6,
            errors: 7,
            placed_bytes: 8,
        }
    );
}

/// Puts members into the request the run fetches, and the hash the invocation carries.
fn with_members(d: &mut Direct, members: Value) {
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    for (k, val) in members.as_object().unwrap() {
        v[k] = val.clone();
    }
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    d.req.request.sha256 = sha(&bytes);
    d.request_json = bytes;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_requests_third_party_member_reaches_the_loopback_checks_answer() {
    // The user's `--allow-third-party` (request member `thirdParty: allow`) is read at startup; the same run without it answers `block`.
    for (member, want) in [(Some("allow"), "allow"), (None, "block")] {
        let mut d = Direct::start(silent_engine()).await;
        if let Some(m) = member {
            with_members(&mut d, json!({"thirdParty": m}));
        }
        let route = Arc::new(FakeRoute::default());
        route.always(paths::LIMITS, &limits_answer());
        let deps = Deps {
            hooks: Some(module(route.clone())),
            ..Deps::default()
        };
        let (app, req) = (d.app.clone(), d.req.clone());
        let run = tokio::spawn(async move { agentd::direct::run_with(&app, req, &deps).await });
        let started = Instant::now();
        let url = loop {
            if let Some(u) = env_of(&d, "HAMH_RESEARCH_URL") {
                break u;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "{}",
                d.engine_probe()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let addr: SocketAddr = url.strip_prefix("http://").unwrap().parse().unwrap();
        let (status, body) = post_json(addr, "/v1/check", &json!({})).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["thirdParty"], want, "{body}");
        run.await.unwrap();
        println!(
            "THIRD-PARTY TRIGGER PRESENT: request thirdParty={member:?} -> check answers {want}"
        );
    }
}

// ---- the listing of what agentd placed ----------------------------------------------------------------------------
//
// What the listing holds for placed items (the hash of each file as it is on the disk, the keys of the judge) is pinned in
// `direct_research_place.rs`. The run id of `direct_support` is not a ULID, which a manifest must carry, so no item can be placed in these
// whole runs; they pin where the listing goes, when it is made and that it can never change the run.

const LISTING_NAME: &str = "context-listing.json";

#[tokio::test(flavor = "multi_thread")]
async fn the_listing_is_uploaded_where_the_collector_reads_it_and_is_the_file_agentd_wrote() {
    // A lane that started and placed nothing still says so: an empty listing is evidence, a missing one is not.
    let d = Direct::start(Opts::default()).await;
    let route = Arc::new(FakeRoute::default());
    route.always(paths::LIMITS, &limits_answer());
    let deps = Deps {
        hooks: Some(module(route.clone())),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    let key = keys::context_listing_key(RUN);
    assert_eq!(
        key,
        format!("runs/{RUN}/agents/direct/{LISTING_NAME}"),
        "the key the collector of the research checks reads"
    );
    let bytes = d.aws.get(&key).unwrap_or_else(|| {
        panic!(
            "no listing in the bucket: {:?}\n{:?}",
            d.aws.keys(),
            d.log.lines()
        )
    });
    let listing: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        listing,
        json!({"schema": "hamh-research-placed/1", "run": RUN, "items": {}}),
        "a lane that placed nothing lists no items"
    );
    assert_eq!(
        std::fs::read(d.agent_data().join(LISTING_NAME)).unwrap(),
        bytes,
        "the file agentd wrote is the object it uploaded"
    );
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct research listing uploaded")),
        "{:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_where_research_did_not_run_leaves_no_listing() {
    // The route says research is off for the run, the user switched it off, and the stage has no route: in none of them did agentd place
    // anything, so there is nothing to list and no file or object is made.
    let off_by_policy = Direct::start(Opts::default()).await;
    let route = Arc::new(FakeRoute::default());
    route.push_fail(
        paths::LIMITS,
        ResearchErrorCode::Disabled,
        "research is off for this run",
    );
    let deps = Deps {
        hooks: Some(module(route)),
        ..Deps::default()
    };
    agentd::direct::run_with(&off_by_policy.app, off_by_policy.req.clone(), &deps).await;

    let no_route = Direct::start(Opts::default()).await;
    let route = Arc::new(FakeRoute::default());
    route.push(paths::LIMITS, Reply::Fail(RouteFailure::absent()));
    let deps = Deps {
        hooks: Some(module(route)),
        ..Deps::default()
    };
    agentd::direct::run_with(&no_route.app, no_route.req.clone(), &deps).await;

    let switched_off = Direct::start(Opts::default()).await;
    let mut v: Value = serde_json::from_slice(&switched_off.request_json).unwrap();
    v["research"] = json!({"enabled": false});
    let bytes = serde_json::to_vec(&v).unwrap();
    switched_off.aws.put(&keys::request_key(RUN), bytes.clone());
    let mut req = switched_off.req.clone();
    req.request.sha256 = sha(&bytes);
    let route = Arc::new(FakeRoute::default());
    route.always(paths::LIMITS, &limits_answer());
    let deps = Deps {
        hooks: Some(module(route)),
        ..Deps::default()
    };
    agentd::direct::run_with(&switched_off.app, req, &deps).await;

    for (what, d) in [
        ("the route said off", &off_by_policy),
        ("the stage has no route", &no_route),
        ("the user switched it off", &switched_off),
    ] {
        assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone, "{what}");
        assert!(
            d.aws.get(&keys::context_listing_key(RUN)).is_none(),
            "{what}: a listing was uploaded"
        );
        assert!(
            !d.agent_data().join(LISTING_NAME).exists(),
            "{what}: a listing was written"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_that_fails_loses_the_listing_and_nothing_else() {
    let d = Direct::start(Opts::default()).await;
    d.aws.fail_puts(LISTING_NAME, 20);
    let route = Arc::new(FakeRoute::default());
    route.always(paths::LIMITS, &limits_answer());
    let deps = Deps {
        hooks: Some(module(route)),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(resp.bundle_uri.is_some());
    assert!(d.aws.get(&keys::context_listing_key(RUN)).is_none());
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct research listing not uploaded")),
        "{:?}",
        d.log.lines()
    );
    assert_eq!(d.aws.complete().len(), 1, "turn-complete was posted");
    assert!(
        d.summary().research.is_some(),
        "the note of the lane is there as before"
    );
}
