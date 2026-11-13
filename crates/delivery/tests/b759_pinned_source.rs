//! The bundle validator reads the version of the source bundle that agentd wrote, and nothing at the key that was written after it.
//! The run's session credentials (which the engine holds too) can put a newer version of `turns/<n>/bundle`; the validator is told the version
//! and the SHA-256 of agentd's own write, so a newer version changes nothing and a missing or different one fails closed.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::error::DeliveryError;
use delivery::files::sha256_hex;
use delivery::git::Git;
use delivery::ports::{Blobs, PutMeta};
use delivery::validator::{Gitleaks, Limits, Rule, ValidateRequest, Validator};
use harness_protocol::store::s3::verified_bundle_key;
use std::path::{Path, PathBuf};

const KEY_SUFFIX: &str = "agents/direct/turns/1/bundle";

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    base: String,
    v: Validator<MemBlobs, Gitleaks>,
}

fn world() -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (work, shas) = work_repo(
        &root,
        "work",
        &[&[("src/calc.py", "def add(a, b):\n    return a + b\n")]],
    );
    let v = Validator {
        blobs: MemBlobs::default(),
        scanner: Gitleaks {
            bin: std::env::var_os("GITLEAKS_BIN")
                .map_or_else(|| PathBuf::from("gitleaks"), PathBuf::from),
            home: home.clone(),
        },
        git: Git::new("git", &home),
        bucket: BUCKET.into(),
        work_root: root.join("scratch"),
    };
    World {
        _tmp: tmp,
        root,
        work,
        base: shas[0].clone(),
        v,
    }
}

fn key() -> String {
    format!("runs/{RUN}/{KEY_SUFFIX}")
}

impl World {
    /// A full-history bundle of the work repo at its current HEAD, and that head.
    fn bundle(&self, label: &str) -> (Vec<u8>, String) {
        let branch = format!("harness/{RUN}");
        git(&self.work, &["branch", "-f", &branch, "HEAD"]);
        let f = self.root.join(format!("{label}.bundle"));
        git(
            &self.work,
            &["bundle", "create", "-q", f.to_str().unwrap(), &branch],
        );
        (
            std::fs::read(&f).unwrap(),
            git(&self.work, &["rev-parse", "HEAD"]),
        )
    }

    fn req(&self, head: &str) -> ValidateRequest {
        ValidateRequest {
            run_id: RUN.into(),
            base_sha: Some(self.base.clone()),
            head_sha: head.into(),
            allowed_globs: vec!["**".into(), "!.git/**".into()],
            limits: Limits::default(),
            source_bundle_uri: Some(format!("s3://{BUCKET}/{}", key())),
            source_bundle_version_id: None,
            source_bundle_sha256: None,
            engine_config: Default::default(),
            third_party: Default::default(),
            target: Default::default(),
            source_run_ids: Vec::new(),
        }
    }

    /// agentd's bundle is stored as version `v-agentd`, then a process with the session credentials stores a different bundle as the newest
    /// version. Returns (the request that names agentd's version and digest, agentd's head, the planted head).
    fn agentd_then_planted(&self) -> (ValidateRequest, String, String) {
        commit(
            &self.work,
            &[("src/calc.py", Some("x = 4\n"))],
            "agentd turn",
        );
        let (mine, my_head) = self.bundle("mine");
        self.v
            .blobs
            .insert_version(&key(), "v-agentd", mine.clone());
        commit(
            &self.work,
            &[("src/calc.py", Some("x = 5  # planted\n"))],
            "planted",
        );
        let (planted, planted_head) = self.bundle("planted");
        self.v.blobs.insert_version(&key(), "v-planted", planted);
        let mut req = self.req(&my_head);
        req.source_bundle_version_id = Some("v-agentd".into());
        req.source_bundle_sha256 = Some(sha256_hex(&mine));
        (req, my_head, planted_head)
    }
}

