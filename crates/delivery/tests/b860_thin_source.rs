//! The bundle validator builds the verified bundle from a source that needs the bundle the run started from.
//!
//! A saved copy of a run's work may hold what the run changed and leave out what the run's base holds; git names the commits it left
//! out in the copy's header. A cancelled or lost run delivers its last saved copy, so the validator takes the base first
//! (`runs/<run>/input/repo.bundle`), then the source, and writes a verified bundle that holds the whole history, as it always did. A
//! source that needs nothing reads no base. A base that is not there is a `bundle` violation that says so, and nothing is written.
//!
//! The second test pins the fact the worker's copies rest on: the base object of a run names the same commits whenever it is staged
//! again, and needs nothing itself.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::gateway::{Gateway, StageRequest, StageTarget};
use delivery::git::Git;
use delivery::validator::{Gitleaks, Limits, Rule, ValidateRequest, Validator};
use harness_protocol::bundle_header::{HEADER_BYTES_MAX, heads, needs};
use harness_protocol::store::s3::{repo_bundle_key, verified_bundle_key};
use std::path::{Path, PathBuf};

const COPY_SUFFIX: &str = "agents/direct/turns/1/snapshot.bundle";

fn copy_key() -> String {
    format!("runs/{RUN}/{COPY_SUFFIX}")
}

fn prefix(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.len().min(usize::try_from(HEADER_BYTES_MAX).unwrap())]
}

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    base: String,
    v: Validator<MemBlobs, Gitleaks>,
}

/// A repository with a history of two commits, and the bundle a run starts from as the service stages it (`HEAD` and the branch).
fn world() -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (work, shas) = work_repo(
        &root,
        "work",
        &[
            &[("src/calc.py", "def add(a, b):\n    return a + b\n")],
            &[("src/more.py", "def sub(a, b):\n    return a - b\n")],
        ],
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
        base: shas[1].clone(),
        v,
    }
}

impl World {
    /// The bundle the run started from, as `stage` writes it.
    fn base_bundle(&self) -> Vec<u8> {
        let f = self.root.join("repo.bundle");
        git(
            &self.work,
            &[
                "bundle",
                "create",
                "-q",
                f.to_str().unwrap(),
                "HEAD",
                "refs/heads/main",
            ],
        );
        std::fs::read(&f).unwrap()
    }

    /// The work of the run: one commit on the base. Returns its commit.
    fn run_works(&self) -> String {
        commit(
            &self.work,
            &[("src/calc.py", Some("def add(a, b):\n    return b + a\n"))],
            "the run's work",
        )
    }

    /// A saved copy of the work at `head`, on the ref every copy uses: whole, or without what the base holds.
    fn copy(&self, label: &str, head: &str, above_base: bool) -> Vec<u8> {
        git(&self.work, &["branch", "-f", "snapshot", head]);
        let f = self.root.join(format!("{label}.bundle"));
        let not_base = format!("^{}", self.base);
        let mut args = vec![
            "bundle",
            "create",
            "-q",
            "--version=2",
            f.to_str().unwrap(),
            "refs/heads/snapshot",
        ];
        if above_base {
            args.push(&not_base);
        }
        git(&self.work, &args);
        std::fs::read(&f).unwrap()
    }

    fn req(&self, head: &str) -> ValidateRequest {
        ValidateRequest {
            run_id: RUN.into(),
            base_sha: Some(self.base.clone()),
            head_sha: head.into(),
            allowed_globs: vec!["**".into(), "!.git/**".into()],
            limits: Limits::default(),
            source_bundle_uri: Some(format!("s3://{BUCKET}/{}", copy_key())),
            source_bundle_version_id: None,
            source_bundle_sha256: None,
            engine_config: Default::default(),
            third_party: Default::default(),
            target: Default::default(),
            source_run_ids: Vec::new(),
        }
    }

    fn reads(&self) -> Vec<String> {
        self.v.blobs.reads.lock().unwrap().clone()
    }
}

/// The commit the verified bundle names for the run's branch, read from a clone of it into an empty repository.
fn verified_head(root: &Path, label: &str, verified: &[u8]) -> String {
    let f = root.join(format!("{label}-verified.bundle"));
    std::fs::write(&f, verified).unwrap();
    let into = root.join(format!("{label}-clone"));
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            f.to_str().unwrap(),
            into.to_str().unwrap(),
        ],
    );
    git(&into, &["fsck", "--no-progress", "--connectivity-only"]);
    git(&into, &["rev-parse", &format!("refs/heads/harness/{RUN}")])
}

