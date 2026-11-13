//! A run whose target is a zip delivers its zip.
//!
//! A run with the zip target ended blocked with no zip because the engine added `.github/workflows/ci.yml`: the validator refused every
//! change under `.github/workflows/` and to a root `buildspec.yml` for every target. Those two rules protect a repository (a git host
//! runs a workflow file; the pull request check runs the build specification). Nothing runs a zip, so for `"target": "zip"` such a file
//! is delivered and reported under `findings`, the list that carries what the validator found and let through. A repository delivery
//! (`"target": "pull-request"`, and a request without the member, which is what an older caller sends) keeps the refusal. The engine's
//! executable configuration stays refused for both.
//!
//! The requests are built as JSON, the way run-session sends them.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::changes::changes_key;
use delivery::git::Git;
use delivery::validator::{Gitleaks, Rule, ValidateOutput, ValidateRequest, Validator};
use harness_protocol::store::s3::verified_bundle_key;
use serde_json::{Value, json};
use std::path::PathBuf;

const WORKFLOW: &str = ".github/workflows/ci.yml";
const WORKFLOW_TEXT: &str = "name: ci\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make test\n";
const BUILDSPEC: &str = "buildspec.yml";
const BUILDSPEC_TEXT: &str = "version: 0.2\nphases:\n  build:\n    commands:\n      - make test\n";

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    base: String,
    v: Validator<MemBlobs, Gitleaks>,
}

/// A repository whose base commit holds `files`, and a validator over it.
fn world_with(files: &[(&str, &str)]) -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (work, shas) = work_repo(&root, "work", &[files]);
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

fn world() -> World {
    world_with(&[("src/calc.py", "def add(a, b):\n    return a + b\n")])
}

impl World {
    /// One more commit, and the verified bundle of the head in the bucket.
    fn turn(&self, files: &[(&str, Option<&str>)]) -> String {
        commit(&self.work, files, "agent turn");
        let f = self.root.join("verified.bundle");
        let head = verified_bundle(&self.work, RUN, &f);
        self.v
            .blobs
            .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
        head
    }

    /// The request run-session sends for a direct run, with `target` when one is given.
    fn request(&self, head: &str, base: &str, target: Option<&str>) -> ValidateRequest {
        let mut v = json!({
            "runId": RUN, "headSha": head, "baseSha": base,
            "allowedGlobs": ["**", "!.git/**"], "engineConfig": "executable-only",
        });
        if let Some(t) = target {
            v["target"] = json!(t);
        }
        serde_json::from_value(v).unwrap()
    }

    async fn validate(&self, head: &str, target: Option<&str>) -> ValidateOutput {
        self.v
            .validate(&self.request(head, &self.base, target))
            .await
            .unwrap()
    }
}

fn rules(list: &[delivery::validator::Violation]) -> Vec<(Rule, Option<String>)> {
    list.iter().map(|v| (v.rule, v.path.clone())).collect()
}

#[tokio::test]
async fn b821_a_zip_target_delivers_a_workflow_file_and_reports_it() {
    let w = world();
    let head = w.turn(&[
        (WORKFLOW, Some(WORKFLOW_TEXT)),
        (
            "src/calc.py",
            Some("def add(a, b):\n    return a + b + 0\n"),
        ),
    ]);
    let out = w.validate(&head, Some("zip")).await;
    assert!(out.pass, "a zip run delivers its zip: {:?}", out.violations);
    assert!(out.violations.is_empty(), "{:?}", out.violations);
    assert_eq!(
        rules(&out.findings),
        vec![(Rule::WorkflowFile, Some(WORKFLOW.to_string()))],
        "the file is reported, not hidden"
    );
    let detail = &out.findings[0].detail;
    assert!(
        detail.contains("the zip holds it") && detail.contains("git host"),
        "the finding says where the file is and when it would run: {detail}"
    );
    // A delivery that passed has its change record and its verified bundle, like any other.
    assert!(w.v.blobs.get(&changes_key(RUN)).is_some());
    assert!(out.verified_bundle_uri.is_some());
    // The answer on the wire carries the finding where finalize reads it.
    let wire: Value = serde_json::to_value(&out).unwrap();
    assert_eq!(wire["findings"][0]["rule"], "workflow-file");
    assert_eq!(wire["findings"][0]["path"], WORKFLOW);
}

