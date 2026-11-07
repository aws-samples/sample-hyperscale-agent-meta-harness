//! Review fixes of the research module: a directory name that is taken is never cleaned
//! away by a placement that failed, a placement whose index file cannot be written still stands and the next call repairs the index, an
//! archive is capped in members as well as in files and is read to its end, a cap that is extreme does not overflow, a placement goes on
//! when the caller hangs up, a body that is not the request is not echoed back, and a listener that is not on the loopback address is
//! not served.
#![allow(clippy::unwrap_used)]

mod common;
mod research_support;

use agentd::direct::context::ContextDir;
use agentd::direct::research::api;
use agentd::direct::research::client::Client;
use agentd::direct::research::extract::{
    ExtractCaps, ExtractError, WantedFile, extract_targz, remove_tree,
};
use agentd::direct::research::place::Placer;
use agentd::direct::research::settings::Settings;
use agentd::logging::MemoryLog;
use common::scratch;
use harness_protocol::research::FetchResponse;
use harness_protocol::research::route as paths;
use research_support::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

fn small() -> Settings {
    Settings {
        min_free_bytes: 0,
        ..Settings::default()
    }
}

// ---- names that are taken ---------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_that_already_has_the_name_is_left_as_it_is_and_the_item_takes_another_name() {
    let data = scratch("rv-names");
    let ctx = ContextDir::create(&data).unwrap();
    let placer = Placer::open(&ctx, RUN, small()).unwrap();
    let repos = ctx.path().join("research/repos");
    let first = repos.join("acme__widgets__666df7c10035");
    let second = repos.join("acme__widgets__666df7c10035-i-0002");
    for d in [&first, &second] {
        std::fs::create_dir(d).unwrap();
        std::fs::write(d.join("keep.txt"), b"another item's file").unwrap();
    }
    let store = FakeStore::default();
    let lib = text(2500, "lib");
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("index.js", &lib)],
    );
    let placed = placer.place(&store, "i-0002").await.unwrap();
    assert!(placed.new);
    assert!(
        placed
            .item
            .path
            .contains("acme__widgets__666df7c10035-i-0002-"),
        "{}",
        placed.item.path
    );
    assert_eq!(
        std::fs::read(std::path::Path::new(&placed.item.path).join("index.js")).unwrap(),
        lib
    );
    for d in [&first, &second] {
        assert_eq!(
            std::fs::read(d.join("keep.txt")).unwrap(),
            b"another item's file",
            "{} was touched",
            d.display()
        );
    }
    remove_tree(&data);
}

// ---- the index --------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_placement_whose_index_cannot_be_written_still_stands_and_the_next_call_repairs_the_index()
 {
    let data = scratch("rv-index");
    let ctx = ContextDir::create(&data).unwrap();
    let placer = Placer::open(&ctx, RUN, small()).unwrap();
    let store = FakeStore::default();
    put_page(
        &store,
        "i-0001",
        "https://docs.example.org/a",
        &text(1500, "p"),
    );
    // A directory where the index file belongs: the file cannot be put there, for any user.
    let blocker = ctx.path().join("research/INDEX.json");
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(blocker.join("occupied"), b"x").unwrap();

    let placed = placer.place(&store, "i-0001").await.unwrap();
    assert!(
        placed.new,
        "the item is placed although the index could not be written"
    );
    assert!(placer.index_dirty());
    assert_eq!(placer.items().len(), 1);
    assert!(ctx.path().join("research/pages/i-0001.txt").is_file());

    std::fs::remove_dir_all(&blocker).unwrap();
    let again = placer.place(&store, "i-0001").await.unwrap();
    assert!(!again.new);
    assert!(!placer.index_dirty());
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&blocker).unwrap()).unwrap();
    assert_eq!(json["items"][0]["item_id"], "i-0001");
    assert_eq!(
        std::fs::read_to_string(ctx.path().join("research/INDEX.md")).unwrap(),
        placer.index_md()
    );
    remove_tree(&data);
}

