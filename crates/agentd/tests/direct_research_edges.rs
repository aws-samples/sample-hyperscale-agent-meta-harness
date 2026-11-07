//! The edges of the research module: how a refusal is worded, what `verify` and `check_tree` say about the ordinary ways a
//! placed tree can differ from its record, what the loopback API answers when the route or the bucket gives it something unusual, and that
//! no debug text holds a credential. These tests drive the module through its own interfaces with the fakes of `research_support`; none
//! starts a process or opens a connection beyond the loopback server under test.
#![allow(clippy::unwrap_used)]

mod common;
mod research_support;

use agentd::BoxFuture;
use agentd::creds::CredStore;
use agentd::direct::context::ContextDir;
use agentd::direct::research::api::{self, Shared, status_and_exit};
use agentd::direct::research::client::Client;
use agentd::direct::research::context::repo_dir_name;
use agentd::direct::research::extract::{ExtractCaps, ExtractError, remove_tree};
use agentd::direct::research::place::{PlaceError, Placer};
use agentd::direct::research::route::{FailureKind, RouteFailure};
use agentd::direct::research::settings::Settings;
use agentd::direct::research::store::{ObjectStore, S3Store, StoreError};
use agentd::logging::MemoryLog;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use common::{CANARY_NONCE, CANARY_SECRET, CANARY_TOKEN, creds, scratch};
use harness_protocol::research::route as paths;
use harness_protocol::research::{
    ItemKind, JobResponse, JobState, ResearchError, ResearchErrorCode, s3,
};
use research_support::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn small() -> Settings {
    Settings {
        min_free_bytes: 0,
        retry_pause: Duration::from_millis(5),
        poll_every: Duration::from_millis(10),
        wait: Duration::from_secs(3),
        ..Settings::default()
    }
}

fn open(label: &str, settings: Settings) -> (PathBuf, ContextDir, Placer) {
    let data = scratch(label);
    let ctx = ContextDir::create(&data).unwrap();
    let placer = Placer::open(&ctx, RUN, settings).unwrap();
    (data, ctx, placer)
}