#[tokio::test]
async fn b759_without_a_record_the_validator_reads_the_object_that_is_at_the_key() {
    // The way every run was read before the records existed, and the way a run without them still is: the newest version is the object. This is
    // the gap the records close.
    let w = world();
    let (req, _, planted_head) = w.agentd_then_planted();
    let mut legacy = w.req(&planted_head);
    legacy.source_bundle_version_id = None;
    legacy.source_bundle_sha256 = None;
    let out = w.v.validate(&legacy).await.unwrap();
    assert!(
        out.pass,
        "the planted bundle was read as the source: {:?}",
        out.violations
    );
    assert!(
        w.v.blobs.reads.lock().unwrap().contains(&key()),
        "the object at the key was downloaded: {:?}",
        w.v.blobs.reads.lock().unwrap()
    );
    drop(req);
}

#[tokio::test]
async fn b759_the_validator_reads_the_recorded_version_and_not_the_newer_one() {
    let w = world();
    let (req, my_head, planted_head) = w.agentd_then_planted();
    assert_ne!(my_head, planted_head);
    let out = w.v.validate(&req).await.unwrap();
    assert!(out.pass, "{:?}", out.violations);
    assert_eq!(out.head_sha, my_head);
    // Only agentd's version of the source was downloaded; the key itself was never read without a version. (The validator then reads the verified
    // copy it just wrote, which is another key.)
    let reads = w.v.blobs.reads.lock().unwrap().clone();
    assert!(reads.contains(&format!("{}@v-agentd", key())), "{reads:?}");
    assert!(!reads.contains(&key()), "{reads:?}");
    // The verified bundle holds agentd's head and not the planted one.
    let verified = w.v.blobs.get(&verified_bundle_key(RUN)).unwrap();
    let f = w.root.join("verified-check.bundle");
    std::fs::write(&f, &verified).unwrap();
    let empty = w.root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    git(&empty, &["init", "-q"]);
    assert_eq!(
        git(&empty, &["bundle", "list-heads", f.to_str().unwrap()]),
        format!("{my_head} refs/heads/harness/{RUN}")
    );
}

#[tokio::test]
async fn b759_a_headsha_that_only_the_planted_bundle_holds_is_refused() {
    // The request names the planted head, but the recorded version is agentd's: the head is not in it.
    let w = world();
    let (mut req, _, planted_head) = w.agentd_then_planted();
    req.head_sha = planted_head.clone();
    let out = w.v.validate(&req).await.unwrap();
    assert!(!out.pass);
    assert_eq!(out.violations[0].rule, Rule::Bundle);
    assert!(
        out.violations[0].detail.contains(&planted_head),
        "{:?}",
        out.violations
    );
    assert!(w.v.blobs.get(&verified_bundle_key(RUN)).is_none());
}

#[tokio::test]
async fn b759_a_recorded_version_that_is_gone_fails_the_bundle_rule_and_writes_nothing() {
    let w = world();
    let (mut req, _, _) = w.agentd_then_planted();
    req.source_bundle_version_id = Some("v-expired".into());
    let out = w.v.validate(&req).await.unwrap();
    assert!(!out.pass);
    assert_eq!(out.violations.len(), 1);
    assert_eq!(out.violations[0].rule, Rule::Bundle);
    assert!(
        out.violations[0].detail.contains("does not exist"),
        "{:?}",
        out.violations
    );
    assert_eq!(out.verified_bundle_uri, None);
    assert!(w.v.blobs.get(&verified_bundle_key(RUN)).is_none());
    // It did not try the current object instead.
    assert_eq!(
        w.v.blobs.reads.lock().unwrap().clone(),
        vec![format!("{}@v-expired", key())]
    );
}

#[tokio::test]
async fn b759_a_digest_that_differs_fails_closed_for_the_recorded_version() {
    let w = world();
    let (mut req, _, _) = w.agentd_then_planted();
    req.source_bundle_sha256 = Some(sha256_hex(b"not the bundle"));
    let out = w.v.validate(&req).await.unwrap();
    assert!(!out.pass);
    assert_eq!(out.violations[0].rule, Rule::Bundle);
    assert!(
        out.violations[0].detail.contains("SHA-256 differs"),
        "{:?}",
        out.violations
    );
    assert!(w.v.blobs.get(&verified_bundle_key(RUN)).is_none());
}