#[tokio::test]
async fn b821_a_zip_target_delivers_a_root_build_specification_and_reports_it() {
    let w = world();
    let head = w.turn(&[(BUILDSPEC, Some(BUILDSPEC_TEXT))]);
    let out = w.validate(&head, Some("zip")).await;
    assert!(out.pass, "{:?}", out.violations);
    assert_eq!(
        rules(&out.findings),
        vec![(Rule::Buildspec, Some(BUILDSPEC.to_string()))]
    );
    assert!(
        out.findings[0].detail.contains("the zip holds it"),
        "{}",
        out.findings[0].detail
    );
}

#[tokio::test]
async fn b821_the_same_bundle_is_refused_for_a_pull_request_target_and_for_a_request_without_the_member()
 {
    let w = world();
    let head = w.turn(&[
        (WORKFLOW, Some(WORKFLOW_TEXT)),
        (BUILDSPEC, Some(BUILDSPEC_TEXT)),
    ]);
    // No member is what a caller of an earlier release sends: it keeps the refusal.
    for target in [Some("pull-request"), None] {
        let out = w.validate(&head, target).await;
        assert!(!out.pass, "{target:?}");
        assert_eq!(
            rules(&out.violations),
            vec![
                (Rule::WorkflowFile, Some(WORKFLOW.to_string())),
                (Rule::Buildspec, Some(BUILDSPEC.to_string())),
            ],
            "{target:?}"
        );
        assert!(out.findings.is_empty(), "{target:?}: {:?}", out.findings);
        // The refusal says what the file is and why a repository does not take it.
        let workflow = &out.violations[0].detail;
        assert!(
            workflow.contains("is a workflow file, which a git host runs")
                && workflow.contains("a delivery to a repository refuses the change (A)"),
            "{workflow}"
        );
        let buildspec = &out.violations[1].detail;
        assert!(
            buildspec.contains("is the build specification that the pull request check runs")
                && buildspec.contains("a delivery to a repository refuses the change (A)"),
            "{buildspec}"
        );
        assert!(
            w.v.blobs.get(&changes_key(RUN)).is_none(),
            "a refused delivery writes no change record"
        );
    }
}

#[tokio::test]
async fn b821_the_engines_executable_configuration_is_refused_for_both_targets() {
    for path in [
        ".cline/hooks/PreToolUse",
        ".cline/plugins/p.js",
        ".cline/agents/a.md",
        ".clinerules/hooks/x",
        "opencode.json",
        ".opencode/agent/x.md",
    ] {
        let w = world();
        let head = w.turn(&[(path, Some("#!/bin/sh\ntrue\n"))]);
        for target in [Some("zip"), Some("pull-request"), None] {
            let out = w.validate(&head, target).await;
            assert!(!out.pass, "{path} {target:?}");
            assert_eq!(
                rules(&out.violations),
                vec![(Rule::EngineConfig, Some(path.to_string()))],
                "{path} {target:?}"
            );
            assert!(out.findings.is_empty(), "{path} {target:?}");
        }
    }
}

#[tokio::test]
async fn b821_a_zip_target_reports_a_changed_workflow_file_and_not_a_deleted_one() {
    let w = world_with(&[
        ("src/calc.py", "def add(a, b):\n    return a + b\n"),
        (WORKFLOW, WORKFLOW_TEXT),
        (".github/workflows/old.yml", "name: old\non: [push]\n"),
    ]);
    let head = w.turn(&[
        (WORKFLOW, Some("name: ci\non: [pull_request]\n")),
        (".github/workflows/old.yml", None),
    ]);
    let out = w.validate(&head, Some("zip")).await;
    assert!(out.pass, "{:?}", out.violations);
    assert_eq!(
        rules(&out.findings),
        vec![(Rule::WorkflowFile, Some(WORKFLOW.to_string()))],
        "the zip holds the changed file; the deleted one is not in it"
    );
    // The same two changes on a repository: both refused, the deletion too.
    let repo = w.validate(&head, None).await;
    assert_eq!(
        rules(&repo.violations),
        vec![
            (Rule::WorkflowFile, Some(WORKFLOW.to_string())),
            (
                Rule::WorkflowFile,
                Some(".github/workflows/old.yml".to_string())
            ),
        ]
    );
}

#[tokio::test]
async fn b821_a_zip_target_still_holds_a_workflow_file_to_the_allowed_globs_and_the_other_rules() {
    let w = world();
    let head = w.turn(&[(WORKFLOW, Some(WORKFLOW_TEXT))]);
    let mut r = w.request(&head, &w.base, Some("zip"));
    r.allowed_globs = vec!["src/**".into()];
    let out = w.v.validate(&r).await.unwrap();
    assert!(!out.pass);
    assert_eq!(
        rules(&out.violations),
        vec![(Rule::PathGlob, Some(WORKFLOW.to_string()))],
        "the zip target lifts two path rules and nothing else"
    );
}

