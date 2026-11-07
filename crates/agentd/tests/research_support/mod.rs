//! Shared fakes of the research module's tests: a bucket that counts what is read from it, builders for the route's
//! stored items from `harness_protocol::research`, a route that answers from a script, and a small HTTP client that needs no process.
#![allow(dead_code, clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::direct::research::route::{FailureKind, Route, RouteFailure};
use agentd::direct::research::store::{ObjectStore, StoreError, research_key};
use flate2::Compression;
use flate2::write::GzEncoder;
use harness_protocol::research::{
    BodyRef, ItemKind, ItemManifest, ItemRef, LicenceEvidence, ManifestFile, ResearchError,
    ResearchErrorCode, ResearchProvider, s3,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const RUN: &str = "01M4XTESTRNDRCT00000000001";
pub const COMMIT: &str = "666df7c10035f7e26f27ec214fe5ae3173435f34";

pub fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// Source-like text of about `n` bytes, whole lines only.
pub fn text(n: usize, tag: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while out.len() < n {
        out.extend_from_slice(format!("{tag} line {i:04}\n").as_bytes());
        i += 1;
    }
    out.truncate(out.iter().rposition(|b| *b == b'\n').map_or(n, |p| p + 1));
    out
}

// ---- the bucket ------------------------------------------------------------------------------------------------------------

/// A bucket that counts what is read from it.
#[derive(Default)]
pub struct FakeStore {
    pub objects: Mutex<BTreeMap<String, Vec<u8>>>,
    pub gets: Mutex<Vec<String>>,
    pub pause: Mutex<Duration>,
}

impl FakeStore {
    pub fn put(&self, rel: &str, bytes: Vec<u8>) {
        self.objects
            .lock()
            .unwrap()
            .insert(research_key(RUN, rel), bytes);
    }

    pub fn get_count(&self, rel_suffix: &str) -> usize {
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

// ---- stored items ----------------------------------------------------------------------------------------------------------

pub fn manifest(
    id: &str,
    kind: ItemKind,
    origin: &str,
    body_name: &str,
    body: &[u8],
) -> ItemManifest {
    ItemManifest {
        schema: "hamh-research-item/1".into(),
        item_id: id.into(),
        kind,
        run: RUN.into(),
        agent: "direct".into(),
        provider: match kind {
            ItemKind::Page => ResearchProvider::Fixture,
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

pub fn put_manifest(store: &FakeStore, m: &ItemManifest) -> Vec<u8> {
    let bytes = m.canonical_bytes();
    store.put(&format!("items/{}/manifest.json", m.item_id), bytes.clone());
    bytes
}

/// Stores a page item; returns its manifest.
pub fn put_page(store: &FakeStore, id: &str, origin: &str, body: &[u8]) -> ItemManifest {
    let m = manifest(id, ItemKind::Page, origin, "body.txt", body);
    store.put(&format!("items/{id}/body.txt"), body.to_vec());
    put_manifest(store, &m);
    m
}

pub fn targz(files: &[(&str, &[u8])]) -> Vec<u8> {
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

/// Stores a repository item; returns its manifest.
pub fn put_repo(
    store: &FakeStore,
    id: &str,
    origin: &str,
    files: &[(&str, &[u8])],
) -> ItemManifest {
    let archive = targz(files);
    let mut m = manifest(id, ItemKind::Repo, origin, "body.tar.gz", &archive);
    m.commit = Some(COMMIT.to_string());
    m.files = files
        .iter()
        .map(|(p, b)| ManifestFile {
            path: (*p).to_string(),
            sha256: sha(b),
            bytes: b.len() as u64,
        })
        .collect();
    store.put(&format!("items/{id}/body.tar.gz"), archive);
    put_manifest(store, &m);
    m
}

/// What the route's answer names for a stored item.
pub fn item_ref(m: &ItemManifest) -> ItemRef {
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
        files: u32::try_from(m.files.len()).ok().filter(|n| *n > 0),
        dropped: None,
    }
}

// ---- the route -------------------------------------------------------------------------------------------------------------

/// One scripted answer.
pub enum Reply {
    Ok(Value),
    Fail(RouteFailure),
}

/// A route that answers each path from a queue and remembers what it was asked.
#[derive(Default)]
pub struct FakeRoute {
    pub calls: Mutex<Vec<(String, Value)>>,
    script: Mutex<HashMap<String, VecDeque<Reply>>>,
    /// The answer once a path's queue is empty.
    fallback: Mutex<HashMap<String, Value>>,
    /// While set, a call waits for a permit before it answers.
    pub gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
}

impl FakeRoute {
    pub fn push(&self, path: &str, reply: Reply) {
        self.script
            .lock()
            .unwrap()
            .entry(path.to_string())
            .or_default()
            .push_back(reply);
    }

    pub fn push_ok<T: serde::Serialize>(&self, path: &str, body: &T) {
        self.push(path, Reply::Ok(serde_json::to_value(body).unwrap()));
    }

    pub fn push_fail(&self, path: &str, code: ResearchErrorCode, message: &str) {
        self.push(
            path,
            Reply::Fail(RouteFailure {
                error: ResearchError::new(code, message),
                kind: FailureKind::Final,
            }),
        );
    }

    pub fn push_transient(&self, path: &str) {
        self.push(
            path,
            Reply::Fail(RouteFailure {
                error: ResearchError::new(ResearchErrorCode::Internal, "the route did not answer"),
                kind: FailureKind::Transient,
            }),
        );
    }

    /// Answers `path` with `body` whenever its queue is empty.
    pub fn always<T: serde::Serialize>(&self, path: &str, body: &T) {
        self.fallback
            .lock()
            .unwrap()
            .insert(path.to_string(), serde_json::to_value(body).unwrap());
    }

    pub fn calls_to(&self, path: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, b)| b.clone())
            .collect()
    }
}

impl Route for FakeRoute {
    fn call<'a>(
        &'a self,
        path: &'static str,
        body: &'a Value,
    ) -> BoxFuture<'a, Result<Value, RouteFailure>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((path.to_string(), body.clone()));
            let gate = self.gate.lock().unwrap().clone();
            if let Some(g) = gate {
                g.acquire().await.unwrap().forget();
            }
            let next = self
                .script
                .lock()
                .unwrap()
                .get_mut(path)
                .and_then(VecDeque::pop_front);
            match next {
                Some(Reply::Ok(v)) => Ok(v),
                Some(Reply::Fail(f)) => Err(f),
                None => match self.fallback.lock().unwrap().get(path) {
                    Some(v) => Ok(v.clone()),
                    None => Err(RouteFailure {
                        error: ResearchError::new(
                            ResearchErrorCode::Internal,
                            "no scripted answer",
                        ),
                        kind: FailureKind::Final,
                    }),
                },
            }
        })
    }
}

// ---- a client that starts no process ----------------------------------------------------------------------------------------

/// One HTTP/1.1 request over a TCP connection; returns the status and the body.
pub async fn http(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

pub async fn post_json(addr: SocketAddr, path: &str, body: &Value) -> (u16, Value) {
    let (status, text) = http(addr, "POST", path, Some(&body.to_string())).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

pub async fn get_json(addr: SocketAddr, path: &str) -> (u16, Value) {
    let (status, text) = http(addr, "GET", path, None).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}