#[tokio::test]
async fn b759_a_record_with_a_digest_and_no_version_holds_the_current_object_to_the_digest() {
    // An answer from S3 with no version id (a bucket that keeps no versions): the current object is read and must be the recorded bytes.
    let w = world();
    commit(&w.work, &[("src/calc.py", Some("x = 4\n"))], "agentd turn");
    let (mine, my_head) = w.bundle("mine");
    w.v.blobs.insert(&key(), mine.clone());
    let mut req = w.req(&my_head);
    req.source_bundle_sha256 = Some(sha256_hex(&mine));
    let out = w.v.validate(&req).await.unwrap();
    assert!(out.pass, "{:?}", out.violations);
    // Someone writes another object over it: the digest no longer holds.
    commit(
        &w.work,
        &[("src/calc.py", Some("x = 5  # planted\n"))],
        "planted",
    );
    let (planted, _) = w.bundle("planted");
    w.v.blobs.insert(&key(), planted);
    let out = w.v.validate(&req).await.unwrap();
    assert!(!out.pass);
    assert_eq!(out.violations[0].rule, Rule::Bundle);
}

#[tokio::test]
async fn b759_a_version_without_its_digest_is_a_request_built_wrong() {
    let w = world();
    let (mut req, _, _) = w.agentd_then_planted();
    req.source_bundle_sha256 = None;
    let e = w.v.validate(&req).await.unwrap_err();
    assert!(matches!(e, DeliveryError::Invalid(_)), "{e:?}");
    assert!(w.v.blobs.reads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b759_a_record_without_a_source_bundle_is_a_request_built_wrong() {
    let w = world();
    let (mut req, _, _) = w.agentd_then_planted();
    req.source_bundle_uri = None;
    let e = w.v.validate(&req).await.unwrap_err();
    assert!(matches!(e, DeliveryError::Invalid(_)), "{e:?}");
    // A digest that is not 64 lowercase hex digits is the same.
    let (mut req, _, _) = w.agentd_then_planted();
    req.source_bundle_sha256 = Some("ABC".into());
    let e = w.v.validate(&req).await.unwrap_err();
    assert!(matches!(e, DeliveryError::Invalid(_)), "{e:?}");
}

/// A client that has the two original reads and no more, as every implementation of `Blobs` had before versions.
struct NoVersions(MemBlobs);

impl Blobs for NoVersions {
    async fn get_to_file(&self, key: &str, path: &Path) -> Result<Option<u64>, DeliveryError> {
        self.0.get_to_file(key, path).await
    }
    async fn put_file(&self, key: &str, path: &Path, meta: &PutMeta) -> Result<(), DeliveryError> {
        self.0.put_file(key, path, meta).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>, DeliveryError> {
        self.0.list(prefix).await
    }
}

#[tokio::test]
async fn b759_a_client_that_cannot_read_a_version_is_unavailable_and_never_reads_the_current_object()
 {
    let w = world();
    let (req, _, _) = w.agentd_then_planted();
    // The same objects behind a client with no version read.
    let inner = MemBlobs::default();
    inner.insert(&key(), w.v.blobs.get(&key()).unwrap());
    let home = w.root.join("home");
    let v = Validator {
        blobs: NoVersions(inner),
        scanner: Gitleaks {
            bin: std::env::var_os("GITLEAKS_BIN")
                .map_or_else(|| PathBuf::from("gitleaks"), PathBuf::from),
            home: home.clone(),
        },
        git: Git::new("git", &home),
        bucket: BUCKET.into(),
        work_root: w.root.join("scratch-nv"),
    };
    let e = v.validate(&req).await.unwrap_err();
    assert!(matches!(e, DeliveryError::Unavailable(_)), "{e:?}");
    assert!(
        v.blobs.0.reads.lock().unwrap().is_empty(),
        "the current object was read: {:?}",
        v.blobs.0.reads.lock().unwrap()
    );
}
