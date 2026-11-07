//! The record of what a delivery changed: when the bundle validator passes a delivery it writes
//! `runs/<run>/deliverable/changes.json` beside the verified bundle, from the same `base..head` changes it checked. The control-api head route
//! reads it so the follow-up digest can list the files the run changed. The record names the run and the head, lists at most 200 paths in git's
//! order with their status, says how many it left out, and never stops a delivery: a pass without the record is still a pass.
//!
//! The shape is the contract `ham_core::head::ChangeList::from_record` reads; `crates/ham-core/tests/fixtures/changes/changes-v1.json` is the
//! file both sides are tested against.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::changes::{self, MAX_FILES, MAX_PATH_BYTES, SCHEMA_VERSION};
use delivery::error::DeliveryError;
use delivery::git::{Change, Git};
use delivery::ports::{Blobs, PutMeta};
use delivery::validator::{Finding, Limits, SecretScanner, ValidateRequest, Validator};
use harness_protocol::store::s3::verified_bundle_key;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE_SHA: &str = "1111111111111111111111111111111111111111";

fn ch(status: char, path: &str) -> Change {
    Change {
        status,
        path: path.into(),
    }
}

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

// ---- the record --------------------------------------------------------------------------------------------------------------------

#[test]
fn the_key_sits_beside_the_verified_bundle() {
    assert_eq!(
        changes::changes_key(RUN),
        format!("runs/{RUN}/deliverable/changes.json")
    );
    let bundle = verified_bundle_key(RUN);
    assert_eq!(
        changes::changes_key(RUN).rsplit_once('/').unwrap().0,
        bundle.rsplit_once('/').unwrap().0
    );
}

#[test]
fn a_record_names_the_run_the_head_the_base_and_each_path_with_its_status() {
    let list = [
        ch('A', "README.md"),
        ch('A', "hello.py"),
        ch('D', "old.txt"),
    ];
    let v = parse(&changes::record(RUN, HEAD, Some(BASE_SHA), &list));
    assert_eq!(
        v,
        json!({"schemaVersion": SCHEMA_VERSION, "runId": RUN, "headSha": HEAD, "base": {"kind": "commit", "sha": BASE_SHA},
               "total": 3, "omitted": 0,
               "files": [{"path": "README.md", "status": "A"}, {"path": "hello.py", "status": "A"}, {"path": "old.txt", "status": "D"}]})
    );
}

#[test]
fn a_run_with_no_base_started_from_an_empty_tree() {
    let v = parse(&changes::record(RUN, HEAD, None, &[ch('A', "a")]));
    assert_eq!(v["base"], json!({"kind": "empty-tree"}));
}

#[test]
fn a_delivery_that_changed_nothing_is_a_record_with_no_files_and_not_a_missing_record() {
    let v = parse(&changes::record(RUN, HEAD, Some(BASE_SHA), &[]));
    assert_eq!(
        (v["total"].as_u64(), v["omitted"].as_u64()),
        (Some(0), Some(0))
    );
    assert_eq!(v["files"], json!([]));
}

#[test]
fn the_paths_are_in_path_order_whatever_order_git_gave_them() {
    let v = parse(&changes::record(
        RUN,
        HEAD,
        None,
        &[ch('A', "z.py"), ch('M', "a/b.py"), ch('A', "a.py")],
    ));
    let paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["a.py", "a/b.py", "z.py"]);
}

#[test]
fn a_long_list_is_cut_at_2000_and_says_how_many_it_left_out() {
    assert_eq!(MAX_FILES, 2_000);
    let list: Vec<Change> = (0..MAX_FILES + 50)
        .map(|i| ch('A', &format!("src/f{i:03}.py")))
        .collect();
    let v = parse(&changes::record(RUN, HEAD, None, &list));
    assert_eq!(v["files"].as_array().unwrap().len(), MAX_FILES);
    assert_eq!(
        (v["total"].as_u64(), v["omitted"].as_u64()),
        (Some(MAX_FILES as u64 + 50), Some(50))
    );
    assert_eq!(
        v["files"][0]["path"], "src/f000.py",
        "the first in path order stay"
    );
}