fn open_up(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A bucket that cannot be read.
struct BrokenStore;

impl ObjectStore for BrokenStore {
    fn get<'a>(&'a self, _key: &'a str) -> BoxFuture<'a, Result<Vec<u8>, StoreError>> {
        Box::pin(async { Err(StoreError::Unavailable("the bucket did not answer".into())) })
    }
}

// ---- words ------------------------------------------------------------------------------------------------------------------

#[test]
fn every_refusal_has_the_word_the_api_answers_with() {
    for (e, code) in [
        (PlaceError::BadManifest("x".into()), "integrity"),
        (PlaceError::Integrity("x".into()), "integrity"),
        (PlaceError::TooLarge("x".into()), "too_large"),
        (PlaceError::LimitContext("x".into()), "limit_context"),
        (PlaceError::NoSpace, "no_space"),
        (PlaceError::Unavailable("x".into()), "unavailable"),
    ] {
        assert_eq!(e.code(), code);
        assert!(!e.to_string().is_empty());
    }
    // The route keeps its own numbers; agentd's words have theirs; a word nobody knows is an upstream fault.
    assert_eq!(status_and_exit("integrity"), (502, 7));
    assert_eq!(status_and_exit("limit_context"), (429, 4));
    assert_eq!(status_and_exit("no_space"), (500, 5));
    assert_eq!(status_and_exit("unavailable"), (502, 5));
    assert_eq!(status_and_exit("not_a_word"), (502, 5));
    assert_eq!(
        status_and_exit("limit_calls"),
        (
            ResearchErrorCode::LimitCalls.http_status(),
            ResearchErrorCode::LimitCalls.exit_code()
        )
    );
}

#[test]
fn every_extraction_failure_has_the_word_the_api_answers_with() {
    let s = |t: &str| t.to_string();
    for (e, code) in [
        (ExtractError::BadArchive(s("x")), "integrity"),
        (ExtractError::BadPath(s("x")), "integrity"),
        (ExtractError::Refused(s("x")), "integrity"),
        (ExtractError::Unlisted(s("x")), "integrity"),
        (ExtractError::Missing(s("x")), "integrity"),
        (ExtractError::Duplicate(s("x")), "integrity"),
        (ExtractError::Integrity(s("x")), "integrity"),
        (ExtractError::TooLarge(s("x")), "too_large"),
        (ExtractError::TooMany, "too_large"),
        (ExtractError::NoSpace, "no_space"),
        (ExtractError::Exists, "unavailable"),
        (ExtractError::Io(s("x")), "unavailable"),
    ] {
        assert_eq!(e.code(), code, "{e}");
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn the_default_caps_are_the_default_settings() {
    let s = Settings::default();
    let c = ExtractCaps::default();
    assert_eq!(c.max_unpacked_bytes, s.max_unpacked_bytes);
    assert_eq!(c.max_files, s.max_files);
    assert_eq!(c.max_file_bytes, s.max_file_bytes);
    assert_eq!(c.min_free_bytes, s.min_free_bytes);
}

#[test]
fn a_directory_name_needs_a_plain_origin_and_a_commit_of_twelve_hexadecimal_characters() {
    assert_eq!(
        repo_dir_name("https://github.com/acme/widgets", None).as_deref(),
        Some("acme__widgets")
    );
    assert_eq!(
        repo_dir_name("https://github.com/acme/widgets.git/", Some(COMMIT)).as_deref(),
        Some("acme__widgets__666df7c10035")
    );
    // A commit that is too short or not hexadecimal gives no name: the item's own name is used instead.
    assert_eq!(
        repo_dir_name("https://github.com/acme/widgets", Some("abc")),
        None
    );
    assert_eq!(
        repo_dir_name("https://github.com/acme/widgets", Some("zzzzzzzzzzzzzz")),
        None
    );
    assert_eq!(
        repo_dir_name("https://example.org/acme/widgets", None),
        None
    );
}

// ---- what a manifest may say --------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_manifest_that_names_another_run_or_another_item_or_repeats_a_path_is_refused() {
    let body = text(1200, "p");
    let other_run = "01K5ZQ4T8M3N6P7R9S0V1W2X3Z";
    // Another run: the body key is under that run too, so the manifest is well formed and simply not this run's.
    let mut m = manifest(
        "i-0001",
        ItemKind::Page,
        "https://docs.example.org/g",
        "body.txt",
        &body,
    );
    m.run = other_run.into();
    m.body.key = format!("{}items/i-0001/body.txt", s3::prefix(other_run));
    let store = FakeStore::default();
    store.put("items/i-0001/manifest.json", m.canonical_bytes());
    let (data, _ctx, placer) = open("ed-run", small());
    let err = placer.place(&store, "i-0001").await.unwrap_err();
    assert!(matches!(err, PlaceError::BadManifest(_)), "{err:?}");
    assert!(err.to_string().contains("another run"), "{err}");
    remove_tree(&data);

    // Another item: the manifest under i-0001 says it is i-0002.
    let mut m = manifest(
        "i-0002",
        ItemKind::Page,
        "https://docs.example.org/g",
        "body.txt",
        &body,
    );
    m.body.key = format!("{}items/i-0002/body.txt", s3::prefix(RUN));
    let store = FakeStore::default();
    store.put("items/i-0001/manifest.json", m.canonical_bytes());
    let (data, _ctx, placer) = open("ed-item", small());
    let err = placer.place(&store, "i-0001").await.unwrap_err();
    assert!(err.to_string().contains("another item"), "{err}");
    remove_tree(&data);

    // A file list that names one path twice.
    let a = text(1200, "a");
    let archive = targz(&[("a.js", &a)]);
    let mut m = manifest(
        "i-0003",
        ItemKind::Repo,
        "https://github.com/acme/widgets",
        "body.tar.gz",
        &archive,
    );
    m.commit = Some(COMMIT.into());
    let entry = harness_protocol::research::ManifestFile {
        path: "a.js".into(),
        sha256: sha(&a),
        bytes: a.len() as u64,
    };
    m.files = vec![entry.clone(), entry];
    let store = FakeStore::default();
    store.put("items/i-0003/manifest.json", m.canonical_bytes());
    store.put("items/i-0003/body.tar.gz", archive);
    let (data, _ctx, placer) = open("ed-twice", small());
    let err = placer.place(&store, "i-0003").await.unwrap_err();
    assert!(matches!(err, PlaceError::BadManifest(_)), "{err:?}");
    assert!(err.to_string().contains("repeats a path"), "{err}");
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manifest_over_eight_mebibytes_is_refused_and_a_bucket_that_does_not_answer_is_unavailable()
 {
    let store = FakeStore::default();
    store.put(
        "items/i-0001/manifest.json",
        vec![b' '; 8 * 1024 * 1024 + 1],
    );
    let (data, _ctx, placer) = open("ed-big", small());
    let err = placer.place(&store, "i-0001").await.unwrap_err();
    assert!(matches!(err, PlaceError::TooLarge(_)), "{err:?}");
    assert_eq!(err.code(), "too_large");

    let err = placer.place(&BrokenStore, "i-0001").await.unwrap_err();
    assert!(matches!(err, PlaceError::Unavailable(_)), "{err:?}");
    assert!(
        err.to_string().contains("the bucket did not answer"),
        "{err}"
    );
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_that_does_not_fit_the_free_space_is_refused_with_its_own_word() {
    let store = FakeStore::default();
    let lib = text(2500, "lib");
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("index.js", &lib)],
    );
    let settings = Settings {
        min_free_bytes: u64::MAX / 2,
        ..small()
    };
    let (data, ctx, placer) = open("ed-space", settings);
    let err = placer.place(&store, "i-0002").await.unwrap_err();
    assert_eq!(err, PlaceError::NoSpace);
    assert_eq!(err.code(), "no_space");
    assert!(placer.items().is_empty());
    assert_eq!(placer.used_bytes(), 0);
    assert_eq!(
        std::fs::read_dir(ctx.path().join("research/repos"))
            .unwrap()
            .count(),
        0,
        "nothing is left in repos/"
    );
    remove_tree(&data);
}

// ---- placing that cannot finish -------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_with_no_free_name_is_refused_and_its_unpack_removed() {
    let store = FakeStore::default();
    let lib = text(2500, "lib");
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("index.js", &lib)],
    );
    let (data, ctx, placer) = open("ed-names", small());
    let repos = ctx.path().join("research/repos");
    let name = "acme__widgets__666df7c10035";
    let mut taken = vec![name.to_string(), format!("{name}-i-0002")];
    taken.extend((1..100).map(|n| format!("{name}-i-0002-{n}")));
    for t in &taken {
        std::fs::create_dir(repos.join(t)).unwrap();
    }
    let err = placer.place(&store, "i-0002").await.unwrap_err();
    assert!(matches!(err, PlaceError::Unavailable(_)), "{err:?}");
    assert!(placer.items().is_empty());
    assert_eq!(placer.used_bytes(), 0);
    assert_eq!(
        std::fs::read_dir(&repos).unwrap().count(),
        taken.len(),
        "only the directories that were already there"
    );
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manifest_copy_that_cannot_be_written_undoes_a_page_and_a_repository() {
    let store = FakeStore::default();
    let lib = text(2500, "lib");
    put_page(
        &store,
        "i-0001",
        "https://docs.example.org/g",
        &text(1500, "p"),
    );
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("index.js", &lib)],
    );
    let (data, ctx, placer) = open("ed-copy", small());
    // A directory where each manifest copy belongs: the copy cannot be put there.
    for id in ["i-0001", "i-0002"] {
        let blocker = ctx.path().join(format!("research/manifests/{id}.json"));
        std::fs::create_dir(&blocker).unwrap();
        std::fs::write(blocker.join("occupied"), b"x").unwrap();
    }
    for id in ["i-0001", "i-0002"] {
        let err = placer.place(&store, id).await.unwrap_err();
        assert!(matches!(err, PlaceError::Unavailable(_)), "{id}: {err:?}");
    }
    assert!(placer.items().is_empty());
    assert_eq!(placer.used_bytes(), 0);
    assert!(!ctx.path().join("research/pages/i-0001.txt").exists());
    assert_eq!(
        std::fs::read_dir(ctx.path().join("research/repos"))
            .unwrap()
            .count(),
        0,
        "the renamed unpack was removed again"
    );
    remove_tree(&data);
}