#[tokio::test]
async fn b860_validator_builds_the_verified_bundle_from_a_thin_source() {
    // A source that needs the base, and the base in the bucket.
    let w = world();
    let base_key = repo_bundle_key(RUN);
    w.v.blobs.insert(&base_key, w.base_bundle());
    let head = w.run_works();
    let thin = w.copy("thin", &head, true);
    assert_eq!(
        needs(prefix(&thin)).unwrap(),
        vec![w.base.clone()],
        "B860 TRIGGER PRESENT: the source names the base commit as a commit it needs"
    );
    w.v.blobs.insert(&copy_key(), thin);
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert!(out.pass, "{:?}", out.violations);
    assert_eq!(out.changed_files, 1);
    // The base was read after the source, once.
    assert_eq!(
        w.reads()
            .iter()
            .filter(|k| **k == copy_key() || **k == base_key)
            .cloned()
            .collect::<Vec<_>>(),
        vec![copy_key(), base_key.clone()]
    );
    // The verified bundle holds the whole history: it needs nothing, clones alone into an empty repository and names the head.
    let verified = w.v.blobs.get(&verified_bundle_key(RUN)).unwrap();
    assert!(needs(prefix(&verified)).unwrap().is_empty());
    assert_eq!(verified_head(&w.root, "thin", &verified), head);

    // A source that holds the whole history: the base is not read, whether or not it is there.
    let w = world();
    w.v.blobs.insert(&base_key, w.base_bundle());
    let head = w.run_works();
    let whole = w.copy("whole", &head, false);
    assert!(needs(prefix(&whole)).unwrap().is_empty());
    w.v.blobs.insert(&copy_key(), whole);
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert!(out.pass, "{:?}", out.violations);
    assert!(
        !w.reads().contains(&base_key),
        "a source that needs nothing reads no base: {:?}",
        w.reads()
    );
    let verified = w.v.blobs.get(&verified_bundle_key(RUN)).unwrap();
    assert_eq!(verified_head(&w.root, "whole", &verified), head);

    // A source that needs the base, and no base in the bucket: a bundle violation that says so, and nothing is written.
    let w = world();
    let head = w.run_works();
    w.v.blobs
        .insert(&copy_key(), w.copy("no-base", &head, true));
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert!(!out.pass);
    assert_eq!(out.violations.len(), 1, "{:?}", out.violations);
    assert_eq!(out.violations[0].rule, Rule::Bundle);
    let uri = format!("s3://{BUCKET}/{}", copy_key());
    assert_eq!(
        out.violations[0].detail,
        format!(
            "the source bundle {uri} needs the run's base bundle, which could not be read: no object at s3://{BUCKET}/{base_key}"
        )
    );
    assert!(w.v.blobs.get(&verified_bundle_key(RUN)).is_none());
    assert!(out.verified_bundle_uri.is_none());

    // The base in the bucket is not the one the source was written on (it does not hold the commit the source needs): git refuses
    // the source, as it refuses any source that does not verify, and nothing is written.
    let w = world();
    let (other, _) = work_repo(&w.root, "other", &[&[("z.txt", "another history\n")]]);
    let other_base = w.root.join("other.bundle");
    git(
        &other,
        &[
            "bundle",
            "create",
            "-q",
            other_base.to_str().unwrap(),
            "HEAD",
            "refs/heads/main",
        ],
    );
    w.v.blobs
        .insert(&base_key, std::fs::read(&other_base).unwrap());
    let head = w.run_works();
    w.v.blobs
        .insert(&copy_key(), w.copy("wrong-base", &head, true));
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert!(!out.pass);
    assert_eq!(out.violations[0].rule, Rule::Bundle);
    assert!(
        out.violations[0]
            .detail
            .starts_with(&format!("the source bundle {uri} does not verify: ")),
        "{}",
        out.violations[0].detail
    );
    assert!(w.v.blobs.get(&verified_bundle_key(RUN)).is_none());
}

#[tokio::test]
async fn b860_a_base_staged_twice_names_the_same_commits() {
    // The repository a run targets, as CodeCommit would hold it: main with two commits.
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let remotes_dir = root.join("codecommit");
    let work_root = root.join("work");
    let home = root.join("home");
    for d in [&remotes_dir, &work_root, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    let target = format!("cc:{FIXTURE}");
    let gw = Gateway {
        codecommit: FakeCodeCommit::new(&remotes_dir),
        allowlist: FakeAllowlist::with(&[target.as_str()]),
        blobs: MemBlobs::default(),
        remotes: LocalRemotes::new(&remotes_dir),
        git: Git::new("git", &home),
        stage: "live".into(),
        owner: "team-a".into(),
        bucket: BUCKET.into(),
        work_root,
        clock: || 1_790_337_600,
        repo_names: Default::default(),
    };
    let (work, shas) = work_repo(
        &root,
        "fixture-work",
        &[
            &[("calc.py", "def add(a, b):\n    return a + b\n")],
            &[("tests/test_calc.py", "def test_add():\n    assert True\n")],
        ],
    );
    publish(&work, &remotes_dir, FIXTURE);
    let req = StageRequest {
        run_id: RUN.into(),
        target: StageTarget::Repo(target.parse().unwrap()),
        base_ref: None,
        from_run: None,
        container_signals: None,
    };
    let key = repo_bundle_key(RUN);

    let first = gw.stage(&req).await.unwrap();
    let first_object = gw.blobs.get(&key).unwrap();
    // A stage that is tried again for the same run.
    let second = gw.stage(&req).await.unwrap();
    let second_object = gw.blobs.get(&key).unwrap();

    assert_eq!(first.base_sha, shas[1]);
    assert_eq!(second.base_sha, first.base_sha);
    for (name, object) in [("first", &first_object), ("second", &second_object)] {
        // The base needs nothing, and its header ends inside the bytes a reader takes.
        assert!(
            needs(prefix(object)).unwrap().is_empty(),
            "{name}: the base of a run needs no other bundle"
        );
        // `HEAD` and the branch: one commit under two names, the commit the run starts from.
        assert_eq!(
            heads(prefix(object)).unwrap(),
            vec![first.base_sha.clone()],
            "{name}"
        );
    }
    assert_eq!(
        heads(prefix(&first_object)).unwrap(),
        heads(prefix(&second_object)).unwrap()
    );
}
