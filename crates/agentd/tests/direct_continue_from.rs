//! The agentd side of `continue_from`: the repo-gateway stages the source run's
//! verified bundle as the new run's `input/repo.bundle`, so the repository the run restores IS the seed. agentd adds one tripwire before the
//! engine starts: the restored HEAD equals the `headSha` of the stored request, and the history is whole. The prompt Cline receives is the
//! user's text as typed (invariant 1); nothing about the earlier run is put into it.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::continue_from::{ContinueFromHook, seed_check};
use agentd::direct::hooks::{DirectHook, Hooks};
use direct_support::{Direct, Opts, PROMPT, RUN, sha};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::{Value, json};
use std::sync::Arc;

const SOURCE_RUN: &str = "01M4XSOURCERUN000000000001";

fn with_members(d: &mut Direct, members: Value) {
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    for (k, val) in members.as_object().unwrap() {
        v[k] = val.clone();
    }
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    d.req.request.sha256 = sha(&bytes);
    d.request_json = bytes;
}

async fn run(d: &Direct) -> InvocationResponse {
    let module: Arc<dyn DirectHook> = Arc::new(ContinueFromHook::new());
    let deps = Deps {
        hooks: Some(Hooks::new(vec![module])),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

/// The head of the repository the test put in the bucket: the seed a gateway would stage.
fn seed(d: &Direct) -> String {
    common::git(&d.dir.join("python-calc"), &["rev-parse", "HEAD"])
}

fn continue_events(d: &Direct) -> Vec<Value> {
    d.events_of(EventType::TurnProgress)
        .into_iter()
        .filter(|e| e["kind"] == "continue-from")
        .collect()
}

#[test]
fn the_source_run_id_of_these_tests_is_a_run_id() {
    assert_eq!(SOURCE_RUN.len(), 26);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_continued_run_starts_at_the_seed_head_and_the_prompt_is_the_users_own() {
    let mut d = Direct::start(Opts::default()).await;
    let head = seed(&d);
    with_members(
        &mut d,
        json!({ "continueFrom": { "runId": SOURCE_RUN, "headSha": head } }),
    );
    let resp = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");

    // The new run's base is the seed: the review of this run measures from the old head.
    let s = d.summary();
    assert_eq!(s.git.base_sha.as_deref(), Some(head.as_str()));
    // The prompt reached the engine byte for byte.
    assert_eq!(d.prompts_received(), [PROMPT]);

    let evs = continue_events(&d);
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0]["fromRun"], SOURCE_RUN);
    assert_eq!(evs[0]["seed"], head.as_str());

    // TRIGGER PRESENT: the seed is a commit with history behind it, not the empty root commit a plain s3-only run starts from.
    let commits: u32 = common::git(&d.dir.join("python-calc"), &["rev-list", "--count", &head])
        .parse()
        .unwrap();
    assert!(
        commits >= 2,
        "the seed must have a history: {commits} commits"
    );
    println!(
        "CONTINUE-FROM TRIGGER PRESENT seed={} commits-behind-seed={commits} run-base=seed",
        &head[..12]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_run_starts_from_the_head_an_earlier_run_delivered() {
    // Run 1 does work (the fake engine edits two files) and delivers a bundle: its head is a commit the base repository never had.
    let d1 = Direct::start(Opts::default()).await;
    let resp1 = agentd::direct::run_with(&d1.app, d1.req.clone(), &Deps::default()).await;
    assert_eq!(resp1.outcome, TurnOutcome::Completed, "{resp1:?}");
    let s1 = d1.summary();
    let head1 = s1.git.head_sha.clone().expect("run 1 delivered a head");
    let base1 = s1.git.base_sha.clone().expect("run 1 has a base");
    assert_ne!(head1, base1, "the trigger: run 1 changed the repository");
    let delivered = std::fs::read(d1.bundle_file().expect("run 1 uploaded its bundle")).unwrap();

    // The repo-gateway stages that bundle as the NEW run's repo.bundle (the run's credentials read only its own prefix).
    let mut d2 = Direct::start(Opts::default()).await;
    d2.aws.put(&keys::repo_bundle_key(RUN), delivered);
    with_members(
        &mut d2,
        json!({ "continueFrom": { "runId": SOURCE_RUN, "headSha": head1 } }),
    );
    let resp2 = agentd::direct::run_with(&d2.app, d2.req.clone(), &Deps::default()).await;
    assert_eq!(resp2.outcome, TurnOutcome::Completed, "{resp2:?}");
    let s2 = d2.summary();
    // The new run's base is the old run's delivered head, not the base repository the old run started from.
    assert_eq!(s2.git.base_sha.as_deref(), Some(head1.as_str()));
    assert_ne!(s2.git.base_sha, s1.git.base_sha);
    assert_eq!(d2.prompts_received(), [PROMPT]);
    let evs = continue_events(&d2);
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0]["seed"], head1.as_str());
    println!(
        "CONTINUE-FROM TRIGGER PRESENT old-run-head={} differs-from-old-run-base={} new-run-base=old-run-head",
        &head1[..12],
        &base1[..12]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_head_that_is_not_the_restored_one_stops_the_run_before_any_model_call() {
    let mut d = Direct::start(Opts::default()).await;
    let head = seed(&d);
    let other = format!("{}1", "0".repeat(39));
    assert_ne!(head, other);
    with_members(
        &mut d,
        json!({ "continueFrom": { "runId": SOURCE_RUN, "headSha": other } }),
    );
    let resp = run(&d).await;
    assert_ne!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    let detail = s.detail.unwrap();
    assert!(
        detail.contains("continue_from")
            && detail.contains(&head[..12])
            && detail.contains(&other[..12]),
        "{detail}"
    );
    assert!(
        d.prompts_received().is_empty(),
        "the engine never got a prompt"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_request_without_a_head_stops_the_run() {
    let mut d = Direct::start(Opts::default()).await;
    with_members(&mut d, json!({ "continueFrom": { "runId": SOURCE_RUN } }));
    let resp = run(&d).await;
    assert_ne!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let detail = d.summary().detail.unwrap();
    assert!(detail.contains("headSha"), "{detail}");
    assert!(d.prompts_received().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_without_continue_from_is_untouched() {
    let d = Direct::start(Opts::default()).await;
    let resp = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(continue_events(&d).is_empty());
}

// ---- the check itself ---------------------------------------------------------------------------------------------------------------

fn two_commit_repo(label: &str) -> (std::path::PathBuf, String) {
    let dir = common::scratch(label);
    let repo = common::python_calc(&dir);
    std::fs::write(repo.join("NOTES.md"), "second\n").unwrap();
    common::git(&repo, &["add", "NOTES.md"]);
    common::git(&repo, &["commit", "-q", "-m", "second"]);
    let head = common::git(&repo, &["rev-parse", "HEAD"]);
    (repo, head)
}

#[tokio::test]
async fn the_check_passes_when_head_is_the_seed_and_the_history_is_whole() {
    let (repo, head) = two_commit_repo("seed-ok");
    seed_check(&repo, &head).await.unwrap();
}

#[tokio::test]
async fn the_check_names_both_commits_when_head_is_another() {
    let (repo, head) = two_commit_repo("seed-other");
    let other = common::git(&repo, &["rev-parse", "HEAD~1"]);
    let e = seed_check(&repo, &other).await.unwrap_err();
    assert!(e.contains(&head[..12]) && e.contains(&other[..12]), "{e}");
}

#[tokio::test]
async fn an_abbreviated_or_upper_case_head_is_not_the_seed() {
    let (repo, head) = two_commit_repo("seed-short");
    assert!(seed_check(&repo, &head[..12]).await.is_err());
    assert!(seed_check(&repo, &head.to_uppercase()).await.is_err());
}

#[tokio::test]
async fn a_shallow_history_is_refused_because_a_follow_up_needs_the_whole_one() {
    let (repo, head) = two_commit_repo("seed-shallow");
    let dst = repo.parent().unwrap().join("shallow");
    common::git(
        repo.parent().unwrap(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", repo.display()),
            dst.to_str().unwrap(),
        ],
    );
    assert_eq!(common::git(&dst, &["rev-parse", "HEAD"]), head);
    let e = seed_check(&dst, &head).await.unwrap_err();
    assert!(e.contains("shallow"), "{e}");
}

// ---- a second invocation of the same run on the same VM ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_second_invocation_finds_the_work_of_the_first_and_starts_again() {
    let mut d = Direct::start(Opts::default()).await;
    let head = seed(&d);
    with_members(
        &mut d,
        json!({ "continueFrom": { "runId": SOURCE_RUN, "headSha": head } }),
    );
    let first = run(&d).await;
    assert_eq!(first.outcome, TurnOutcome::Completed, "{first:?}");
    // TRIGGER PRESENT: the first attempt moved the work tree beyond the seed (the fake engine's edits are committed), so a second
    // invocation of the run finds HEAD at its own work, which has the seed in its history.
    let now = common::git(&d.worktree(), &["rev-parse", "HEAD"]);
    assert_ne!(now, head, "the work tree moved on from the seed");
    common::git(&d.worktree(), &["merge-base", "--is-ancestor", &head, &now]);

    let second = run(&d).await;
    // The second invocation started and ran the engine: a refusal by the module ends the run before any prompt, with no stop reason.
    // Its end is the end sequence's conflict, not the module's: the first attempt's bundle and summary.json are already in the bucket
    // (written create-only), so agentd records the conflict, writes the second attempt's own objects over them and the second turn says so.
    assert_eq!(
        second.stop_reason.as_deref(),
        Some("end_turn"),
        "{second:?}"
    );
    assert_eq!(second.outcome, TurnOutcome::Failed, "{second:?}");
    assert!(
        second
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("already existed"),
        "{second:?}"
    );
    assert_eq!(continue_events(&d).len(), 2, "{:?}", continue_events(&d));
    println!(
        "CONTINUE-FROM RETRY TRIGGER PRESENT seed={} work-tree-head={} second-invocation=started",
        &head[..12],
        &now[..12]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_restore_that_is_ahead_of_the_seed_is_still_refused() {
    // No first attempt has run here, so the work tree is a fresh restore: it must BE the commit the user was shown, not a descendant of it.
    let mut d = Direct::start(Opts::default()).await;
    let head = seed(&d);
    let parent = common::git(&d.dir.join("python-calc"), &["rev-parse", "HEAD~1"]);
    with_members(
        &mut d,
        json!({ "continueFrom": { "runId": SOURCE_RUN, "headSha": parent } }),
    );
    let resp = run(&d).await;
    assert_ne!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let detail = d.summary().detail.unwrap();
    assert!(
        detail.contains(&head[..12]) && detail.contains(&parent[..12]),
        "{detail}"
    );
    assert!(d.prompts_received().is_empty());
}