// ---- verify and check_tree ----------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn verify_names_each_way_the_record_and_the_disk_can_differ() {
    let store = FakeStore::default();
    let page_body = text(2000, "p");
    put_page(&store, "i-0001", "https://docs.example.org/g", &page_body);
    put_page(
        &store,
        "i-0003",
        "https://docs.example.org/h",
        &text(2000, "h"),
    );
    put_page(
        &store,
        "i-0004",
        "https://docs.example.org/i",
        &text(2000, "i"),
    );
    put_page(
        &store,
        "i-0005",
        "https://docs.example.org/j",
        &text(2000, "j"),
    );
    let lib = text(2500, "lib");
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("index.js", &lib)],
    );
    let (data, ctx, placer) = open("ed-verify", small());
    for id in ["i-0001", "i-0002", "i-0003", "i-0004", "i-0005"] {
        placer.place(&store, id).await.unwrap();
    }
    assert!(placer.verify(&store).await.is_empty());
    let root = ctx.path().join("research");
    let what = |problems: &[agentd::direct::research::place::Problem], id: &str| -> Vec<String> {
        problems
            .iter()
            .filter(|p| p.item_id == id)
            .map(|p| format!("{}: {}", p.path, p.what))
            .collect()
    };

    // i-0001: the page changed. i-0003: the page is gone. i-0004: the local copy of the manifest is gone.
    // i-0005: the local copy of the manifest is not a manifest.
    open_up(&root.join("pages"), 0o755);
    open_up(&root.join("pages/i-0001.txt"), 0o644);
    std::fs::write(root.join("pages/i-0001.txt"), b"edited").unwrap();
    std::fs::remove_file(root.join("pages/i-0003.txt")).unwrap();
    open_up(&root.join("manifests"), 0o755);
    std::fs::remove_file(root.join("manifests/i-0004.json")).unwrap();
    open_up(&root.join("manifests/i-0005.json"), 0o644);
    std::fs::write(root.join("manifests/i-0005.json"), b"{not json").unwrap();
    // i-0002: a file the manifest does not list.
    let repo_dir = placer
        .items()
        .into_iter()
        .find(|i| i.item_id == "i-0002")
        .unwrap()
        .path;
    open_up(Path::new(&repo_dir), 0o755);
    std::fs::write(Path::new(&repo_dir).join("extra.txt"), b"unlisted").unwrap();
    // The route lost the manifest of i-0003's neighbour, and cannot be read for another.
    store
        .objects
        .lock()
        .unwrap()
        .remove(&agentd::direct::research::store::research_key(
            RUN,
            "items/i-0004/manifest.json",
        ));

    let problems = placer.verify(&store).await;
    assert!(
        what(&problems, "i-0001")
            .iter()
            .any(|w| w.contains("the page changed")),
        "{problems:?}"
    );
    assert!(
        what(&problems, "i-0003")
            .iter()
            .any(|w| w.contains("the page is gone")),
        "{problems:?}"
    );
    assert!(
        what(&problems, "i-0004")
            .iter()
            .any(|w| w.contains("the route no longer has the manifest")),
        "{problems:?}"
    );
    assert!(
        what(&problems, "i-0004")
            .iter()
            .any(|w| w.contains("the local copy of the manifest is gone")),
        "{problems:?}"
    );
    assert!(
        what(&problems, "i-0005")
            .iter()
            .any(|w| w.contains("does not parse")),
        "{problems:?}"
    );
    assert!(
        what(&problems, "i-0002")
            .iter()
            .any(|w| w.contains("extra.txt") && w.contains("not in the manifest")),
        "{problems:?}"
    );
    // A bucket that cannot be read says so for every item, and still checks the disk.
    let unread = placer.verify(&BrokenStore).await;
    assert!(
        unread
            .iter()
            .any(|p| p.what.contains("could not be read") && p.item_id == "i-0001"),
        "{unread:?}"
    );
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn check_tree_needs_a_directory_inside_the_work_tree_and_says_nothing_when_nothing_is_stored()
{
    let (data, _ctx, placer) = open("ed-check", small());
    let work = scratch("ed-check-work");
    std::fs::write(work.join("a.txt"), text(2000, "a")).unwrap();
    // Nothing is stored: an empty report, whatever the tree holds.
    let report = placer.check_tree(&work, &work).unwrap();
    assert!(report.matches.is_empty());
    assert_eq!(report.scanned_files, 0);
    // A directory that does not exist, and one outside the work tree, are refused.
    assert!(placer.check_tree(&work, &work.join("missing")).is_err());
    let elsewhere = scratch("ed-check-else");
    let err = placer.check_tree(&work, &elsewhere).unwrap_err();
    assert!(err.contains("outside the work tree"), "{err}");
    assert!(placer.check_tree(&work.join("missing"), &work).is_err());
    remove_tree(&data);
    remove_tree(&work);
    remove_tree(&elsewhere);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stage_left_by_an_earlier_agentd_is_removed_when_the_research_directory_is_opened() {
    let data = scratch("ed-stage");
    let ctx = ContextDir::create(&data).unwrap();
    let first = Placer::open(&ctx, RUN, small()).unwrap();
    drop(first);
    let repos = ctx.path().join("research/repos");
    std::fs::create_dir_all(repos.join(".stage-i-0001-0/sub")).unwrap();
    std::fs::write(repos.join(".stage-i-0001-0/sub/f.txt"), b"half an unpack").unwrap();
    std::fs::write(repos.join(".stage-i-0002-0"), b"a file where an unpack was").unwrap();
    std::fs::create_dir(repos.join("kept__repo")).unwrap();
    let _again = Placer::open(&ctx, RUN, small()).unwrap();
    let left: Vec<String> = std::fs::read_dir(&repos)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["kept__repo"]);
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn debug_text_of_the_module_holds_no_credential() {
    let (data, ctx, placer) = open("ed-debug", small());
    let settings = small();
    let route = Arc::new(FakeRoute::default());
    let shared = Shared::new(
        Client::new(route, &settings),
        Arc::new(FakeStore::default()),
        Arc::new(Placer::open(&ctx, RUN, settings.clone()).unwrap()),
        settings.clone(),
        data.clone(),
        MemoryLog::new(),
    );
    let store = CredStore::new(
        Arc::new(agentd::creds::HttpRefresher::new()),
        Arc::new(agentd::time::SystemClock),
    );
    let s3store = S3Store::new(store, "us-east-1".into(), None, "bucket".into());
    let text = format!("{placer:?} {shared:?} {s3store:?}");
    assert!(text.contains("Placer") && text.contains("Shared") && text.contains("S3Store"));
    for secret in [CANARY_SECRET, CANARY_TOKEN] {
        assert!(!text.contains(secret));
    }
    remove_tree(&data);
}

// ---- the bucket over signed HTTP ----------------------------------------------------------------------------------------------

type Seen = Arc<Mutex<Vec<(String, HeaderMap)>>>;

async fn bucket_handler(
    State(seen): State<Seen>,
    uri: Uri,
    headers: HeaderMap,
) -> impl IntoResponse {
    seen.lock().unwrap().push((uri.path().to_string(), headers));
    match uri.path() {
        "/the-bucket/runs/r/items/i-1/body.txt" => (StatusCode::OK, "the body".to_string()),
        "/the-bucket/runs/r/items/i-2/body.txt" => (StatusCode::NOT_FOUND, String::new()),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "<Error><Code>InternalError</Code></Error>".to_string(),
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bucket_is_read_with_a_signed_get_and_its_answers_are_told_apart() {
    let seen: Seen = Arc::default();
    let app = Router::new()
        .fallback(bucket_handler)
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let store = CredStore::new(
        Arc::new(agentd::creds::HttpRefresher::new()),
        Arc::new(agentd::time::SystemClock),
    );
    store
        .install(
            agentd::creds::RefreshContext {
                run_id: "r".into(),
                agent_id: "direct".into(),
                runtime_session_id: "x".repeat(40),
                internal_api_url: format!("http://{addr}"),
                region: "us-east-1".into(),
            },
            creds(&agentd::time::format_rfc3339(
                agentd::time::now_unix() + 3600,
            )),
            CANARY_NONCE.into(),
        )
        .await;
    let bucket = S3Store::new(
        store,
        "us-east-1".into(),
        Some(format!("http://{addr}")),
        "the-bucket".into(),
    );
    assert_eq!(
        bucket.get("runs/r/items/i-1/body.txt").await.unwrap(),
        b"the body"
    );
    assert_eq!(
        bucket.get("runs/r/items/i-2/body.txt").await.unwrap_err(),
        StoreError::Missing
    );
    let err = bucket.get("runs/r/items/i-3/body.txt").await.unwrap_err();
    assert!(matches!(err, StoreError::Unavailable(_)), "{err:?}");
    let g = seen.lock().unwrap();
    let auth = g[0].1["authorization"].to_str().unwrap();
    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential="), "{auth}");
    assert!(auth.contains("/us-east-1/s3/aws4_request"), "{auth}");
    assert_eq!(g[0].1["x-amz-security-token"], CANARY_TOKEN);
    assert!(!auth.contains(CANARY_SECRET));
}

// ---- the API: what the route and the bucket can give it ----------------------------------------------------------------------------

struct Rig {
    data: PathBuf,
    work: PathBuf,
    route: Arc<FakeRoute>,
    store: Arc<FakeStore>,
    log: Arc<MemoryLog>,
    server: api::Server,
}

impl Rig {
    async fn start(label: &str) -> Self {
        let data = scratch(label);
        let work = scratch(&format!("{label}-work"));
        let ctx = ContextDir::create(&data).unwrap();
        let settings = small();
        let route = Arc::new(FakeRoute::default());
        let store = Arc::new(FakeStore::default());
        let log = MemoryLog::new();
        let shared = Shared::new(
            Client::new(route.clone(), &settings),
            store.clone(),
            Arc::new(Placer::open(&ctx, RUN, settings.clone()).unwrap()),
            settings,
            work.clone(),
            log.clone(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = api::start(listener, shared);
        Self {
            data,
            work,
            route,
            store,
            log,
            server,
        }
    }

    async fn done(self) {
        self.server.stop().await;
        remove_tree(&self.data);
        remove_tree(&self.work);
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

#[tokio::test(flavor = "multi_thread")]
async fn an_error_of_the_route_keeps_its_hint_and_its_request_id_and_its_exit_code() {
    let rig = Rig::start("ed-error").await;
    let mut e = ResearchError::new(ResearchErrorCode::UpstreamError, "the provider failed");
    e.retry_after_s = Some(7);
    e.request_id = Some("req-9".into());
    rig.route.push(
        paths::SEARCH,
        research_support::Reply::Fail(RouteFailure {
            error: e,
            kind: FailureKind::Final,
        }),
    );
    let (status, v) = post_json(rig.server.addr(), "/v1/search", &json!({"query": "q"})).await;
    assert_eq!(status, ResearchErrorCode::UpstreamError.http_status());
    assert_eq!(v["error"]["retryAfterS"], 7);
    assert_eq!(v["error"]["requestId"], "req-9");
    assert_eq!(
        v["error"]["exit"],
        ResearchErrorCode::UpstreamError.exit_code()
    );
    rig.done().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_call_that_is_not_an_object_or_fails_or_ends_without_an_item_says_so() {
    let rig = Rig::start("ed-repo").await;
    let addr = rig.server.addr();
    let (status, v) = post_json(addr, "/v1/repo", &json!([1, 2])).await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("JSON object"),
        "{v}"
    );

    // The route refuses the call.
    rig.route.push_fail(
        paths::REPO,
        ResearchErrorCode::RefusedUrl,
        "that address is not allowed",
    );
    let (status, v) = post_json(
        addr,
        "/v1/repo",
        &json!({"url": "https://github.com/acme/widgets", "ref": "main"}),
    )
    .await;
    assert_eq!(status, ResearchErrorCode::RefusedUrl.http_status(), "{v}");
    assert_eq!(
        v["error"]["exit"],
        ResearchErrorCode::RefusedUrl.exit_code()
    );

    // A finished job that names no item.
    rig.route
        .push_ok(paths::REPO, &job(JobState::Succeeded, "j-1"));
    let (status, v) = post_json(
        addr,
        "/v1/repo",
        &json!({"url": "https://github.com/acme/widgets", "ref": "v1"}),
    )
    .await;
    assert_eq!(
        status,
        ResearchErrorCode::UpstreamError.http_status(),
        "{v}"
    );
    assert_eq!(v["error"]["code"], "upstream_error");

    // A finished job whose item is not in the bucket: the placement fails and is counted.
    let m =
        {
            let lib = text(2500, "lib");
            let m = put_repo(
                &rig.store,
                "i-0002",
                "https://github.com/acme/widgets",
                &[("index.js", &lib)],
            );
            rig.store.objects.lock().unwrap().remove(
                &agentd::direct::research::store::research_key(RUN, "items/i-0002/body.tar.gz"),
            );
            m
        };
    let mut done = job(JobState::Succeeded, "j-2");
    done.item = Some(item_ref(&m));
    rig.route.push_ok(paths::REPO, &done);
    let (status, v) = post_json(
        addr,
        "/v1/repo",
        &json!({"url": "https://github.com/acme/widgets", "ref": "dev"}),
    )
    .await;
    assert_eq!(status, 502, "{v}");
    assert_eq!(v["error"]["code"], "unavailable");
    assert!(
        rig.log
            .lines()
            .iter()
            .any(|l| l.contains("not placed code=unavailable")),
        "{:?}",
        rig.log.lines()
    );
    rig.done().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_call_checks_its_body_and_reports_a_job_the_route_cannot_find() {
    let rig = Rig::start("ed-wait").await;
    let addr = rig.server.addr();
    let (status, _) = post_json(addr, "/v1/wait", &json!({"nope": 1})).await;
    assert_eq!(status, 400);
    let (status, _) = post_json(addr, "/v1/wait", &json!({"jobId": ""})).await;
    assert_eq!(status, 400);
    // The route answers the first poll with a failure.
    rig.route
        .push_fail(paths::JOB, ResearchErrorCode::NotFound, "no such job");
    let (status, v) = post_json(addr, "/v1/wait", &json!({"jobId": "j-9"})).await;
    assert_eq!(status, ResearchErrorCode::NotFound.http_status(), "{v}");
    // A job that is still running when the wait ends: 202, and a timeout above the setting is cut to it.
    rig.route.always(paths::JOB, &job(JobState::Running, "j-9"));
    let started = std::time::Instant::now();
    let (status, v) = post_json(addr, "/v1/wait", &json!({"jobId": "j-9", "timeoutS": 3600})).await;
    assert_eq!(status, 202, "{v}");
    assert!(started.elapsed() < Duration::from_secs(10));
    // The route fails while the job is being followed.
    rig.route
        .push_ok(paths::JOB, &job(JobState::Running, "j-8"));
    rig.route.push_fail(
        paths::JOB,
        ResearchErrorCode::UpstreamError,
        "the provider failed",
    );
    let (status, v) = post_json(addr, "/v1/wait", &json!({"jobId": "j-8"})).await;
    assert_eq!(
        status,
        ResearchErrorCode::UpstreamError.http_status(),
        "{v}"
    );
    rig.done().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_call_with_a_body_that_is_not_its_request_or_a_directory_outside_the_tree_is_a_usage_error()
 {
    let rig = Rig::start("ed-check-api").await;
    let addr = rig.server.addr();
    let (status, v) = post_json(addr, "/v1/check", &json!({"dir": 5})).await;
    assert_eq!(status, 400, "{v}");
    let outside = scratch("ed-check-api-else");
    let (status, v) = post_json(
        addr,
        "/v1/check",
        &json!({"dir": outside.display().to_string()}),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside the work tree"),
        "{v}"
    );
    // An empty body is the whole work tree.
    let (status, text) = http(addr, "POST", "/v1/check", None).await;
    assert_eq!(status, 200, "{text}");
    remove_tree(&outside);
    rig.done().await;
}