// ---- the member count -------------------------------------------------------------------------------------------------------

#[test]
fn an_archive_with_more_members_than_the_cap_is_refused_even_when_they_are_directories() {
    let body = text(1200, "a");
    let mut b = tar::Builder::new(Vec::new());
    for i in 0..60 {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_size(0);
        h.set_mode(0o755);
        h.set_mtime(0);
        h.set_cksum();
        b.append_data(&mut h, format!("d{i:02}/"), std::io::empty())
            .unwrap();
    }
    let mut h = tar::Header::new_gnu();
    h.set_size(body.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(0);
    h.set_cksum();
    b.append_data(&mut h, "a.txt", body.as_slice()).unwrap();
    let tar = b.into_inner().unwrap();
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut enc, &tar).unwrap();
    let archive = enc.finish().unwrap();
    let mut wanted = BTreeMap::new();
    wanted.insert(
        "a.txt".to_string(),
        WantedFile {
            sha256: sha(&body),
            bytes: body.len() as u64,
        },
    );
    let parent = scratch("rv-members");
    let dest = parent.join("repo");
    let caps = ExtractCaps {
        max_unpacked_bytes: 1024 * 1024,
        max_files: 10,
        max_file_bytes: 256 * 1024,
        min_free_bytes: 0,
    };
    let err = extract_targz(&archive, &dest, &wanted, &caps).unwrap_err();
    assert!(matches!(err, ExtractError::TooMany), "{err:?}");
    assert!(!dest.exists());
    remove_tree(&parent);
}

// ---- an archive is read to its end, and an extreme cap does not overflow ------------------------------------------------------

fn one_file_archive(body: &[u8], tail_zeros: usize) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(body.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(0);
    h.set_cksum();
    b.append_data(&mut h, "a.txt", body).unwrap();
    let mut tar = b.into_inner().unwrap();
    tar.extend(std::iter::repeat_n(0u8, tail_zeros));
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut enc, &tar).unwrap();
    enc.finish().unwrap()
}

fn wanted_one(body: &[u8]) -> BTreeMap<String, WantedFile> {
    let mut wanted = BTreeMap::new();
    wanted.insert(
        "a.txt".to_string(),
        WantedFile {
            sha256: sha(body),
            bytes: body.len() as u64,
        },
    );
    wanted
}

#[test]
fn data_after_the_end_of_the_tar_counts_against_the_cap_on_the_decoded_stream() {
    let body = text(1200, "a");
    let caps = ExtractCaps {
        max_unpacked_bytes: 64 * 1024,
        max_files: 10,
        max_file_bytes: 64 * 1024,
        min_free_bytes: 0,
    };
    let parent = scratch("rv-tail");
    // A tar written by a tool is padded with zeros to a whole record; a few KiB of them are fine.
    let padded = one_file_archive(&body, 8 * 1024);
    let ok = extract_targz(&padded, &parent.join("ok"), &wanted_one(&body), &caps);
    assert!(ok.is_ok(), "{ok:?}");
    // Megabytes after the end marker are a decoded stream far past what the manifest lists.
    let long = one_file_archive(&body, 3 * 1024 * 1024);
    let err = extract_targz(&long, &parent.join("long"), &wanted_one(&body), &caps).unwrap_err();
    assert!(matches!(err, ExtractError::TooLarge(_)), "{err:?}");
    assert!(!parent.join("long").exists());
    remove_tree(&parent);
}

#[test]
fn caps_that_are_as_large_as_a_number_can_be_do_not_overflow() {
    let body = text(1200, "a");
    let caps = ExtractCaps {
        max_unpacked_bytes: u64::MAX,
        max_files: u32::MAX,
        max_file_bytes: u64::MAX,
        min_free_bytes: 0,
    };
    let parent = scratch("rv-huge");
    let done = extract_targz(
        &one_file_archive(&body, 0),
        &parent.join("repo"),
        &wanted_one(&body),
        &caps,
    );
    assert!(done.is_ok(), "{done:?}");
    remove_tree(&parent);
}

