//! `repo-gateway` under a configured repository-name rule (`Gateway::repo_names`, from `HAMH_REPO_NAMES`): a run reaches a repository only when the allowlist has it
//! and the rule covers its name, and the check comes before any repository read. Repositories the harness creates itself (`--new-repo`) are covered whatever the rule
//! says, so a stage that lists only its own repositories can still use `--new-repo`.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::error::DeliveryError;
use delivery::gateway::{Gateway, NewRepoRequest, StageRequest, StageTarget};
use delivery::git::Git;
use delivery::repo_names::{EARLIER_DEFAULT_PREFIX, RepoNames};
use std::path::PathBuf;

const AT: u64 = 1_790_337_600; // 2026-09-25T12:00:00Z
const OWN: &str = "my-app";
const NEW: &str = "hamh-e2e-new-20260925";

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    remotes_dir: PathBuf,
    gw: Gateway<FakeCodeCommit, FakeAllowlist, MemBlobs, LocalRemotes>,
}

fn world(allowed: &[&str], rule: &str) -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let remotes_dir = root.join("codecommit");
    let work = root.join("work");
    let home = root.join("home");
    for d in [&remotes_dir, &work, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    let gw = Gateway {
        codecommit: FakeCodeCommit::new(&remotes_dir),
        allowlist: FakeAllowlist::with(allowed),
        blobs: MemBlobs::default(),
        remotes: LocalRemotes::new(&remotes_dir),
        git: Git::new("git", &home),
        stage: "live".into(),
        owner: "team-a".into(),
        bucket: BUCKET.into(),
        work_root: work,
        clock: || AT,
        repo_names: RepoNames::parse(rule).unwrap(),
    };
    World {
        _tmp: tmp,
        root,
        remotes_dir,
        gw,
    }
}

fn publish_own(w: &World, name: &str) -> String {
    let (work, shas) = work_repo(
        &w.root,
        "own-work",
        &[&[
            ("calc.py", "def add(a, b):\n    return a + b\n"),
            ("buildspec.yml", "version: 0.2\n"),
        ]],
    );
    publish(&work, &w.remotes_dir, name);
    shas[0].clone()
}

fn stage_req(target: &str) -> StageRequest {
    StageRequest {
        run_id: RUN.into(),
        target: StageTarget::Repo(target.parse().unwrap()),
        base_ref: None,
        from_run: None,
        container_signals: None,
    }
}

fn new_req(target: &str) -> NewRepoRequest {
    NewRepoRequest {
        run_id: RUN.into(),
        target: target.parse().unwrap(),
    }
}

