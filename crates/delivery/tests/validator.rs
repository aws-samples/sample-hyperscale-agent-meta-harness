//! `bundle-validator`:
//! on the verified bundle of `harness/<runId>` it checks the branch,
//! the history from base, every changed path against the allowed globs, that
//! `.github/workflows/**`, `buildspec.yml` and the engine-config paths are unchanged, the size limits, and gitleaks
//! over `base..head`. It returns pass or fail with every violation and never the secret itself.
//!
//! The AKIA-format key is assembled at test runtime.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::error::DeliveryError;
use delivery::git::{Change, Git, TreeEntry};
use delivery::validator::{
    Gitleaks, Limits, Protected, Rule, ValidateRequest, Validator, evaluate, path_allowed,
    protected,
};
use harness_protocol::store::s3::verified_bundle_key;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn gitleaks_bin() -> PathBuf {
    std::env::var_os("GITLEAKS_BIN").map_or_else(|| PathBuf::from("gitleaks"), PathBuf::from)
}

/// An AWS access key id pattern built at runtime.
fn planted_key() -> String {
    format!("{}{}", "AKIA", "Q3VZ7WJ2XK5M4NPL")
}

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
    let scratch = root.join("scratch");
    std::fs::create_dir_all(&home).unwrap();
    let (work, shas) = work_repo(
        &root,
        "work",
        &[&[
            ("src/calc.py", "def add(a, b):\n    return a + b\n"),
            ("tests/test_calc.py", "from src.calc import add\n"),
            ("buildspec.yml", "version: 0.2\n"),
            (".github/workflows/ci.yml", "on: push\n"),
            ("README.md", "calc\n"),
        ]],
    );
    let v = Validator {
        blobs: MemBlobs::default(),
        scanner: Gitleaks {
            bin: gitleaks_bin(),
            home: home.clone(),
        },
        git: Git::new("git", &home),
        bucket: BUCKET.into(),
        work_root: scratch,
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
    /// Commits the change as the agent's turn and stages the verified bundle; returns the head.
    fn turn(&self, files: &[(&str, Option<&str>)]) -> String {
        commit(&self.work, files, "agent turn");
        self.stage_bundle()
    }
    fn stage_bundle(&self) -> String {
        let f = self.root.join("verified.bundle");
        let head = verified_bundle(&self.work, RUN, &f);
        self.v
            .blobs
            .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
        head
    }
    fn req(&self, head: &str) -> ValidateRequest {
        ValidateRequest {
            run_id: RUN.into(),
            base_sha: Some(self.base.clone()),
            head_sha: head.into(),
            allowed_globs: vec!["src/**".into(), "tests/**".into()],
            limits: Limits::default(),
            source_bundle_uri: None,
            engine_config: Default::default(),
            third_party: Default::default(),
        }
    }
}

fn rules(out: &delivery::validator::ValidateOutput) -> Vec<(Rule, Option<String>)> {
    out.violations
        .iter()
        .map(|v| (v.rule, v.path.clone()))
        .collect()
}

// ---- pure rules ---------------------------------------------------------------------------

#[test]
fn protected_paths_are_workflows_the_root_buildspec_and_engine_config() {
    assert_eq!(
        protected(".github/workflows/ci.yml"),
        Some(Protected::Workflow)
    );
    assert_eq!(
        protected(".github/workflows/sub/x.yaml"),
        Some(Protected::Workflow)
    );
    assert_eq!(protected("buildspec.yml"), Some(Protected::Buildspec));
    for p in [
        ".cline/hooks/PreToolUse",
        ".cline/rules/x.md",
        ".clinerules/hooks/x",
        ".clinerules/style.md",
        "opencode.json",
        ".opencode/agent/x.md",
    ] {
        assert_eq!(protected(p), Some(Protected::EngineConfig), "{p}");
    }
    for p in [
        "src/buildspec.yml",
        ".github/CODEOWNERS",
        "docs/opencode.json",
        "cline/x",
        "src/.cline",
        ".clinerulesx",
    ] {
        assert_eq!(protected(p), None, "{p}");
    }
}