// ---- the API ---------------------------------------------------------------------------------------------------------------

struct Rig {
    data: std::path::PathBuf,
    work: std::path::PathBuf,
    route: Arc<FakeRoute>,
    store: Arc<FakeStore>,
    placer: Arc<Placer>,
    ctx_path: std::path::PathBuf,
    server: api::Server,
}

impl Rig {
    async fn start(name: &str) -> Self {
        let data = scratch(name);
        let work = scratch(&format!("{name}-work"));
        let ctx = ContextDir::create(&data).unwrap();
        let settings = small();
        let route = Arc::new(FakeRoute::default());
        let store = Arc::new(FakeStore::default());
        let placer = Arc::new(Placer::open(&ctx, RUN, settings.clone()).unwrap());
        let shared = api::Shared::new(
            Client::new(route.clone(), &settings),
            store.clone(),
            placer.clone(),
            settings,
            work.clone(),
            MemoryLog::new(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = api::start(listener, shared);
        Self {
            data,
            work,
            route,
            store,
            placer,
            ctx_path: ctx.path().to_path_buf(),
            server,
        }
    }

    async fn done(self) {
        self.server.stop().await;
        remove_tree(&self.data);
        remove_tree(&self.work);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_placement_goes_on_when_the_caller_hangs_up() {
    let rig = Rig::start("rv-detach").await;
    let lib = text(2500, "lib");
    let m = put_repo(
        &rig.store,
        "i-0002",
        "https://github.com/acme/widgets",
        &[("index.js", &lib)],
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
    *rig.store.pause.lock().unwrap() = Duration::from_millis(600);
    let body = json!({"urls": ["https://example.org/a"]}).to_string();
    let request = format!(
        "POST /v1/fetch HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut caller = tokio::net::TcpStream::connect(rig.server.addr())
        .await
        .unwrap();
    caller.write_all(request.as_bytes()).await.unwrap();
    // Long enough for the request to be read and the first read of the bucket to begin; short of the end of the placement.
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(caller);
    let started = Instant::now();
    while rig.placer.items().is_empty() && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        rig.placer.items().len(),
        1,
        "the placement was dropped with the caller"
    );
    let stages = std::fs::read_dir(rig.ctx_path.join("research/repos"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".stage-"))
        .count();
    assert_eq!(stages, 0, "an unpack was left behind");
    rig.done().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_the_request_is_not_echoed_back() {
    let rig = Rig::start("rv-echo").await;
    let marker = "zq-unique-marker-4471";
    for (path, body) in [
        ("/v1/search", format!(r#"{{"query":"q","{marker}":1}}"#)),
        ("/v1/fetch", format!(r#"{{"urls":"{marker}"}}"#)),
        ("/v1/repo", format!(r#"{{"url":"x","{marker}":1}}"#)),
    ] {
        let (status, text) = http(rig.server.addr(), "POST", path, Some(&body)).await;
        assert!((400..500).contains(&status), "{path}: {status} {text}");
        assert!(!text.contains(marker), "{path} echoes the request: {text}");
    }
    rig.done().await;
}

// ---- the listener -----------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_listener_that_is_not_on_the_loopback_address_is_not_served() {
    let data = scratch("rv-listen");
    let work = scratch("rv-listen-work");
    let ctx = ContextDir::create(&data).unwrap();
    let settings = small();
    let shared = api::Shared::new(
        Client::new(Arc::new(FakeRoute::default()), &settings),
        Arc::new(FakeStore::default()),
        Arc::new(Placer::open(&ctx, RUN, settings.clone()).unwrap()),
        settings,
        work.clone(),
        MemoryLog::new(),
    );
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = api::start(listener, shared);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let loopback: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    assert!(
        tokio::net::TcpStream::connect(loopback).await.is_err(),
        "the server answered on a wildcard address"
    );
    server.stop().await;
    remove_tree(&data);
    remove_tree(&work);
}
