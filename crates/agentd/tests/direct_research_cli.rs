//! The `ham-research` command against the real loopback API. Every argument is ordinary text.
#![allow(clippy::unwrap_used)]

mod common;
mod research_support;

use agentd::direct::context::ContextDir;
use agentd::direct::research::api::{self, Server, Shared};
use agentd::direct::research::client::Client;
use agentd::direct::research::extract::remove_tree;
use agentd::direct::research::place::Placer;
use agentd::direct::research::settings::Settings;
use agentd::logging::MemoryLog;
use common::{repo_root, scratch};
use harness_protocol::research::route as paths;
use harness_protocol::research::{
    FetchResponse, JobResponse, JobState, ResearchErrorCode, ResearchProvider, SearchResponse,
    SearchResult,
};
use research_support::*;
use std::sync::Arc;
use std::time::Duration;

struct Rig {
    data: std::path::PathBuf,
    work: std::path::PathBuf,
    route: Arc<FakeRoute>,
    store: Arc<FakeStore>,
    server: Option<Server>,
    url: String,
}

impl Rig {
    async fn start() -> Self {
        let data = scratch("cli");
        let work = scratch("cli-work");
        let settings = Settings {
            min_free_bytes: 0,
            retry_pause: Duration::from_millis(5),
            poll_every: Duration::from_millis(10),
            wait: Duration::from_millis(100),
            ..Settings::default()
        };
        let ctx = ContextDir::create(&data).unwrap();
        let route = Arc::new(FakeRoute::default());
        let store = Arc::new(FakeStore::default());
        let placer = Arc::new(Placer::open(&ctx, RUN, settings.clone()).unwrap());
        let shared = Shared::new(
            Client::new(route.clone(), &settings),
            store.clone(),
            placer,
            settings,
            work.clone(),
            MemoryLog::new(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = api::start(listener, shared);
        let url = format!("http://{}", server.addr());
        Self {
            data,
            work,
            route,
            store,
            server: Some(server),
            url,
        }
    }

    /// Runs the command with `args`; returns its exit code, stdout and stderr.
    async fn cli(&self, args: &[&str]) -> (i32, String, String) {
        let script = repo_root().join("images/worker-python/ham-research");
        let out = tokio::process::Command::new("python3")
            .arg(script)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HAMH_RESEARCH_URL", &self.url)
            .env("HAMH_RESEARCH_WAIT_S", "1")
            .output()
            .await
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    async fn done(mut self) {
        if let Some(s) = self.server.take() {
            s.stop().await;
        }
        remove_tree(&self.data);
        remove_tree(&self.work);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_search_prints_its_header_and_the_marked_text() {
    let rig = Rig::start().await;
    rig.route.push_ok(
        paths::SEARCH,
        &SearchResponse {
            id: "s-0001".into(),
            provider: ResearchProvider::Fixture,
            results: vec![SearchResult {
                title: "Plain objects".into(),
                url: "https://example.org/plain".into(),
                snippet: "A short snippet.".into(),
                published: None,
                score: None,
            }],
            credits: Some(1),
            request_id: "r".into(),
            usage: None,
        },
    );
    let (code, out, err) = rig
        .cli(&["search", "what counts as a plain object", "--max", "3"])
        .await;
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert!(
        out.starts_with("search s-0001 provider=fixture results=1 credits=1"),
        "{out}"
    );
    assert!(
        out.contains("untrusted text from https://example.org/plain"),
        "{out}"
    );
    assert!(out.contains("snippet: A short snippet."), "{out}");
    let sent = rig.route.calls_to(paths::SEARCH);
    assert_eq!(sent[0]["maxResults"], 3);
    rig.done().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_prints_the_path_of_the_stored_page_and_not_its_text() {
    let rig = Rig::start().await;
    let body = text(1500, "the stored page text");
    let m = put_page(&rig.store, "i-0001", "https://example.org/plain", &body);
    rig.route.push_ok(
        paths::FETCH,
        &FetchResponse {
            items: vec![item_ref(&m)],
            failed: Vec::new(),
            request_id: "r".into(),
            usage: None,
        },
    );
    let (code, out, err) = rig.cli(&["fetch", "https://example.org/plain"]).await;
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    let page = rig.data.join("context/research/pages/i-0001.txt");
    assert!(out.contains(&format!("path {}", page.display())), "{out}");
    assert!(out.contains(&m.sha256()));
    assert_eq!(std::fs::read(&page).unwrap(), body);
    assert!(!out.contains("stored page text"));
    rig.done().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_of_the_route_reaches_the_exit_code() {
    let rig = Rig::start().await;
    rig.route.push_fail(
        paths::FETCH,
        ResearchErrorCode::RefusedUnseenUrl,
        "that address was not seen in this run",
    );
    let (code, out, err) = rig
        .cli(&["fetch", "https://example.org/never-listed"])
        .await;
    assert_eq!(code, 3, "{out}");
    assert_eq!(
        err.trim(),
        "refused_unseen_url: that address was not seen in this run"
    );
    rig.done().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_that_is_not_ready_is_waited_for_with_the_second_command() {
    let rig = Rig::start().await;
    let lib = text(2500, "lib");
    let m = put_repo(
        &rig.store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("index.js", &lib)],
    );
    rig.route.push_ok(
        paths::REPO,
        &JobResponse {
            job_id: "job-9".into(),
            state: JobState::Queued,
            item: None,
            error: None,
            poll_after_ms: None,
            usage: None,
        },
    );
    rig.route.always(
        paths::JOB,
        &JobResponse {
            job_id: "job-9".into(),
            state: JobState::Running,
            item: None,
            error: None,
            poll_after_ms: None,
            usage: None,
        },
    );
    let (code, out, _) = rig
        .cli(&["repo", "https://github.com/acme/widgets", "--ref", COMMIT])
        .await;
    assert_eq!(code, 6, "{out}");
    assert_eq!(out.trim(), "pending job-9; run: ham-research wait job-9");
    let mut done = JobResponse {
        job_id: "job-9".into(),
        state: JobState::Succeeded,
        item: Some(item_ref(&m)),
        error: None,
        poll_after_ms: None,
        usage: None,
    };
    done.item.as_mut().unwrap().files = Some(1);
    rig.route.always(paths::JOB, &done);
    let (code, out, err) = rig.cli(&["wait", "job-9"]).await;
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert!(
        out.contains("item i-0002 repo https://github.com/acme/widgets @ "),
        "{out}"
    );
    let dir = rig
        .data
        .join("context/research/repos/acme__widgets__666df7c10035");
    assert!(out.contains(&format!("path {}", dir.display())), "{out}");
    assert!(dir.join("index.js").is_file());
    rig.done().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_index_and_the_check_and_verify_calls_work_end_to_end() {
    let rig = Rig::start().await;
    let lib = text(2500, "lib");
    let m = put_repo(
        &rig.store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("lib/util.js", &lib)],
    );
    rig.route.push_ok(
        paths::REPO,
        &JobResponse {
            job_id: "job-1".into(),
            state: JobState::Succeeded,
            item: Some(item_ref(&m)),
            error: None,
            poll_after_ms: None,
            usage: None,
        },
    );
    rig.cli(&["repo", "https://github.com/acme/widgets", "--ref", COMMIT])
        .await;

    let (code, out, _) = rig.cli(&["index", "--md"]).await;
    assert_eq!(code, 0);
    assert!(
        out.starts_with("| Origin | Date read | sha256 | Licence | Why it matters |"),
        "{out}"
    );
    // The first two lines are those of the example the DIRECT-RESEARCH index judge is tested with.
    let example = include_str!("../../../fixtures/research/index-md.example");
    assert_eq!(
        out.lines().take(2).collect::<Vec<_>>(),
        example.lines().take(2).collect::<Vec<_>>()
    );
    assert!(out.contains(&m.sha256()), "{out}");

    std::fs::create_dir_all(rig.work.join("src")).unwrap();
    std::fs::write(rig.work.join("src/copied.js"), &lib).unwrap();
    let (code, out, _) = rig.cli(&["check"]).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("copy src/copied.js = lib/util.js of item i-0002"),
        "{out}"
    );

    let (code, out, _) = rig.cli(&["verify"]).await;
    assert_eq!((code, out.trim()), (0, "verify ok"));
    rig.done().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_an_address_the_command_says_research_is_off() {
    let script = repo_root().join("images/worker-python/ham-research");
    let out = tokio::process::Command::new("python3")
        .arg(script)
        .arg("limits")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("research is off for this run (code disabled)")
    );
}
