//! Placing what the route stored into the read-only context directory: agentd
//! reads an item's manifest and body from the run's bucket with the session credentials, checks every hash against the manifest, keeps
//! the size caps per item and per run, places the material under `<agent data>/context/research/` where the engine can read it and
//! nothing can change it, and keeps the index (origin, date, licence, SHA-256 of each item). The bucket is a fake here.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::direct::context::ContextDir;
use agentd::direct::research::context::{ResearchDir, repo_dir_name, valid_item_id};
use agentd::direct::research::extract::remove_tree;
use agentd::direct::research::index::{Item, render_md};
use agentd::direct::research::place::{PlaceError, Placer};
use agentd::direct::research::settings::Settings;
use agentd::direct::research::store::{ObjectStore, StoreError, research_key};
use common::scratch;
use flate2::Compression;
use flate2::write::GzEncoder;
use harness_protocol::research::{
    BodyRef, ItemKind, ItemManifest, ItemRef, LicenceEvidence, ManifestFile, ResearchProvider, s3,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RUN: &str = "01M4XTESTRNDRCT00000000001";
const COMMIT: &str = "666df7c10035f7e26f27ec214fe5ae3173435f34";

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn text(n: usize, tag: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while out.len() < n {
        out.extend_from_slice(format!("{tag} line {i:04}\n").as_bytes());
        i += 1;
    }
    out.truncate(out.iter().rposition(|b| *b == b'\n').map_or(n, |p| p + 1));
    out
}

/// A bucket that counts what is read from it.
#[derive(Default)]
struct FakeStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    gets: Mutex<Vec<String>>,
    pause: Mutex<Duration>,
}

impl FakeStore {
    fn put(&self, rel: &str, bytes: Vec<u8>) {
        self.objects
            .lock()
            .unwrap()
            .insert(research_key(RUN, rel), bytes);
    }

    fn get_count(&self, rel_suffix: &str) -> usize {
        self.gets
            .lock()
            .unwrap()
            .iter()
            .filter(|k| k.ends_with(rel_suffix))
            .count()
    }
}

impl ObjectStore for FakeStore {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Vec<u8>, StoreError>> {
        Box::pin(async move {
            self.gets.lock().unwrap().push(key.to_string());
            let pause = *self.pause.lock().unwrap();
            if !pause.is_zero() {
                tokio::time::sleep(pause).await;
            }
            self.objects
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or(StoreError::Missing)
        })
    }
}

fn manifest(id: &str, kind: ItemKind, origin: &str, body_name: &str, body: &[u8]) -> ItemManifest {
    ItemManifest {
        schema: "hamh-research-item/1".into(),
        item_id: id.into(),
        kind,
        run: RUN.into(),
        agent: "direct".into(),
        provider: match kind {
            ItemKind::Page => ResearchProvider::Tavily,
            ItemKind::Repo => ResearchProvider::GithubArchive,
            ItemKind::Attachment => ResearchProvider::User,
        },
        request_id: "req-1".into(),
        origin: origin.into(),
        commit: None,
        git_ref: None,
        retrieved_at: "2026-10-16T14:30:02Z".into(),
        licence: LicenceEvidence {
            spdx: "MIT".into(),
            source: "file:license".into(),
            evidence_sha256: None,
        },
        body: BodyRef {
            key: format!("{}items/{id}/{body_name}", s3::prefix(RUN)),
            sha256: sha(body),
            bytes: body.len() as u64,
        },
        source_sha256: None,
        files: Vec::new(),
        dropped: Vec::new(),
        caps: None,
        truncated: false,
    }
}

fn put_manifest(store: &FakeStore, m: &ItemManifest) -> Vec<u8> {
    let bytes = m.canonical_bytes();
    store.put(&format!("items/{}/manifest.json", m.item_id), bytes.clone());
    bytes
}

/// Stores a page item; returns the manifest bytes.
fn put_page(store: &FakeStore, id: &str, origin: &str, body: &[u8]) -> Vec<u8> {
    let m = manifest(id, ItemKind::Page, origin, "body.txt", body);
    store.put(&format!("items/{id}/body.txt"), body.to_vec());
    put_manifest(store, &m)
}

fn targz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (p, body) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(0);
        h.set_cksum();
        b.append_data(&mut h, p, *body).unwrap();
    }
    let tar = b.into_inner().unwrap();
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&tar).unwrap();
    enc.finish().unwrap()
}

fn repo_manifest(
    id: &str,
    origin: &str,
    commit: &str,
    files: &[(&str, &[u8])],
) -> (ItemManifest, Vec<u8>) {
    let archive = targz(files);
    let mut m = manifest(id, ItemKind::Repo, origin, "body.tar.gz", &archive);
    m.commit = Some(commit.to_string());
    m.files = files
        .iter()
        .map(|(p, b)| ManifestFile {
            path: (*p).to_string(),
            sha256: sha(b),
            bytes: b.len() as u64,
        })
        .collect();
    (m, archive)
}

/// Stores a repository item; returns the manifest bytes.
fn put_repo(
    store: &FakeStore,
    id: &str,
    origin: &str,
    commit: &str,
    files: &[(&str, &[u8])],
) -> Vec<u8> {
    let (m, archive) = repo_manifest(id, origin, commit, files);
    store.put(&format!("items/{id}/body.tar.gz"), archive);
    put_manifest(store, &m)
}