/// Trace 3 of the item: a repository run whose base already holds a workflow file and a build specification, and that touches neither,
/// passes as it always did. Passes before the change.
#[tokio::test]
async fn b821_a_repository_run_whose_base_holds_a_workflow_file_passes_when_it_does_not_touch_it() {
    let w = world_with(&[
        ("src/calc.py", "def add(a, b):\n    return a + b\n"),
        (WORKFLOW, WORKFLOW_TEXT),
        (BUILDSPEC, BUILDSPEC_TEXT),
    ]);
    let head = w.turn(&[(
        "src/calc.py",
        Some("def add(a, b):\n    return a + b + 0\n"),
    )]);
    for target in [None, Some("pull-request")] {
        let out = w.validate(&head, target).await;
        assert!(out.pass, "{target:?}: {:?}", out.violations);
        assert!(out.findings.is_empty(), "{target:?}");
    }
}

/// The follow-up traces of the item rest on this: the validator judges every commit from the base it is given. A follow-up of a run that
/// was refused for a workflow file is given that run's base (run-session, `b821_zip_delivery.rs`), so the file is in the range again: a
/// repository delivery refuses it, a zip delivery reports it. Judged from the refused head, as before, the file is not seen at all.
#[tokio::test]
async fn b821_a_follow_up_judged_from_the_base_of_the_refused_run_sees_its_workflow_file() {
    let w = world();
    let refused_head = w.turn(&[(WORKFLOW, Some(WORKFLOW_TEXT))]);
    let head = w.turn(&[("README.md", Some("# calc\n"))]);
    // From the refused head: nothing to see (what made the route around the rule).
    let from_refused_head =
        w.v.validate(&w.request(&head, &refused_head, None))
            .await
            .unwrap();
    assert!(from_refused_head.pass, "{:?}", from_refused_head.violations);
    // From the refused run's base: the repository rule holds, and the zip target reports.
    let repo = w.validate(&head, None).await;
    assert_eq!(
        rules(&repo.violations),
        vec![(Rule::WorkflowFile, Some(WORKFLOW.to_string()))]
    );
    let zip = w.validate(&head, Some("zip")).await;
    assert!(zip.pass, "{:?}", zip.violations);
    assert_eq!(
        rules(&zip.findings),
        vec![(Rule::WorkflowFile, Some(WORKFLOW.to_string()))]
    );
    // A follow-up that removed the file again holds no change to it from that base, for either target.
    let cleaned = w.turn(&[(WORKFLOW, None)]);
    let repo = w.validate(&cleaned, None).await;
    assert!(repo.pass, "{:?}", repo.violations);
}

#[test]
fn b821_the_request_reads_the_target_and_leaves_the_default_off_the_wire() {
    let base = json!({"runId": RUN, "headSha": "a".repeat(40), "allowedGlobs": ["**"]});
    let absent: ValidateRequest = serde_json::from_value(base.clone()).unwrap();
    let absent_wire = serde_json::to_value(&absent).unwrap();
    assert!(
        absent_wire.get("target").is_none(),
        "the default is not written, so an earlier caller's request keeps its bytes: {absent_wire}"
    );
    let mut named = base.clone();
    named["target"] = json!("pull-request");
    let named: ValidateRequest = serde_json::from_value(named).unwrap();
    assert_eq!(named, absent, "no member is a pull request target");
    let mut zip = base.clone();
    zip["target"] = json!("zip");
    let zip: ValidateRequest = serde_json::from_value(zip).unwrap();
    assert_ne!(zip, absent);
    assert_eq!(serde_json::to_value(&zip).unwrap()["target"], "zip");
    // Any other word is not a zip: the request is refused, and the machine ends the run blocked.
    for word in ["s3-only", "ZIP", "repo", ""] {
        let mut bad = base.clone();
        bad["target"] = json!(word);
        assert!(
            serde_json::from_value::<ValidateRequest>(bad).is_err(),
            "{word:?}"
        );
    }
}

/// What makes the deploy order safe on the other side: a validator of an earlier release reads the request of a new caller, because the
/// request type takes a member it does not know. Pinned here so the type is never made strict without that order being thought about.
#[test]
fn b821_the_request_takes_a_member_it_does_not_know() {
    let v = json!({"runId": RUN, "headSha": "a".repeat(40), "allowedGlobs": ["**"], "aLaterMember": "x"});
    assert!(serde_json::from_value::<ValidateRequest>(v).is_ok());
}