#[test]
fn a_path_is_allowed_by_the_last_matching_glob() {
    let g: Vec<String> = ["src/**", "!src/vendor/**", "tests/**"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(path_allowed(&g, "src/calc.py"));
    assert!(path_allowed(&g, "src/pkg/deep/mod.py"));
    assert!(!path_allowed(&g, "src/vendor/lib.py"));
    assert!(path_allowed(&g, "tests/test_calc.py"));
    assert!(!path_allowed(&g, "setup.py"));
    assert!(!path_allowed(&[], "src/calc.py"), "no globs allows nothing");
}

fn entry(path: &str, sha: &str, size: u64) -> (String, TreeEntry) {
    (
        path.into(),
        TreeEntry {
            mode: "100644".into(),
            kind: "blob".into(),
            sha: sha.into(),
            size: Some(size),
            path: path.into(),
        },
    )
}

#[test]
fn evaluate_reports_each_rule_with_its_path() {
    let changes = vec![
        Change {
            status: 'M',
            path: "src/calc.py".into(),
        },
        Change {
            status: 'A',
            path: "setup.py".into(),
        },
        Change {
            status: 'A',
            path: ".github/workflows/deploy.yml".into(),
        },
        Change {
            status: 'M',
            path: "buildspec.yml".into(),
        },
        Change {
            status: 'D',
            path: ".clinerules/style.md".into(),
        },
        Change {
            status: 'A',
            path: "src/big.bin".into(),
        },
    ];
    let tree: BTreeMap<String, TreeEntry> = [
        entry("src/calc.py", &"1".repeat(40), 10),
        entry("setup.py", &"2".repeat(40), 10),
        entry(".github/workflows/deploy.yml", &"3".repeat(40), 10),
        entry("buildspec.yml", &"4".repeat(40), 10),
        entry("src/big.bin", &"5".repeat(40), 2_000),
    ]
    .into_iter()
    .collect();
    let globs = vec!["src/**".to_string()];
    let limits = Limits {
        max_file_bytes: 1_000,
        ..Limits::default()
    };
    let v = evaluate(&changes, &tree, &globs, &limits);
    let got: Vec<(Rule, Option<String>)> = v.iter().map(|x| (x.rule, x.path.clone())).collect();
    assert_eq!(
        got,
        vec![
            (Rule::PathGlob, Some("setup.py".into())),
            (
                Rule::WorkflowFile,
                Some(".github/workflows/deploy.yml".into())
            ),
            (Rule::Buildspec, Some("buildspec.yml".into())),
            (Rule::EngineConfig, Some(".clinerules/style.md".into())),
            (Rule::FileSize, Some("src/big.bin".into())),
        ]
    );
    assert!(v[4].detail.contains("2000 bytes"), "{}", v[4].detail);
}

// ---- end to end on real repositories ------------------------------------------------------

#[tokio::test]
async fn a_change_inside_the_globs_passes() {
    let w = world();
    let head = w.turn(&[
        (
            "src/calc.py",
            Some("def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\n"),
        ),
        ("tests/test_sub.py", Some("from src.calc import sub\n")),
    ]);
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert!(out.pass, "{:?}", out.violations);
    assert_eq!(out.changed_files, 2);
    assert_eq!(out.head_sha, head);
    assert_eq!(out.base_sha.as_deref(), Some(w.base.as_str()));
}

#[tokio::test]
async fn a_change_outside_the_globs_fails_with_the_path() {
    let w = world();
    let head = w.turn(&[
        ("setup.py", Some("from setuptools import setup\n")),
        ("README.md", None),
    ]);
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert!(!out.pass);
    assert_eq!(
        rules(&out),
        vec![
            (Rule::PathGlob, Some("README.md".into())),
            (Rule::PathGlob, Some("setup.py".into()))
        ]
    );
}

#[tokio::test]
async fn workflow_and_buildspec_changes_fail() {
    let w = world();
    let head = w.turn(&[
        (".github/workflows/ci.yml", Some("on: push\njobs: {}\n")),
        (
            "buildspec.yml",
            Some("version: 0.2\nphases: {build: {commands: [true]}}\n"),
        ),
    ]);
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert!(!out.pass);
    assert_eq!(
        rules(&out),
        vec![
            (Rule::WorkflowFile, Some(".github/workflows/ci.yml".into())),
            (Rule::Buildspec, Some("buildspec.yml".into()))
        ]
    );
}

#[tokio::test]
async fn every_way_to_change_the_root_buildspec_is_refused() {
    // The pull-request check is attached from the delivered head's root buildspec.yml, so a run must not be able to edit, drop,
    // rename, retype or re-mode it. `diff-tree --no-renames` reports each as a change of the path itself.
    type Edit = fn(&Path);
    let cases: [(&str, Edit); 6] = [
        ("edit", |d| {
            std::fs::write(d.join("buildspec.yml"), "version: 0.2\nphases: {}\n").unwrap()
        }),
        ("delete", |d| {
            git(d, &["rm", "-q", "buildspec.yml"]);
        }),
        ("rename away", |d| {
            git(d, &["mv", "buildspec.yml", "src/spec.yml"]);
        }),
        ("replace by a link", |d| {
            git(d, &["rm", "-q", "buildspec.yml"]);
            std::os::unix::fs::symlink("src/calc.py", d.join("buildspec.yml")).unwrap();
        }),
        ("replace by a folder", |d| {
            git(d, &["rm", "-q", "buildspec.yml"]);
            std::fs::create_dir_all(d.join("buildspec.yml")).unwrap();
            std::fs::write(d.join("buildspec.yml/x.yml"), "version: 0.2\n").unwrap();
        }),
        ("make it executable", |d| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                d.join("buildspec.yml"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }),
    ];
    for (name, edit) in cases {
        let w = world();
        edit(&w.work);
        git(&w.work, &["add", "-A"]);
        git(&w.work, &["commit", "-q", "-m", name]);
        let head = w.stage_bundle();
        let out = w.v.validate(&w.req(&head)).await.unwrap();
        assert!(!out.pass, "{name}");
        assert!(
            rules(&out).contains(&(Rule::Buildspec, Some("buildspec.yml".into()))),
            "{name}: {:?}",
            rules(&out)
        );
    }
}

#[tokio::test]
async fn engine_config_changes_fail_even_inside_the_globs() {
    let w = world();
    let head = w.turn(&[
        (".cline/hooks/PreToolUse", Some("#!/bin/sh\ntrue\n")),
        (".clinerules/style.md", Some("Use two spaces.\n")),
        (
            "opencode.json",
            Some("{\"permission\": {\"bash\": \"allow\"}}\n"),
        ),
        (".opencode/agent/x.md", Some("x\n")),
    ]);
    let mut req = w.req(&head);
    req.allowed_globs = vec!["**".into()];
    let out = w.v.validate(&req).await.unwrap();
    assert_eq!(
        rules(&out),
        vec![
            (Rule::EngineConfig, Some(".cline/hooks/PreToolUse".into())),
            (Rule::EngineConfig, Some(".clinerules/style.md".into())),
            (Rule::EngineConfig, Some(".opencode/agent/x.md".into())),
            (Rule::EngineConfig, Some("opencode.json".into())),
        ]
    );
}

#[tokio::test]
async fn size_limits_fail_large_files_and_large_bundles() {
    let w = world();
    let head = w.turn(&[("src/data.txt", Some(&"x".repeat(5_000)))]);
    let mut req = w.req(&head);
    req.limits = Limits {
        max_file_bytes: 4_096,
        max_bundle_bytes: 100,
    };
    let out = w.v.validate(&req).await.unwrap();
    let got = rules(&out);
    assert_eq!(got[0].0, Rule::BundleSize);
    assert_eq!(got[1], (Rule::FileSize, Some("src/data.txt".into())));
    assert_eq!(got.len(), 2);
    assert!(Limits::default().max_file_bytes >= 100 * 1024 * 1024);
}

#[tokio::test]
async fn a_planted_access_key_fails_with_its_location_and_no_secret() {
    let w = world();
    let key = planted_key();
    let head = w.turn(&[(
        "src/settings.py",
        Some(&format!("REGION = 'us-west-2'\nKEY_ID = '{key}'\n")),
    )]);
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert!(!out.pass);
    let secrets: Vec<_> = out
        .violations
        .iter()
        .filter(|v| v.rule == Rule::Secret)
        .collect();
    assert_eq!(secrets.len(), 1, "{:?}", out.violations);
    assert_eq!(secrets[0].path.as_deref(), Some("src/settings.py"));
    assert!(
        secrets[0].detail.contains("aws-access-token"),
        "{}",
        secrets[0].detail
    );
    assert!(
        secrets[0].detail.contains("line 2"),
        "{}",
        secrets[0].detail
    );
    assert!(
        secrets[0].detail.contains(&head[..7]),
        "{}",
        secrets[0].detail
    );
    let shown = serde_json::to_string(&out).unwrap();
    assert!(
        !shown.contains(&key),
        "the secret never leaves the scanner: {shown}"
    );
}

#[tokio::test]
async fn a_key_added_then_removed_inside_the_range_still_fails() {
    let w = world();
    let key = planted_key();
    commit(
        &w.work,
        &[("src/settings.py", Some(&format!("KEY_ID = '{key}'\n")))],
        "oops",
    );
    let head = w.turn(&[("src/settings.py", Some("KEY_ID = None\n"))]);
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert!(
        out.violations.iter().any(|v| v.rule == Rule::Secret),
        "{:?}",
        out.violations
    );
    // A gitleaks:allow comment does not hide it either.
    let w2 = world();
    let head2 = w2.turn(&[(
        "src/settings.py",
        Some(&format!("KEY_ID = '{key}'  # gitleaks:allow\n")),
    )]);
    let out2 = w2.v.validate(&w2.req(&head2)).await.unwrap();
    assert!(
        out2.violations.iter().any(|v| v.rule == Rule::Secret),
        "{:?}",
        out2.violations
    );
}

#[tokio::test]
async fn a_bundle_without_the_harness_branch_or_at_another_head_fails_the_branch_check() {
    let w = world();
    commit(&w.work, &[("src/calc.py", Some("x = 1\n"))], "turn");
    let head = git(&w.work, &["rev-parse", "HEAD"]);
    let f = w.root.join("main.bundle");
    git(
        &w.work,
        &["bundle", "create", "-q", f.to_str().unwrap(), "main"],
    );
    w.v.blobs
        .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert_eq!(rules(&out), vec![(Rule::Branch, None)]);
    assert!(
        out.violations[0]
            .detail
            .contains(&format!("refs/heads/harness/{RUN}"))
    );
    let staged = w.stage_bundle();
    let out = w.v.validate(&w.req(&w.base)).await.unwrap();
    assert_eq!(rules(&out), vec![(Rule::Branch, None)]);
    assert!(out.violations[0].detail.contains(&staged));
}

#[tokio::test]
async fn a_missing_or_corrupt_bundle_fails_the_bundle_check() {
    let w = world();
    let out = w.v.validate(&w.req(&"a".repeat(40))).await.unwrap();
    assert_eq!(rules(&out), vec![(Rule::Bundle, None)]);
    w.v.blobs
        .insert(&verified_bundle_key(RUN), b"garbage".to_vec());
    let out = w.v.validate(&w.req(&"a".repeat(40))).await.unwrap();
    assert_eq!(rules(&out), vec![(Rule::Bundle, None)]);
    assert!(out.violations[0].detail.contains("does not verify"));
}

#[tokio::test]
async fn a_head_that_does_not_descend_from_base_fails_the_history_check() {
    let w = world();
    git(&w.work, &["checkout", "-q", "--orphan", "other"]);
    git(&w.work, &["rm", "-rqf", "."]);
    let head = w.turn(&[("src/calc.py", Some("x = 2\n"))]);
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert_eq!(rules(&out)[0], (Rule::History, None));
    let mut unknown = w.req(&head);
    unknown.base_sha = Some("c".repeat(40));
    let out = w.v.validate(&unknown).await.unwrap();
    assert_eq!(rules(&out), vec![(Rule::History, None)]);
}

#[tokio::test]
async fn an_empty_base_checks_every_file_of_the_head() {
    let w = world();
    let head = w.stage_bundle();
    let mut req = w.req(&head);
    req.base_sha = None;
    let out = w.v.validate(&req).await.unwrap();
    assert_eq!(out.changed_files, 5);
    assert_eq!(
        rules(&out),
        vec![
            (Rule::WorkflowFile, Some(".github/workflows/ci.yml".into())),
            (Rule::PathGlob, Some("README.md".into())),
            (Rule::Buildspec, Some("buildspec.yml".into())),
        ]
    );
}

#[tokio::test]
async fn a_broken_scanner_is_an_error_and_never_a_pass() {
    let mut w = world();
    let head = w.turn(&[("src/calc.py", Some("x = 3\n"))]);
    w.v.scanner.bin = w.root.join("no-such-gitleaks");
    assert!(matches!(
        w.v.validate(&w.req(&head)).await,
        Err(DeliveryError::Unavailable(_))
    ));
}

#[tokio::test]
async fn malformed_requests_are_invalid() {
    let w = world();
    let mut bad_run = w.req(&"a".repeat(40));
    bad_run.run_id = "run-1".into();
    assert!(matches!(
        w.v.validate(&bad_run).await,
        Err(DeliveryError::Invalid(_))
    ));
    let parsed: ValidateRequest = serde_json::from_value(json!({
        "runId": RUN, "baseSha": null, "headSha": "a".repeat(40), "allowedGlobs": ["src/**"]
    }))
    .unwrap();
    assert_eq!(parsed.limits, Limits::default());
}

// ---- building the verified bundle from a worker turn bundle ---------------------------------

/// A worker turn bundle: full history on `harness/<RUN>/<agent>` at the work repo's HEAD.
fn turn_bundle(w: &World, agent: &str) -> (String, String) {
    let branch = format!("harness/{RUN}/{agent}");
    git(&w.work, &["branch", "-f", &branch, "HEAD"]);
    let f = w.root.join(format!("{agent}.bundle"));
    git(
        &w.work,
        &["bundle", "create", "-q", f.to_str().unwrap(), &branch],
    );
    let key = format!("runs/{RUN}/agents/{agent}/turns/3/bundle");
    w.v.blobs.insert(&key, std::fs::read(&f).unwrap());
    (
        format!("s3://{BUCKET}/{key}"),
        git(&w.work, &["rev-parse", "HEAD"]),
    )
}

#[tokio::test]
async fn a_source_bundle_becomes_the_verified_bundle_and_is_validated() {
    let w = world();
    commit(&w.work, &[("src/calc.py", Some("x = 4\n"))], "turn");
    let (uri, head) = turn_bundle(&w, "worker-1");
    let mut req = w.req(&head);
    req.source_bundle_uri = Some(uri);
    let out = w.v.validate(&req).await.unwrap();
    assert!(out.pass, "{:?}", out.violations);
    let key = verified_bundle_key(RUN);
    assert_eq!(
        out.verified_bundle_uri,
        Some(format!("s3://{BUCKET}/{key}"))
    );
    let body = w.v.blobs.get(&key).unwrap();
    let meta = w.v.blobs.meta(&key).unwrap();
    assert_eq!(meta.content_type, "application/octet-stream");
    assert_eq!(meta.sha256_hex, delivery::files::sha256_hex(&body));
    let f = w.root.join("check.bundle");
    std::fs::write(&f, &body).unwrap();
    let empty = w.root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    git(&empty, &["init", "-q"]);
    let verify = std::process::Command::new("git")
        .current_dir(&empty)
        .args(["bundle", "verify", f.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(verify.status.success());
    assert!(
        String::from_utf8_lossy(&[verify.stdout, verify.stderr].concat())
            .contains("complete history")
    );
    assert_eq!(
        git(&empty, &["bundle", "list-heads", f.to_str().unwrap()]),
        format!("{head} refs/heads/harness/{RUN}")
    );
}

#[tokio::test]
async fn a_source_bundle_change_outside_the_globs_still_fails_validation() {
    let w = world();
    commit(&w.work, &[("setup.py", Some("x\n"))], "turn");
    let (uri, head) = turn_bundle(&w, "worker-1");
    let mut req = w.req(&head);
    req.source_bundle_uri = Some(uri);
    let out = w.v.validate(&req).await.unwrap();
    assert_eq!(rules(&out), vec![(Rule::PathGlob, Some("setup.py".into()))]);
    assert!(out.verified_bundle_uri.is_some());
}

#[tokio::test]
async fn a_head_absent_from_the_source_bundle_writes_no_verified_bundle() {
    let w = world();
    let (uri, _) = turn_bundle(&w, "worker-1");
    let mut req = w.req(&"d".repeat(40));
    req.source_bundle_uri = Some(uri.clone());
    let out = w.v.validate(&req).await.unwrap();
    assert!(!out.pass);
    assert_eq!(rules(&out), vec![(Rule::Bundle, None)]);
    assert!(
        out.violations[0].detail.contains(&"d".repeat(40))
            && out.violations[0].detail.contains(&uri),
        "{:?}",
        out.violations
    );
    assert!(w.v.blobs.get(&verified_bundle_key(RUN)).is_none());
    assert_eq!(out.verified_bundle_uri, None);
    // A missing or corrupt source bundle fails the same way.
    let mut missing = w.req(&"d".repeat(40));
    missing.source_bundle_uri = Some(format!(
        "s3://{BUCKET}/runs/{RUN}/agents/nobody/turns/1/bundle"
    ));
    assert_eq!(
        rules(&w.v.validate(&missing).await.unwrap()),
        vec![(Rule::Bundle, None)]
    );
    w.v.blobs.insert(
        &format!("runs/{RUN}/agents/junk/turns/1/bundle"),
        b"junk".to_vec(),
    );
    let mut junk = w.req(&"d".repeat(40));
    junk.source_bundle_uri = Some(format!(
        "s3://{BUCKET}/runs/{RUN}/agents/junk/turns/1/bundle"
    ));
    assert_eq!(
        rules(&w.v.validate(&junk).await.unwrap()),
        vec![(Rule::Bundle, None)]
    );
    assert!(w.v.blobs.get(&verified_bundle_key(RUN)).is_none());
}

#[tokio::test]
async fn a_source_bundle_outside_the_harness_bucket_or_the_run_is_invalid() {
    let w = world();
    for uri in [
        format!("s3://other-bucket/runs/{RUN}/agents/a/turns/1/bundle"),
        format!("s3://{BUCKET}/runs/01K5ZQ4T8M3N6P7R9S0V1W2X00/agents/a/turns/1/bundle"),
        format!("s3://{BUCKET}/runs/{RUN}/../x/bundle"),
        "file:///work/turns/2/bundle".to_string(),
    ] {
        let mut req = w.req(&"d".repeat(40));
        req.source_bundle_uri = Some(uri.clone());
        assert!(
            matches!(w.v.validate(&req).await, Err(DeliveryError::Invalid(_))),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn without_a_source_bundle_the_staged_verified_bundle_is_validated_and_named() {
    let w = world();
    let head = w.turn(&[("src/calc.py", Some("x = 5\n"))]);
    let out = w.v.validate(&w.req(&head)).await.unwrap();
    assert!(out.pass);
    assert_eq!(
        out.verified_bundle_uri,
        Some(format!("s3://{BUCKET}/{}", verified_bundle_key(RUN)))
    );
    let parsed: ValidateRequest = serde_json::from_value(json!({
        "runId": RUN, "headSha": head, "allowedGlobs": ["src/**"],
        "sourceBundleUri": format!("s3://{BUCKET}/runs/{RUN}/agents/a/turns/1/bundle")
    }))
    .unwrap();
    assert!(parsed.source_bundle_uri.is_some());
}