fn refused<T: std::fmt::Debug>(r: Result<T, DeliveryError>) -> String {
    match r {
        Err(DeliveryError::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_repository_the_rule_covers_and_the_allowlist_has_is_staged() {
    let w = world(&[&format!("cc:{OWN}")], "my-app,team-*");
    let sha = publish_own(&w, OWN);
    let out = w.gw.stage(&stage_req(&format!("cc:{OWN}"))).await.unwrap();
    assert_eq!(out.base_sha, sha);
}

#[tokio::test]
async fn a_repository_the_rule_covers_but_the_allowlist_lacks_is_refused() {
    let w = world(&[], "my-app");
    publish_own(&w, OWN);
    let reason = refused(w.gw.stage(&stage_req(&format!("cc:{OWN}"))).await);
    assert!(reason.contains("is not on the allowlist"), "{reason}");
}

#[tokio::test]
async fn an_allowlisted_repository_outside_the_rule_is_refused_before_any_lookup() {
    // An entry that was allowed under an earlier rule stays on the allowlist; the rule decides at delivery time as well.
    let w = world(&["cc:hamh-e2e-python-calc"], "my-app");
    let reason = refused(w.gw.stage(&stage_req("cc:hamh-e2e-python-calc")).await);
    assert_eq!(
        reason,
        "refused: cc:hamh-e2e-python-calc is outside the repository names this stage allows (my-app)"
    );
    assert_eq!(*w.gw.allowlist.reads.lock().unwrap(), 0);
    assert!(w.gw.codecommit.calls().is_empty());
}

#[tokio::test]
async fn the_default_rule_keeps_the_text_of_the_fixed_prefix() {
    let w = world(&["cc:python-calc"], "hamh-e2e-*");
    let reason = refused(w.gw.stage(&stage_req("cc:python-calc")).await);
    assert_eq!(
        reason,
        "refused: cc:python-calc is outside the hamh-e2e- prefix"
    );
}

#[tokio::test]
async fn new_repo_still_creates_its_own_name_when_the_rule_lists_other_names() {
    let w = world(&[], "my-app");
    let out = w.gw.new_repo(&new_req(&format!("cc:{NEW}"))).await.unwrap();
    assert!(out.created);
    let created = w.gw.codecommit.created.lock().unwrap().clone();
    assert_eq!(
        created.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        [NEW]
    );
    // The harness allowlisted what it made, and the repository is usable although the rule does not list its name.
    let staged = w.gw.stage(&stage_req(&format!("cc:{NEW}"))).await.unwrap();
    assert_eq!(staged.base_sha, out.base_sha);
}

#[tokio::test]
async fn new_repo_does_not_make_a_name_the_rule_lists_but_the_harness_does_not_use() {
    let w = world(&[], "my-app");
    let reason = refused(w.gw.new_repo(&new_req(&format!("cc:{OWN}"))).await);
    assert!(reason.contains("is not a new-repo name"), "{reason}");
    assert!(w.gw.codecommit.calls().is_empty());
}

#[tokio::test]
async fn new_repo_outside_the_rule_and_outside_the_harness_names_is_refused_with_the_rule() {
    let w = world(&[], "my-app");
    let reason = refused(w.gw.new_repo(&new_req("cc:new-20260925")).await);
    assert_eq!(
        reason,
        "refused: cc:new-20260925 is outside the repository names this stage allows (my-app)"
    );
    assert!(w.gw.codecommit.calls().is_empty());
}

#[tokio::test]
async fn an_earlier_repository_is_staged_under_the_default_rule_when_the_allowlist_has_it() {
    // The default rule covers the name; the allowlist and the repository's ARN check decide the rest, as for any repository.
    let name = format!("{EARLIER_DEFAULT_PREFIX}python-calc-20260925");
    let w = world(&[&format!("cc:{name}")], "hamh-e2e-*");
    let sha = publish_own(&w, &name);
    let out = w.gw.stage(&stage_req(&format!("cc:{name}"))).await.unwrap();
    assert_eq!(out.base_sha, sha);
}

#[tokio::test]
async fn an_earlier_repository_is_refused_when_the_allowlist_lacks_it() {
    // The operator re-allows it on the new stage's table (`ham-admin repos allow`); the name alone never reaches a repository.
    let name = format!("{EARLIER_DEFAULT_PREFIX}python-calc-20260925");
    let w = world(&[], "hamh-e2e-*");
    publish_own(&w, &name);
    let reason = refused(w.gw.stage(&stage_req(&format!("cc:{name}"))).await);
    assert!(reason.contains("is not on the allowlist"), "{reason}");
}

#[tokio::test]
async fn a_configured_rule_refuses_an_earlier_repository_before_any_lookup() {
    let name = format!("{EARLIER_DEFAULT_PREFIX}python-calc-20260925");
    let w = world(&[&format!("cc:{name}")], "my-app");
    publish_own(&w, &name);
    let reason = refused(w.gw.stage(&stage_req(&format!("cc:{name}"))).await);
    assert_eq!(
        reason,
        format!("refused: cc:{name} is outside the repository names this stage allows (my-app)")
    );
    assert_eq!(*w.gw.allowlist.reads.lock().unwrap(), 0);
    assert!(w.gw.codecommit.calls().is_empty());
}

#[tokio::test]
async fn new_repo_makes_no_repository_under_the_earlier_prefix() {
    // New repositories get the new prefix only, whatever the rule covers.
    let name = format!("{EARLIER_DEFAULT_PREFIX}new-20260925");
    let w = world(&[], "hamh-e2e-*");
    let reason = refused(w.gw.new_repo(&new_req(&format!("cc:{name}"))).await);
    assert!(reason.contains("is not a new-repo name"), "{reason}");
    assert!(w.gw.codecommit.calls().is_empty());
}