#[test]
fn a_path_that_is_too_long_or_holds_a_nul_is_counted_and_not_listed() {
    let long = "x".repeat(MAX_PATH_BYTES + 1);
    let at_limit = "y".repeat(MAX_PATH_BYTES);
    let list = [
        ch('A', &long),
        ch('A', &at_limit),
        ch('A', "a\0b"),
        ch('A', "ok.py"),
    ];
    let v = parse(&changes::record(RUN, HEAD, None, &list));
    let paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["ok.py", at_limit.as_str()]);
    assert_eq!(
        (v["total"].as_u64(), v["omitted"].as_u64()),
        (Some(4), Some(2))
    );
}

#[test]
fn the_status_letters_are_the_ones_git_writes() {
    let v = parse(&changes::record(
        RUN,
        HEAD,
        None,
        &[ch('A', "a"), ch('M', "b"), ch('D', "c"), ch('T', "d")],
    ));
    let s: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["status"].as_str().unwrap())
        .collect();
    assert_eq!(s, ["A", "M", "D", "T"]);
}

/// The fixture the client side parses (`ham-core`'s tests): the record for the same facts is the same JSON.
#[test]
fn the_record_is_the_shape_the_client_reads() {
    let fixture: Value = parse(
        &std::fs::read(format!(
            "{}/../ham-core/tests/fixtures/changes/changes-v1.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    );
    // The fixture lists two of three paths and says one was left out; the writer's output for 3 paths has all three.
    let mut v = parse(&changes::record(
        fixture["runId"].as_str().unwrap(),
        fixture["headSha"].as_str().unwrap(),
        None,
        &[ch('A', "README.md"), ch('A', "hello.py")],
    ));
    v["total"] = fixture["total"].clone();
    v["omitted"] = fixture["omitted"].clone();
    assert_eq!(v, fixture);
}

// ---- the validator writes it ---------------------------------------------------------------------------------------------------------

struct NoScan;

impl SecretScanner for NoScan {
    async fn scan(&self, _repo: &Path, _range: &str) -> Result<Vec<Finding>, DeliveryError> {
        Ok(vec![])
    }
}

/// Writes of the blob store that fail, for the key under test.
struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    base: String,
    v: Validator<MemBlobs, NoScan>,
}

fn world() -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (work, shas) = work_repo(
        &root,
        "work",
        &[&[
            ("src/calc.py", "def add(a, b):\n    return a + b\n"),
            ("README.md", "calc\n"),
        ]],
    );
    let v = Validator {
        blobs: MemBlobs::default(),
        scanner: NoScan,
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

impl World {
    fn turn(&self, files: &[(&str, Option<&str>)]) -> String {
        commit(&self.work, files, "agent turn");
        let f = self.root.join("verified.bundle");
        let head = verified_bundle(&self.work, RUN, &f);
        self.v
            .blobs
            .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
        head
    }
    fn req(&self, head: &str, base: Option<&str>, globs: &[&str]) -> ValidateRequest {
        ValidateRequest {
            run_id: RUN.into(),
            base_sha: base.map(str::to_string),
            head_sha: head.into(),
            allowed_globs: globs.iter().map(|g| (*g).to_string()).collect(),
            limits: Limits::default(),
            source_bundle_uri: None,
            engine_config: Default::default(),
            third_party: Default::default(),
        }
    }
}

#[tokio::test]
async fn a_passing_validation_writes_the_record_of_the_changes_it_checked() {
    let w = world();
    let head = w.turn(&[
        ("src/new.py", Some("print(1)\n")),
        ("src/calc.py", Some("def add(a, b):\n    return b + a\n")),
        ("README.md", None),
    ]);
    let out =
        w.v.validate(&w.req(&head, Some(&w.base), &["src/**", "README.md"]))
            .await
            .unwrap();
    assert!(out.pass, "{:?}", out.violations);
    let bytes =
        w.v.blobs
            .get(&changes::changes_key(RUN))
            .expect("the record");
    let v = parse(&bytes);
    assert_eq!(v["runId"], RUN);
    assert_eq!(v["headSha"], head);
    assert_eq!(v["base"], json!({"kind": "commit", "sha": w.base}));
    assert_eq!(
        v["files"],
        json!([
            {"path": "README.md", "status": "D"},
            {"path": "src/calc.py", "status": "M"},
            {"path": "src/new.py", "status": "A"}
        ])
    );
    assert_eq!(
        v["total"], out.changed_files,
        "the count the validator returns is the record's total"
    );
    let meta: PutMeta = w.v.blobs.meta(&changes::changes_key(RUN)).unwrap();
    assert_eq!(meta.content_type, "application/json");
}

#[tokio::test]
async fn a_run_with_no_base_lists_every_file_as_an_addition_from_an_empty_tree() {
    let w = world();
    let head = w.turn(&[("src/hello.py", Some("print('hi')\n"))]);
    let out =
        w.v.validate(&w.req(&head, None, &["src/**", "README.md"]))
            .await
            .unwrap();
    assert!(out.pass, "{:?}", out.violations);
    let v = parse(&w.v.blobs.get(&changes::changes_key(RUN)).unwrap());
    assert_eq!(v["base"], json!({"kind": "empty-tree"}));
    let paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["README.md", "src/calc.py", "src/hello.py"]);
}

#[tokio::test]
async fn a_failing_validation_writes_no_record() {
    let w = world();
    let head = w.turn(&[("setup.py", Some("from setuptools import setup\n"))]);
    let out =
        w.v.validate(&w.req(&head, Some(&w.base), &["src/**"]))
            .await
            .unwrap();
    assert!(!out.pass);
    assert!(
        w.v.blobs.get(&changes::changes_key(RUN)).is_none(),
        "no delivery follows a failed validation, so nothing describes one"
    );
}

#[tokio::test]
async fn the_record_is_rewritten_for_the_head_of_a_new_validation() {
    let w = world();
    let first = w.turn(&[("src/a.py", Some("a = 1\n"))]);
    w.v.validate(&w.req(&first, Some(&w.base), &["src/**"]))
        .await
        .unwrap();
    let second = w.turn(&[("src/b.py", Some("b = 2\n"))]);
    w.v.validate(&w.req(&second, Some(&w.base), &["src/**"]))
        .await
        .unwrap();
    let v = parse(&w.v.blobs.get(&changes::changes_key(RUN)).unwrap());
    assert_eq!(v["headSha"], second);
}

/// A bucket that refuses the write of the record: the validation is a pass all the same.
struct RefusingBlobs {
    inner: MemBlobs,
}

impl Blobs for RefusingBlobs {
    async fn get_to_file(&self, key: &str, path: &Path) -> Result<Option<u64>, DeliveryError> {
        self.inner.get_to_file(key, path).await
    }
    async fn put_file(&self, key: &str, path: &Path, meta: &PutMeta) -> Result<(), DeliveryError> {
        if key.ends_with("changes.json") {
            return Err(DeliveryError::AccessDenied(format!(
                "s3 put {key}: AccessDenied"
            )));
        }
        self.inner.put_file(key, path, meta).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>, DeliveryError> {
        self.inner.list(prefix).await
    }
}

#[tokio::test]
async fn a_record_that_cannot_be_written_never_fails_the_delivery() {
    let w = world();
    let head = w.turn(&[("src/new.py", Some("print(1)\n"))]);
    let blobs = RefusingBlobs {
        inner: MemBlobs::default(),
    };
    blobs.inner.insert(
        &verified_bundle_key(RUN),
        w.v.blobs.get(&verified_bundle_key(RUN)).unwrap(),
    );
    let v = Validator {
        blobs,
        scanner: NoScan,
        git: Git::new("git", w.root.join("home")),
        bucket: BUCKET.into(),
        work_root: w.root.join("scratch2"),
    };
    let out = v
        .validate(&w.req(&head, Some(&w.base), &["src/**"]))
        .await
        .unwrap();
    assert!(out.pass, "{:?}", out.violations);
    assert!(v.blobs.inner.get(&changes::changes_key(RUN)).is_none());
}

#[test]
fn b776_the_listed_paths_are_held_to_half_a_mebibyte_of_text_in_all() {
    use delivery::changes::MAX_LISTED_PATH_BYTES;
    let list: Vec<Change> = (0..MAX_FILES)
        .map(|i| ch('A', &format!("{i:04}{}", "d/".repeat(498))))
        .collect();
    let v = parse(&changes::record(RUN, HEAD, None, &list));
    let listed = v["files"].as_array().unwrap().len();
    let bytes: usize = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap().len())
        .sum();
    assert!(bytes <= MAX_LISTED_PATH_BYTES, "{bytes}");
    assert!(listed < MAX_FILES && listed > 400, "{listed}");
    assert_eq!(v["total"].as_u64(), Some(MAX_FILES as u64));
    assert_eq!(
        v["omitted"].as_u64(),
        Some((MAX_FILES - listed) as u64),
        "what is not listed is counted"
    );
}