/// What the route's answer names for a stored item.
fn item_ref(m: &ItemManifest) -> ItemRef {
    ItemRef {
        id: m.item_id.clone(),
        kind: m.kind,
        origin: m.origin.clone(),
        manifest_key: s3::manifest_key(RUN, &m.item_id),
        manifest_sha256: m.sha256(),
        body_key: m.body.key.clone(),
        body_sha256: m.body.sha256.clone(),
        bytes: m.body.bytes,
        licence: Some(m.licence.spdx.clone()),
        truncated: false,
        files: None,
        dropped: None,
    }
}

fn open(label: &str, settings: Settings) -> (std::path::PathBuf, ContextDir, Placer) {
    let data = scratch(label);
    let ctx = ContextDir::create(&data).unwrap();
    ctx.write_task("the original prompt").unwrap();
    let placer = Placer::open(&ctx, RUN, settings).unwrap();
    (data, ctx, placer)
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

fn small() -> Settings {
    Settings {
        min_free_bytes: 0,
        ..Settings::default()
    }
}

// ---- a page ---------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_page_is_placed_read_only_with_its_manifest_and_an_index_row() {
    let store = FakeStore::default();
    let body = text(2000, "page body");
    let manifest = put_page(&store, "i-0001", "https://docs.example.org/guide", &body);
    let (data, ctx, placer) = open("pl-page", small());

    let placed = placer.place(&store, "i-0001").await.unwrap();
    assert!(placed.new);
    let item = &placed.item;
    assert_eq!(item.item_id, "i-0001");
    assert_eq!(item.kind, "page");
    assert_eq!(item.origin, "https://docs.example.org/guide");
    assert_eq!(item.licence, "MIT");
    assert_eq!(item.manifest_sha256, sha(&manifest));
    assert_eq!(item.body_sha256, sha(&body));
    assert_eq!(item.bytes, body.len() as u64);

    let research = ctx.path().join("research");
    let page = research.join("pages/i-0001.txt");
    assert_eq!(item.path, page.display().to_string());
    assert_eq!(item.rel, "research/pages/i-0001.txt");
    assert_eq!(std::fs::read(&page).unwrap(), body);
    assert_eq!(mode(&page), 0o444);
    let copy = research.join("manifests/i-0001.json");
    assert_eq!(std::fs::read(&copy).unwrap(), manifest);
    assert_eq!(mode(&copy), 0o444);
    // The directories the next item needs stay writable for agentd; the context's own files are untouched.
    for d in ["", "pages", "repos", "manifests"] {
        assert_eq!(mode(&research.join(d)), 0o755, "{d:?}");
    }
    assert_eq!(
        std::fs::read_to_string(ctx.task_path()).unwrap(),
        "the original prompt"
    );

    let index: Value =
        serde_json::from_slice(&std::fs::read(research.join("INDEX.json")).unwrap()).unwrap();
    assert_eq!(index["schema"], "hamh-research-index/1");
    assert_eq!(index["run"], RUN);
    assert_eq!(index["items"][0]["item_id"], "i-0001");
    assert_eq!(index["items"][0]["manifest_sha256"], sha(&manifest));
    assert_eq!(mode(&research.join("INDEX.json")), 0o444);
    let md = std::fs::read_to_string(research.join("INDEX.md")).unwrap();
    assert_eq!(md, placer.index_md());
    assert_eq!(placer.used_bytes(), body.len() as u64);
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn placing_the_same_item_again_reads_nothing_and_changes_nothing() {
    let store = FakeStore::default();
    put_page(
        &store,
        "i-0001",
        "https://docs.example.org/guide",
        &text(2000, "p"),
    );
    let (data, _ctx, placer) = open("pl-again", small());
    placer.place(&store, "i-0001").await.unwrap();
    let reads = store.gets.lock().unwrap().len();
    let again = placer.place(&store, "i-0001").await.unwrap();
    assert!(!again.new);
    assert_eq!(
        store.gets.lock().unwrap().len(),
        reads,
        "no object was read again"
    );
    assert_eq!(placer.items().len(), 1);
    assert_eq!(placer.used_bytes(), text(2000, "p").len() as u64);
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_whose_hash_differs_leaves_no_file_no_index_row_and_no_counted_bytes() {
    let store = FakeStore::default();
    let body = text(2000, "p");
    put_page(&store, "i-0001", "https://docs.example.org/guide", &body);
    let mut altered = body.clone();
    altered[3] = b'#';
    store.put("items/i-0001/body.txt", altered);
    let (data, ctx, placer) = open("pl-altered", small());
    let err = placer.place(&store, "i-0001").await.unwrap_err();
    assert!(matches!(err, PlaceError::Integrity(_)), "{err:?}");
    assert_eq!(err.code(), "integrity");
    assert!(!ctx.path().join("research/pages/i-0001.txt").exists());
    assert!(!ctx.path().join("research/manifests/i-0001.json").exists());
    assert!(placer.items().is_empty());
    assert_eq!(placer.used_bytes(), 0);
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_of_another_length_than_the_manifest_says_is_refused() {
    let store = FakeStore::default();
    let body = text(2000, "p");
    put_page(&store, "i-0001", "https://docs.example.org/guide", &body);
    let mut longer = body.clone();
    longer.extend_from_slice(b"extra\n");
    store.put("items/i-0001/body.txt", longer);
    let (data, _ctx, placer) = open("pl-length", small());
    let err = placer.place(&store, "i-0001").await.unwrap_err();
    assert!(matches!(err, PlaceError::Integrity(_)), "{err:?}");
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manifest_that_is_not_this_runs_or_this_items_or_not_understood_is_refused() {
    let body = text(1200, "p");
    let good = manifest(
        "i-0001",
        ItemKind::Page,
        "https://docs.example.org/g",
        "body.txt",
        &body,
    );
    let prefix = s3::prefix(RUN);
    type Edit = Box<dyn Fn(&mut ItemManifest)>;
    let edits: Vec<(&str, Edit)> = vec![
        (
            "another run",
            Box::new(|m| m.run = "01M4XOTHERRNDRCT0000000001".into()),
        ),
        ("another agent", Box::new(|m| m.agent = "other".into())),
        ("another item", Box::new(|m| m.item_id = "i-0002".into())),
        (
            "unknown schema",
            Box::new(|m| m.schema = "hamh-research-item/9".into()),
        ),
        (
            "an attachment",
            Box::new(|m| {
                m.kind = ItemKind::Attachment;
                m.provider = ResearchProvider::User;
            }),
        ),
        (
            "a provider that does not fit the kind",
            Box::new(|m| m.provider = ResearchProvider::GithubArchive),
        ),
        ("a body key of another item", {
            let p = prefix.clone();
            Box::new(move |m| m.body.key = format!("{p}items/i-0002/body.txt"))
        }),
        ("a body key below a folder", {
            let p = prefix.clone();
            Box::new(move |m| m.body.key = format!("{p}items/i-0001/sub/body.txt"))
        }),
        (
            "a body key of another run",
            Box::new(|m| {
                m.body.key =
                    "runs/01M4XOTHERRNDRCT0000000001/agents/direct/research/items/i-0001/body.txt"
                        .into();
            }),
        ),
        (
            "a hash that is not hex",
            Box::new(|m| m.body.sha256 = "zz".into()),
        ),
        (
            "a time that is not UTC",
            Box::new(|m| m.retrieved_at = "yesterday".into()),
        ),
    ];
    for (what, edit) in edits {
        let store = FakeStore::default();
        let mut m = good.clone();
        edit(&mut m);
        store.put("items/i-0001/manifest.json", m.canonical_bytes());
        store.put("items/i-0001/body.txt", body.clone());
        let (data, _ctx, placer) = open("pl-manifest", small());
        let err = placer.place(&store, "i-0001").await.unwrap_err();
        assert!(matches!(err, PlaceError::BadManifest(_)), "{what}: {err:?}");
        assert_eq!(err.code(), "integrity", "{what}");
        assert_eq!(
            store.get_count("/body.txt"),
            0,
            "{what}: the body is not read"
        );
        remove_tree(&data);
    }
    // Text that is not a manifest, and a manifest without its body member.
    for (what, junk) in [
        ("not json", b"{not json".to_vec()),
        (
            "no body member",
            serde_json::to_vec(&{
                let mut v = serde_json::to_value(&good).unwrap();
                v.as_object_mut().unwrap().remove("body");
                v
            })
            .unwrap(),
        ),
    ] {
        let store = FakeStore::default();
        store.put("items/i-0001/manifest.json", junk);
        let (data, _ctx, placer) = open("pl-manifest-junk", small());
        let err = placer.place(&store, "i-0001").await.unwrap_err();
        assert!(matches!(err, PlaceError::BadManifest(_)), "{what}: {err:?}");
        remove_tree(&data);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_routes_answer_is_checked_against_the_manifest_before_the_body_is_read() {
    let store = FakeStore::default();
    let body = text(1500, "p");
    put_page(&store, "i-0001", "https://docs.example.org/g", &body);
    let m = manifest(
        "i-0001",
        ItemKind::Page,
        "https://docs.example.org/g",
        "body.txt",
        &body,
    );
    let (data, _ctx, placer) = open("pl-ref", small());
    type Edit = Box<dyn Fn(&mut ItemRef)>;
    let edits: Vec<(&str, Edit)> = vec![
        (
            "the manifest hash",
            Box::new(|r| r.manifest_sha256 = "0".repeat(64)),
        ),
        (
            "the body hash",
            Box::new(|r| r.body_sha256 = "1".repeat(64)),
        ),
        (
            "the manifest key",
            Box::new(|r| r.manifest_key = "elsewhere/manifest.json".into()),
        ),
        (
            "the body key",
            Box::new(|r| r.body_key = format!("{}items/i-0001/other", s3::prefix(RUN))),
        ),
        ("the kind", Box::new(|r| r.kind = ItemKind::Repo)),
        ("the item", Box::new(|r| r.id = "i-0002".into())),
    ];
    for (what, edit) in edits {
        let mut r = item_ref(&m);
        edit(&mut r);
        let err = placer.place_ref(&store, &r).await.unwrap_err();
        assert!(
            matches!(err, PlaceError::Integrity(_) | PlaceError::BadManifest(_)),
            "{what}: {err:?}"
        );
        assert_eq!(
            store.get_count("/body.txt"),
            0,
            "{what}: the body is not read"
        );
        assert!(placer.items().is_empty(), "{what}");
    }
    // The answer that agrees with the manifest places the item.
    let placed = placer.place_ref(&store, &item_ref(&m)).await.unwrap();
    assert!(placed.new);
    assert_eq!(placed.item.manifest_sha256, m.sha256());
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_item_id_that_is_not_plain_is_refused_before_anything_is_read() {
    let store = FakeStore::default();
    let (data, _ctx, placer) = open("pl-id", small());
    let long = "x".repeat(40);
    for bad in ["", "a/b", "i 1", "i-0001\n", ".", long.as_str()] {
        let err = placer.place(&store, bad).await.unwrap_err();
        assert!(
            matches!(err, PlaceError::BadManifest(_)),
            "{bad:?}: {err:?}"
        );
    }
    assert!(store.gets.lock().unwrap().is_empty());
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_manifest_is_unavailable_not_an_integrity_failure() {
    let store = FakeStore::default();
    let (data, _ctx, placer) = open("pl-missing", small());
    let err = placer.place(&store, "i-0009").await.unwrap_err();
    assert!(matches!(err, PlaceError::Unavailable(_)), "{err:?}");
    assert_eq!(err.code(), "unavailable");
    remove_tree(&data);
}

// ---- caps -----------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_page_over_the_page_cap_is_refused_before_its_body_is_read() {
    let store = FakeStore::default();
    put_page(
        &store,
        "i-0001",
        "https://docs.example.org/g",
        &text(5000, "big"),
    );
    let (data, _ctx, placer) = open(
        "pl-pagecap",
        Settings {
            max_page_bytes: 4000,
            ..small()
        },
    );
    let err = placer.place(&store, "i-0001").await.unwrap_err();
    assert!(matches!(err, PlaceError::TooLarge(_)), "{err:?}");
    assert_eq!(err.code(), "too_large");
    assert_eq!(store.get_count("/body.txt"), 0);
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_run_cap_counts_every_placed_byte_and_refuses_the_item_that_would_pass_it() {
    let store = FakeStore::default();
    let a = text(3000, "a");
    let b = text(3000, "b");
    let c = text(1000, "c");
    put_page(&store, "i-0001", "https://docs.example.org/a", &a);
    put_page(&store, "i-0002", "https://docs.example.org/b", &b);
    put_page(&store, "i-0003", "https://docs.example.org/c", &c);
    let (data, _ctx, placer) = open(
        "pl-runcap",
        Settings {
            max_run_bytes: 5000,
            ..small()
        },
    );
    placer.place(&store, "i-0001").await.unwrap();
    let err = placer.place(&store, "i-0002").await.unwrap_err();
    assert!(matches!(err, PlaceError::LimitContext(_)), "{err:?}");
    assert_eq!(err.code(), "limit_context");
    assert_eq!(
        store.get_count("i-0002/body.txt"),
        0,
        "refused before the body is read"
    );
    assert_eq!(
        placer.used_bytes(),
        a.len() as u64,
        "a refused item counts nothing"
    );
    placer.place(&store, "i-0003").await.unwrap();
    assert_eq!(placer.used_bytes(), (a.len() + c.len()) as u64);
    assert_eq!(placer.items().len(), 2);
    remove_tree(&data);
}

// ---- a repository ---------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_is_unpacked_to_a_named_read_only_directory_and_indexed() {
    let store = FakeStore::default();
    let lib = text(2500, "lib");
    let readme = text(1500, "readme");
    let commit = "666df7c10035f7e26f27ec214fe5ae3173435f34";
    let manifest = put_repo(
        &store,
        "i-0002",
        "https://github.com/sindresorhus/is-plain-obj",
        commit,
        &[("index.js", &lib), ("docs/readme.md", &readme)],
    );
    let (data, ctx, placer) = open("pl-repo", small());
    let placed = placer.place(&store, "i-0002").await.unwrap();
    let item = &placed.item;
    assert_eq!(item.kind, "repo");
    assert_eq!(item.commit.as_deref(), Some(commit));
    assert_eq!(item.files, 2);
    assert_eq!(item.bytes, (lib.len() + readme.len()) as u64);
    assert_eq!(item.manifest_sha256, sha(&manifest));
    let dir = ctx
        .path()
        .join("research/repos/sindresorhus__is-plain-obj__666df7c10035");
    assert_eq!(item.path, dir.display().to_string());
    assert_eq!(
        item.rel,
        "research/repos/sindresorhus__is-plain-obj__666df7c10035"
    );
    assert_eq!(std::fs::read(dir.join("index.js")).unwrap(), lib);
    assert_eq!(std::fs::read(dir.join("docs/readme.md")).unwrap(), readme);
    assert_eq!(mode(&dir.join("index.js")), 0o444);
    assert_eq!(mode(&dir.join("docs")), 0o555);
    assert_eq!(mode(&dir), 0o555);
    assert!(
        !ctx.path().join("research/.stage").exists()
            || std::fs::read_dir(ctx.path().join("research/.stage"))
                .unwrap()
                .count()
                == 0,
        "no staging directory is left"
    );
    assert_eq!(placer.used_bytes(), item.bytes);
    // The kept archive is not placed: only the unpacked files count against the run.
    assert!(!dir.join("body.tar.gz").exists());
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_items_of_the_same_repository_commit_get_different_directories() {
    let store = FakeStore::default();
    let a = text(1500, "a");
    let b = text(1500, "b");
    let commit = "666df7c10035f7e26f27ec214fe5ae3173435f34";
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        commit,
        &[("a.js", &a)],
    );
    put_repo(
        &store,
        "i-0003",
        "https://github.com/acme/widgets",
        commit,
        &[("b.js", &b)],
    );
    let (data, _ctx, placer) = open("pl-twin", small());
    let one = placer.place(&store, "i-0002").await.unwrap().item;
    let two = placer.place(&store, "i-0003").await.unwrap().item;
    assert_ne!(one.path, two.path);
    assert!(Path::new(&one.path).join("a.js").is_file());
    assert!(Path::new(&two.path).join("b.js").is_file());
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_archive_whose_hash_differs_is_refused_and_leaves_nothing() {
    let store = FakeStore::default();
    let a = text(1500, "a");
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        COMMIT,
        &[("a.js", &a)],
    );
    let mut altered = store
        .objects
        .lock()
        .unwrap()
        .get(&research_key(RUN, "items/i-0002/body.tar.gz"))
        .cloned()
        .unwrap();
    let last = altered.len() - 3;
    altered[last] ^= 0xFF;
    store.put("items/i-0002/body.tar.gz", altered);
    let (data, ctx, placer) = open("pl-repo-altered", small());
    let err = placer.place(&store, "i-0002").await.unwrap_err();
    assert!(matches!(err, PlaceError::Integrity(_)), "{err:?}");
    assert!(placer.items().is_empty());
    assert_eq!(placer.used_bytes(), 0);
    let repos = ctx.path().join("research/repos");
    assert_eq!(std::fs::read_dir(&repos).unwrap().count(), 0);
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manifest_that_lists_more_than_the_caps_allow_is_refused_before_the_archive_is_read() {
    let a = text(1500, "a");
    let b = text(1500, "b");
    let cases: Vec<(&str, Settings)> = vec![
        (
            "files",
            Settings {
                max_files: 1,
                ..small()
            },
        ),
        (
            "unpacked",
            Settings {
                max_unpacked_bytes: 2000,
                ..small()
            },
        ),
        (
            "file",
            Settings {
                max_file_bytes: 1000,
                ..small()
            },
        ),
        (
            "archive",
            Settings {
                max_archive_bytes: 100,
                ..small()
            },
        ),
    ];
    for (what, settings) in cases {
        let store = FakeStore::default();
        put_repo(
            &store,
            "i-0002",
            "https://github.com/acme/widgets",
            COMMIT,
            &[("a.js", &a), ("b.js", &b)],
        );
        let (data, _ctx, placer) = open("pl-repo-caps", settings);
        let err = placer.place(&store, "i-0002").await.unwrap_err();
        assert!(matches!(err, PlaceError::TooLarge(_)), "{what}: {err:?}");
        assert_eq!(store.get_count("body.tar.gz"), 0, "{what}");
        remove_tree(&data);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_that_would_pass_the_run_cap_is_refused() {
    let store = FakeStore::default();
    let a = text(1500, "a");
    let b = text(1500, "b");
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        COMMIT,
        &[("a.js", &a), ("b.js", &b)],
    );
    let (data, _ctx, placer) = open(
        "pl-repo-run",
        Settings {
            max_run_bytes: 2500,
            ..small()
        },
    );
    let err = placer.place(&store, "i-0002").await.unwrap_err();
    assert!(matches!(err, PlaceError::LimitContext(_)), "{err:?}");
    assert_eq!(store.get_count("body.tar.gz"), 0);
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_placements_of_one_item_read_its_body_once() {
    let store = Arc::new(FakeStore::default());
    *store.pause.lock().unwrap() = Duration::from_millis(60);
    put_page(
        &store,
        "i-0001",
        "https://docs.example.org/g",
        &text(2000, "p"),
    );
    let (data, _ctx, placer) = open("pl-race", small());
    let placer = Arc::new(placer);
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let (s, p) = (store.clone(), placer.clone());
        tasks.push(tokio::spawn(async move {
            p.place(&*s, "i-0001").await.unwrap()
        }));
    }
    let mut firsts = 0;
    for t in tasks {
        if t.await.unwrap().new {
            firsts += 1;
        }
    }
    assert_eq!(firsts, 1, "exactly one caller placed it");
    assert_eq!(store.get_count("i-0001/body.txt"), 1);
    assert_eq!(placer.items().len(), 1);
    remove_tree(&data);
}

// ---- verify ---------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn verify_finds_a_changed_file_a_removed_file_and_a_manifest_that_changed() {
    let store = FakeStore::default();
    let lib = text(2500, "lib");
    let readme = text(1500, "readme");
    put_page(
        &store,
        "i-0001",
        "https://docs.example.org/g",
        &text(2000, "p"),
    );
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        COMMIT,
        &[("index.js", &lib), ("readme.md", &readme)],
    );
    let (data, _ctx, placer) = open("pl-verify", small());
    let page = placer.place(&store, "i-0001").await.unwrap().item;
    let repo = placer.place(&store, "i-0002").await.unwrap().item;
    assert!(
        placer.verify(&store).await.is_empty(),
        "a fresh placement verifies"
    );

    // Someone with the owner's rights changes one file and removes another.
    let index_js = Path::new(&repo.path).join("index.js");
    std::fs::set_permissions(
        Path::new(&repo.path),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::set_permissions(&index_js, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&index_js, b"changed").unwrap();
    std::fs::remove_file(Path::new(&repo.path).join("readme.md")).unwrap();
    let problems = placer.verify(&store).await;
    let whats: Vec<(&str, &str)> = problems
        .iter()
        .map(|p| (p.item_id.as_str(), p.path.as_str()))
        .collect();
    assert!(whats.contains(&("i-0002", "index.js")), "{problems:?}");
    assert!(whats.contains(&("i-0002", "readme.md")), "{problems:?}");
    assert!(!whats.iter().any(|(id, _)| *id == "i-0001"), "{problems:?}");

    // The route's manifest changed under the same item id: the local copy no longer matches what the route says.
    let mut changed = manifest(
        "i-0001",
        ItemKind::Page,
        "https://docs.example.org/other",
        "body.txt",
        &text(2000, "p"),
    );
    changed.licence.spdx = "GPL-3.0-only".into();
    store.put("items/i-0001/manifest.json", changed.canonical_bytes());
    let problems = placer.verify(&store).await;
    assert!(
        problems
            .iter()
            .any(|p| p.item_id == page.item_id && p.path == "manifest.json"),
        "{problems:?}"
    );
    remove_tree(&data);
}

// ---- the listing of what was placed ---------------------------------------------------------------------------

fn listing_of(placer: &Placer) -> Value {
    serde_json::from_str(&placer.listing()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_listing_names_every_placed_file_with_the_hash_of_the_bytes_on_the_disk() {
    let store = FakeStore::default();
    let body = text(2000, "page body");
    let lib = text(2500, "lib");
    let util = text(900, "util");
    let readme = text(1500, "readme");
    let page_manifest = put_page(&store, "i-0001", "https://docs.example.org/guide", &body);
    let repo_manifest_bytes = put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        COMMIT,
        &[
            ("index.js", &lib),
            ("src/util.js", &util),
            ("readme.md", &readme),
        ],
    );
    let (data, _ctx, placer) = open("pl-listing", small());
    placer.place(&store, "i-0001").await.unwrap();
    placer.place(&store, "i-0002").await.unwrap();

    let raw = placer.listing();
    assert!(raw.ends_with('\n'), "a text file ends with a newline");
    let v: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["schema"], "hamh-research-placed/1");
    assert_eq!(v["run"], RUN);
    // A page is keyed by the name of the manifest's body key, a repository file by its path under the repository's directory: the keys
    // a judge reads the route's manifests with.
    assert_eq!(
        v["items"]["i-0001"]["files"],
        serde_json::json!({"body.txt": sha(&body)})
    );
    assert_eq!(
        v["items"]["i-0002"]["files"],
        serde_json::json!({
            "index.js": sha(&lib),
            "readme.md": sha(&readme),
            "src/util.js": sha(&util),
        })
    );
    assert_eq!(v["items"].as_object().unwrap().len(), 2);
    assert_eq!(placer.listing(), raw, "the same text every time");
    // What the judge of the research checks expects, built from the manifests the
    // route stored by its rule: a repository by the paths and hashes of `files`, a page by the name of the body key and the hash of the body.
    let judge_wants = |bytes: &[u8]| -> BTreeMap<String, String> {
        let m: ItemManifest = serde_json::from_slice(bytes).unwrap();
        if m.kind == ItemKind::Repo {
            m.files
                .iter()
                .map(|f| (f.path.clone(), f.sha256.clone()))
                .collect()
        } else {
            BTreeMap::from([(
                m.body.key.rsplit('/').next().unwrap().to_string(),
                m.body.sha256.clone(),
            )])
        }
    };
    for (id, bytes) in [("i-0001", &page_manifest), ("i-0002", &repo_manifest_bytes)] {
        let have: BTreeMap<String, String> =
            serde_json::from_value(v["items"][id]["files"].clone()).unwrap();
        assert_eq!(have, judge_wants(bytes), "{id}");
    }
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_listing_hashes_the_disk_and_not_the_manifest() {
    // A listing taken from the manifests would agree with them by construction and prove nothing. This one reads the files: a file that
    // changed shows the hash of what is there, a file that is gone is missing, and a file the manifest does not list is listed.
    let store = FakeStore::default();
    let lib = text(2500, "lib");
    let readme = text(1500, "readme");
    put_page(
        &store,
        "i-0001",
        "https://docs.example.org/guide",
        &text(2000, "page body"),
    );
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        COMMIT,
        &[("index.js", &lib), ("readme.md", &readme)],
    );
    let (data, _ctx, placer) = open("pl-listing-disk", small());
    let page = placer.place(&store, "i-0001").await.unwrap().item;
    let repo = placer.place(&store, "i-0002").await.unwrap().item;
    std::fs::set_permissions(&page.path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&page.path, b"the page was changed").unwrap();
    let root = Path::new(&repo.path);
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(
        root.join("index.js"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::fs::write(root.join("index.js"), b"changed").unwrap();
    std::fs::remove_file(root.join("readme.md")).unwrap();
    std::fs::write(root.join("extra.txt"), b"not in the manifest").unwrap();

    let listing = listing_of(&placer);
    assert_eq!(
        listing["items"]["i-0002"]["files"],
        serde_json::json!({"extra.txt": sha(b"not in the manifest"), "index.js": sha(b"changed")}),
        "TRIGGER PRESENT: the disk differs from the manifest in three ways"
    );
    assert_eq!(
        listing["items"]["i-0001"]["files"],
        serde_json::json!({"body.txt": sha(b"the page was changed")}),
        "a page is hashed from the disk too"
    );
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_in_a_repository_directory_is_neither_followed_nor_listed() {
    let store = FakeStore::default();
    let lib = text(2500, "lib");
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        COMMIT,
        &[("index.js", &lib)],
    );
    let (data, _ctx, placer) = open("pl-listing-link", small());
    let repo = placer.place(&store, "i-0002").await.unwrap().item;
    let root = Path::new(&repo.path);
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(root.join("index.js"), root.join("alias.js")).unwrap();

    let files = listing_of(&placer)["items"]["i-0002"]["files"].clone();
    assert_eq!(
        files,
        serde_json::json!({"index.js": sha(&lib)}),
        "TRIGGER PRESENT: the link exists"
    );
    assert!(root.join("alias.js").symlink_metadata().is_ok());
    remove_tree(&data);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_placement_has_a_listing_with_no_items() {
    let (data, _ctx, placer) = open("pl-listing-empty", small());
    let v = listing_of(&placer);
    assert_eq!(
        v,
        serde_json::json!({"schema": "hamh-research-placed/1", "run": RUN, "items": {}})
    );
    remove_tree(&data);
}

// ---- the copy check -------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn check_tree_reports_copies_of_stored_files_by_content_and_not_by_name() {
    let store = FakeStore::default();
    let lib = text(2500, "lib");
    let tiny = text(500, "tiny");
    let licence = text(3000, "licence text");
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        COMMIT,
        &[
            ("lib/util.js", &lib),
            ("tiny.js", &tiny),
            ("LICENSE", &licence),
        ],
    );
    let (data, _ctx, placer) = open("pl-check", small());
    placer.place(&store, "i-0002").await.unwrap();

    let work = scratch("pl-check-work");
    std::fs::create_dir_all(work.join("src/vendor")).unwrap();
    std::fs::create_dir_all(work.join(".git")).unwrap();
    std::fs::write(work.join("src/vendor/renamed.txt"), &lib).unwrap();
    let crlf = String::from_utf8(lib.clone())
        .unwrap()
        .replace('\n', "\r\n");
    std::fs::write(work.join("src/crlf.js"), crlf).unwrap();
    std::fs::write(work.join("src/tiny.js"), &tiny).unwrap();
    std::fs::write(work.join("LICENSE"), &licence).unwrap();
    std::fs::write(work.join("docs-licence-copy.txt"), &licence).unwrap();
    std::fs::write(work.join(".git/copied"), &lib).unwrap();
    std::os::unix::fs::symlink(work.join("src/vendor/renamed.txt"), work.join("link.js")).unwrap();
    std::fs::write(work.join("own.rs"), text(2500, "my own code")).unwrap();

    let report = placer.check_tree(&work, &work).unwrap();
    let mut found: Vec<(&str, &str, &str)> = report
        .matches
        .iter()
        .map(|m| (m.path.as_str(), m.kind.as_str(), m.item_id.as_str()))
        .collect();
    found.sort();
    assert_eq!(
        found,
        [
            ("src/crlf.js", "normalised", "i-0002"),
            ("src/vendor/renamed.txt", "exact", "i-0002")
        ],
        "{report:?}"
    );
    let m = &report.matches[0];
    assert_eq!(m.origin, "https://github.com/acme/widgets");
    assert_eq!(m.licence, "MIT");
    assert_eq!(m.stored_path, "lib/util.js");
    assert!(!report.truncated);
    assert!(report.scanned_files >= 5);

    // A directory outside the work tree is not read.
    let outside = scratch("pl-check-outside");
    assert!(placer.check_tree(&work, &outside).is_err());
    remove_tree(&data);
}

// ---- the index text -------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_index_table_has_the_columns_the_lifecycle_pack_reads_and_one_row_per_item() {
    let store = FakeStore::default();
    let m1 = put_page(
        &store,
        "i-0001",
        "https://docs.example.org/a|b",
        &text(1200, "p"),
    );
    let lib = text(1500, "lib");
    let m2 = put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        COMMIT,
        &[("a.js", &lib)],
    );
    let (data, _ctx, placer) = open("pl-md", small());
    placer.place(&store, "i-0001").await.unwrap();
    placer.place(&store, "i-0002").await.unwrap();
    let md = placer.index_md();
    let lines: Vec<&str> = md.lines().collect();
    assert_eq!(
        lines[0],
        "| Origin | Date read | sha256 | Licence | Why it matters |"
    );
    assert!(lines[1].starts_with("| ---"));
    assert_eq!(lines.len(), 4, "{md}");
    // A pipe in an origin cannot split the row; the date is the day the route retrieved it.
    assert!(
        lines[2].contains("https://docs.example.org/a%7Cb"),
        "{}",
        lines[2]
    );
    assert!(lines[2].contains("| 2026-10-16 |"), "{}", lines[2]);
    assert!(lines[2].contains(&sha(&m1)));
    // A repository's origin carries its commit, like package@version.
    assert!(
        lines[3].contains(&format!("https://github.com/acme/widgets@{COMMIT}")),
        "{}",
        lines[3]
    );
    assert!(lines[3].contains(&sha(&m2)));
    assert!(lines[3].contains("| MIT |"), "{}", lines[3]);
    remove_tree(&data);
}

/// `fixtures/research/index-md.golden` is the table `ham-research index --md` prints for two stored items. The lifecycle pack's self-check
/// (check LC-2) and the harness scorer are tested on that same file,
/// so the renderer, the pack text that tells a run to start its context index from this table, and the check that reads the result cannot
/// drift apart without a test failing here or there.
#[tokio::test(flavor = "multi_thread")]
async fn the_index_table_is_the_golden_file_the_lifecycle_check_is_tested_on() {
    let store = FakeStore::default();
    put_page(
        &store,
        "i-0001",
        "https://docs.example.org/a|b",
        &text(1200, "p"),
    );
    let lib = text(1500, "lib");
    put_repo(
        &store,
        "i-0002",
        "https://github.com/acme/widgets",
        COMMIT,
        &[("a.js", &lib)],
    );
    let (data, _ctx, placer) = open("pl-golden", small());
    placer.place(&store, "i-0001").await.unwrap();
    placer.place(&store, "i-0002").await.unwrap();
    let golden = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/research/index-md.golden"
    ))
    .unwrap();
    assert_eq!(placer.index_md(), golden);
    remove_tree(&data);
}

/// The table `ham-research index --md` prints is, byte for byte, the example that the DIRECT-RESEARCH index judge and the record builder of
/// `scripts/tests` are tested with (`fixtures/research/index-md.example`): five columns, a page by its origin, a repository by `origin@commit`,
/// the day it was read, the manifest's SHA-256, the licence and where the stored copy is.
#[test]
fn the_index_table_is_byte_for_byte_the_example_the_judge_reads() {
    let item = |id: &str,
                kind: &str,
                origin: &str,
                commit: Option<&str>,
                licence: &str,
                sha: &str,
                rel: &str| Item {
        item_id: id.into(),
        kind: kind.into(),
        origin: origin.into(),
        commit: commit.map(str::to_string),
        retrieved_at: "2026-10-16T14:29:55Z".into(),
        licence: licence.into(),
        licence_source: "none".into(),
        provider: "fixture".into(),
        manifest_sha256: sha.into(),
        body_sha256: "0".repeat(64),
        bytes: 1,
        files: 1,
        path: format!("/ctx/{rel}"),
        rel: rel.into(),
    };
    // The manifest hashes are opaque to the renderer: they are read from the example, which another area regenerates when the manifest bytes change.
    let example = include_str!("../../../fixtures/research/index-md.example");
    let sha_cells: Vec<&str> = example
        .lines()
        .skip(2)
        .map(|row| row.split(" | ").nth(2).unwrap())
        .collect();
    assert_eq!(
        sha_cells.len(),
        2,
        "the example has one row for the page and one for the repository"
    );
    let mut items = BTreeMap::new();
    items.insert(
        "i-0001".to_string(),
        item(
            "i-0001",
            "page",
            "https://www.npmjs.com/package/is-plain-obj",
            None,
            "NOASSERTION",
            sha_cells[0],
            "research/pages/i-0001.txt",
        ),
    );
    items.insert(
        "i-0002".to_string(),
        item(
            "i-0002",
            "repo",
            "https://github.com/sindresorhus/is-plain-obj",
            Some(COMMIT),
            "MIT",
            sha_cells[1],
            "research/repos/sindresorhus__is-plain-obj__666df7c10035",
        ),
    );
    assert_eq!(render_md(&items), example);
}

// ---- names and the directory ----------------------------------------------------------------------------------------------

#[test]
fn item_ids_are_plain_names_and_repository_directories_follow_the_origin() {
    let (l32, l33) = ("x".repeat(32), "x".repeat(33));
    for ok in ["i-0001", "i-12", "A", "a_b-c", l32.as_str()] {
        assert!(valid_item_id(ok), "{ok:?}");
    }
    for bad in ["", "-a", "_a", "a/b", "a b", ".", "a.b", l33.as_str(), "é"] {
        assert!(!valid_item_id(bad), "{bad:?}");
    }
    let commit = "666df7c10035f7e26f27ec214fe5ae3173435f34";
    assert_eq!(
        repo_dir_name("https://github.com/sindresorhus/is-plain-obj", Some(commit)).as_deref(),
        Some("sindresorhus__is-plain-obj__666df7c10035")
    );
    assert_eq!(
        repo_dir_name("https://github.com/acme/widgets.git", Some(commit)).as_deref(),
        Some("acme__widgets__666df7c10035")
    );
    assert_eq!(
        repo_dir_name("https://github.com/acme/widgets", None).as_deref(),
        Some("acme__widgets")
    );
    assert_eq!(repo_dir_name("https://example.org/a/b", Some(commit)), None);
    assert_eq!(
        repo_dir_name("https://github.com/a/b/c", Some(commit)),
        None
    );
    assert_eq!(
        repo_dir_name("https://github.com/a%2Fb/c", Some(commit)),
        None
    );
}

#[test]
fn the_research_directory_refuses_a_link_or_a_file_in_its_place() {
    let data = scratch("pl-dir");
    let ctx = ContextDir::create(&data).unwrap();
    let elsewhere = data.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, ctx.path().join("research")).unwrap();
    assert!(ResearchDir::create(&ctx).is_err());
    let _ = std::fs::remove_file(ctx.path().join("research"));
    std::fs::write(ctx.path().join("research"), b"x").unwrap();
    assert!(ResearchDir::create(&ctx).is_err());
    let _ = std::fs::remove_file(ctx.path().join("research"));
    let dir = ResearchDir::create(&ctx).unwrap();
    assert!(dir.root().join("pages").is_dir() && dir.root().join("repos").is_dir());
    remove_tree(&data);
}
