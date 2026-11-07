//! `repo-gateway push-pr` for a follow-up run: the pull request of a run that continues an earlier run's delivered commit
//! shows only the new work, points where the old run's branch says it should, and links to the old pull request (and back).
//!
//! The three cases of the maintainers' sentence, each with real git over the CodeCommit fake's bare repositories:
//! - the old branch still points at the commit that run delivered: the new pull request targets that branch;
//! - the old work was merged, or the branch was deleted: it targets the destination branch of the old run's pull request;
//! - the old branch is alive but moved: a delivery conflict, no pull request, nothing pushed.
//!
//! The fake refuses what CodeCommit refuses (the same source and destination, a changed request under one token, a comment on a pull
//! request of another repository, a commit that does not exist); the tests at the end hold the fake to the API reference.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use delivery::checks::{pr_console_url, pull_request_id_in};
use delivery::error::DeliveryError;
use delivery::follow_up::{Aim, Facts, FollowUp, FollowUpOpened, Plan, decide};
use delivery::gateway::{
    CodeCommit, Gateway, GatewayRequest, NewComment, NewPullRequest, PushPrOutput, PushPrRequest,
};
use delivery::git::Git;
use harness_protocol::store::s3::verified_bundle_key;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const AT: u64 = 1_790_337_600;
/// The earlier run; `RUN` (from `common`) is the follow-up run.
const OLD: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z";
/// A third run, for a pull request that is not the earlier run's.
const THIRD: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X5A";

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    remotes_dir: PathBuf,
    gw: Gateway<FakeCodeCommit, FakeAllowlist, MemBlobs, LocalRemotes>,
}

fn world() -> World {
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
        allowlist: FakeAllowlist::with(&[&format!("cc:{FIXTURE}")]),
        blobs: MemBlobs::default(),
        remotes: LocalRemotes::new(&remotes_dir),
        git: Git::new("git", &home),
        stage: "live".into(),
        owner: "team-a".into(),
        bucket: BUCKET.into(),
        work_root: work,
        clock: || AT,
        repo_names: Default::default(),
    };
    World {
        _tmp: tmp,
        root,
        remotes_dir,
        gw,
    }
}

fn bare(w: &World) -> PathBuf {
    w.remotes_dir.join(format!("{FIXTURE}.git"))
}

fn request(run: &str, head: &str, follow_up: Option<FollowUp>) -> PushPrRequest {
    PushPrRequest {
        run_id: run.into(),
        target: format!("cc:{FIXTURE}").parse().unwrap(),
        head_sha: head.into(),
        title: "ham: add percent_change".into(),
        description: "Run description.".into(),
        comments: vec!["| table |".into()],
        source_ref: None,
        follow_up,
    }
}

/// What the first run left in the repository and what the follow-up run delivers.
struct Delivered {
    work: PathBuf,
    /// `main` of the repository.
    base: String,
    /// The commit the old run delivered (PR 1, `harness/OLD` to `main`).
    old_head: String,
    /// The follow-up run's head: `old_head` and one more commit.
    new_head: String,
    /// The link of PR 1.
    old_url: String,
}

fn bundle_of(w: &World, work: &Path, run: &str) -> String {
    let f = w.root.join(format!("{run}.bundle"));
    let head = verified_bundle(work, run, &f);
    w.gw.blobs
        .insert(&verified_bundle_key(run), std::fs::read(&f).unwrap());
    head
}

/// The fixture repository with `main` (two commits). The old run delivered a commit through the real `push-pr` (PR 1), and the
/// follow-up run's verified bundle holds that commit and one more. The repository has the root `buildspec.yml` that the pull-request
/// check runs, so the check rule is attached to both pull requests.
async fn after_first_delivery(w: &World) -> Delivered {
    after_first_delivery_with(w, true).await
}

async fn after_first_delivery_with(w: &World, build_file: bool) -> Delivered {
    let first_files: &[(&str, &str)] = if build_file {
        &[
            ("calc.py", "def add(a, b):\n    return a + b\n"),
            ("buildspec.yml", "version: 0.2\n"),
        ]
    } else {
        &[("calc.py", "def add(a, b):\n    return a + b\n")]
    };
    let (work, shas) = work_repo(
        &w.root,
        "fixture-work",
        &[
            first_files,
            &[("tests/test_calc.py", "from calc import add\n")],
        ],
    );
    publish(&work, &w.remotes_dir, FIXTURE);
    let base = shas[1].clone();
    commit(
        &work,
        &[("notes.md", Some("- percent_change\n"))],
        "old work",
    );
    let old_head = bundle_of(w, &work, OLD);
    let first = w.gw.push_pr(&request(OLD, &old_head, None)).await.unwrap();
    assert_eq!(first.pull_request_id, "1");
    commit(&work, &[("README.md", Some("# calc\n"))], "follow-up work");
    let new_head = bundle_of(w, &work, RUN);
    Delivered {
        work,
        base,
        old_head,
        new_head,
        old_url: first.url,
    }
}

fn follow(d: &Delivered) -> FollowUp {
    FollowUp {
        run_id: OLD.into(),
        head_sha: d.old_head.clone(),
        pull_request_url: Some(d.old_url.clone()),
    }
}

fn pr_of_new_run(w: &World) -> (NewPullRequest, delivery::gateway::PullRequestInfo) {
    w.gw.codecommit
        .prs
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p.client_request_token == format!("hamh-pr-{RUN}"))
        .cloned()
        .expect("the follow-up run opened a pull request")
}

fn comments_on(w: &World, id: &str) -> Vec<NewComment> {
    w.gw.codecommit
        .comments
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.pull_request_id == id)
        .cloned()
        .collect()
}

fn opened(out: &PushPrOutput) -> &FollowUpOpened {
    out.follow_up
        .as_ref()
        .expect("a follow-up run reports where it points")
}

fn conflict(r: Result<PushPrOutput, DeliveryError>) -> String {
    match r {
        Err(DeliveryError::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn invalid(r: Result<PushPrOutput, DeliveryError>) -> String {
    match r {
        Err(DeliveryError::Invalid(m)) => m,
        other => panic!("expected an invalid request, got {other:?}"),
    }
}

/// Nothing of the follow-up run reached the repository: no branch, no pull request, no comment.
fn nothing_delivered(w: &World) {
    let r = refs(&bare(w));
    assert!(
        !r.contains_key(&format!("refs/heads/harness/{RUN}")),
        "the branch of the follow-up run was pushed: {r:?}"
    );
    assert!(
        w.gw.codecommit
            .prs
            .lock()
            .unwrap()
            .iter()
            .all(|(p, _)| p.client_request_token != format!("hamh-pr-{RUN}")),
        "a pull request was opened"
    );
    assert!(
        w.gw.codecommit.comments.lock().unwrap().iter().all(|c| !c
            .client_request_token
            .starts_with(&format!("hamh-pr-{RUN}"))),
        "a comment was posted"
    );
}

// ---- the three cases --------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn while_the_old_branch_holds_the_delivered_commit_the_new_pull_request_targets_it() {
    let w = world();
    let d = after_first_delivery(&w).await;
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
            .await
            .unwrap();

    let r = refs(&bare(&w));
    assert_eq!(
        r.get(&format!("refs/heads/harness/{RUN}")),
        Some(&d.new_head)
    );
    assert_eq!(
        r.get(&format!("refs/heads/harness/{OLD}")),
        Some(&d.old_head),
        "the old branch is untouched"
    );
    assert_eq!(r.get("refs/heads/main"), Some(&d.base), "main is untouched");

    let (pr, info) = pr_of_new_run(&w);
    assert_eq!(pr.source_ref, format!("refs/heads/harness/{RUN}"));
    assert_eq!(pr.destination_ref, format!("refs/heads/harness/{OLD}"));
    assert_eq!(
        info.destination_commit, d.old_head,
        "the list of changes starts at the commit the old run delivered"
    );
    assert_eq!(info.source_commit, d.new_head);

    assert_eq!(
        opened(&out),
        &FollowUpOpened {
            destination_ref: format!("refs/heads/harness/{OLD}"),
            aim: Aim::OldBranch,
            old_pull_request_id: Some("1".into()),
            old_pull_request_linked: true,
        }
    );
    // The check rule is on the new pull request as on every one.
    assert_eq!(out.approval_rules, ["hamh-prcheck-required"]);
}

#[tokio::test]
async fn a_follow_up_without_a_root_build_file_gets_the_note_once_and_still_links_the_old_pull_request()
 {
    // No root buildspec.yml, no check rule; each pull request gets one note, and the follow-up comments and links are as before.
    let w = world();
    let d = after_first_delivery_with(&w, false).await;
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
            .await
            .unwrap();
    assert!(out.approval_rules.is_empty());
    assert!(w.gw.codecommit.rules.lock().unwrap().is_empty());
    assert!(opened(&out).old_pull_request_linked);
    let tokens = |id: &str| -> Vec<String> {
        comments_on(&w, id)
            .into_iter()
            .map(|c| c.client_request_token)
            .collect()
    };
    // PR 2 (this run): its table, the note, then the follow-up note; PR 1: its table and its note, then the link to PR 2.
    assert_eq!(
        tokens("2"),
        [
            format!("hamh-pr-{RUN}-c0"),
            format!("hamh-pr-{RUN}-nocheck"),
            format!("hamh-pr-{RUN}-follow-up")
        ]
    );
    assert_eq!(
        tokens("1"),
        [
            format!("hamh-pr-{OLD}-c0"),
            format!("hamh-pr-{OLD}-nocheck"),
            format!("hamh-pr-{RUN}-link")
        ]
    );
}

#[tokio::test]
async fn a_merged_old_pull_request_sends_the_new_one_to_the_destination_of_the_old_one() {
    for squash in [false, true] {
        let w = world();
        let d = after_first_delivery(&w).await;
        // The user merged PR 1 in the console and left the branch where it was.
        w.gw.codecommit.merge_pull_request("1", squash);
        assert_eq!(
            refs(&bare(&w)).get(&format!("refs/heads/harness/{OLD}")),
            Some(&d.old_head),
            "the merge left the branch (squash {squash})"
        );
        let out =
            w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
                .await
                .unwrap();
        let (pr, _) = pr_of_new_run(&w);
        assert_eq!(pr.destination_ref, "refs/heads/main", "squash {squash}");
        assert_eq!(opened(&out).aim, Aim::Merged, "squash {squash}");
        assert!(opened(&out).old_pull_request_linked, "squash {squash}");
    }
}

#[tokio::test]
async fn work_that_reached_the_destination_branch_by_git_alone_counts_as_merged() {
    // The user merged harness/OLD into main with git: PR 1 still reads OPEN, the branch still holds the delivered commit, and `main`
    // has the commit in its history.
    let w = world();
    let d = after_first_delivery(&w).await;
    w.gw.codecommit.set_branch(FIXTURE, "main", &d.old_head);
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
            .await
            .unwrap();
    let (pr, _) = pr_of_new_run(&w);
    assert_eq!(pr.destination_ref, "refs/heads/main");
    assert_eq!(opened(&out).aim, Aim::Merged);
}

#[tokio::test]
async fn a_deleted_old_branch_sends_the_new_pull_request_to_the_destination_of_the_old_one() {
    let w = world();
    let d = after_first_delivery(&w).await;
    w.gw.codecommit
        .delete_branch(FIXTURE, &format!("harness/{OLD}"));
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
            .await
            .unwrap();
    let (pr, info) = pr_of_new_run(&w);
    assert_eq!(pr.destination_ref, "refs/heads/main");
    assert_eq!(info.destination_commit, d.base);
    assert_eq!(opened(&out).aim, Aim::Deleted);
    assert_eq!(opened(&out).old_pull_request_id.as_deref(), Some("1"));
    // The branch of the follow-up run carries the old work with it: the push brought the commits the repository no longer had.
    assert_eq!(
        refs(&bare(&w)).get(&format!("refs/heads/harness/{RUN}")),
        Some(&d.new_head)
    );
}

#[tokio::test]
async fn a_moved_old_branch_is_a_delivery_conflict_and_nothing_is_delivered() {
    // Someone pushed one more commit to harness/OLD after the old run delivered, and a second case moved it to an unrelated commit.
    for variant in ["a descendant", "another line of history"] {
        let w = world();
        let d = after_first_delivery(&w).await;
        let moved = if variant == "a descendant" {
            let c = commit(&d.work, &[("late.md", Some("late\n"))], "someone's edit");
            // `commit` ran in the work repository; the bare repository gets the object through a fetch of the work branch.
            git(
                &bare(&w),
                &[
                    "fetch",
                    "-q",
                    d.work.to_str().unwrap(),
                    "HEAD:refs/hamh/late",
                ],
            );
            c
        } else {
            git(&d.work, &["checkout", "-q", "--orphan", "elsewhere"]);
            let c = commit(&d.work, &[("other.md", Some("other\n"))], "unrelated");
            git(
                &bare(&w),
                &[
                    "fetch",
                    "-q",
                    d.work.to_str().unwrap(),
                    "HEAD:refs/hamh/other",
                ],
            );
            c
        };
        w.gw.codecommit
            .set_branch(FIXTURE, &format!("harness/{OLD}"), &moved);
        let m = conflict(
            w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
                .await,
        );
        for want in [
            "delivery conflict",
            &format!("harness/{OLD}"),
            &d.old_head[..12],
            &moved[..12],
        ] {
            assert!(m.contains(want), "{variant}: {want} in {m}");
        }
        assert!(
            !["verified", "tested", "gated", "evaluated"]
                .iter()
                .any(|b| m.to_lowercase().contains(b)),
            "the harness claims nothing about the work: {m}"
        );
        nothing_delivered(&w);
    }
}

// ---- the destination of the old pull request -------------------------------------------------------------------------------

#[tokio::test]
async fn an_old_pull_request_aimed_at_another_branch_sends_the_new_one_there() {
    // The old run was itself a follow-up whose pull request targeted harness/THIRD; its branch is gone, harness/THIRD lives.
    let w = world();
    let d = after_first_delivery(&w).await;
    w.gw.codecommit
        .set_branch(FIXTURE, &format!("harness/{THIRD}"), &d.base);
    w.gw.codecommit.edit_pull_request("1", |i| {
        i.destination_ref = format!("refs/heads/harness/{THIRD}");
    });
    w.gw.codecommit
        .delete_branch(FIXTURE, &format!("harness/{OLD}"));
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
            .await
            .unwrap();
    let (pr, _) = pr_of_new_run(&w);
    assert_eq!(pr.destination_ref, format!("refs/heads/harness/{THIRD}"));
    assert_eq!(opened(&out).aim, Aim::Deleted);
}

#[tokio::test]
async fn a_destination_branch_that_is_gone_too_is_a_delivery_conflict() {
    let w = world();
    let d = after_first_delivery(&w).await;
    w.gw.codecommit.edit_pull_request("1", |i| {
        i.destination_ref = format!("refs/heads/harness/{THIRD}");
    });
    w.gw.codecommit
        .delete_branch(FIXTURE, &format!("harness/{OLD}"));
    let m = conflict(
        w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
            .await,
    );
    assert!(
        m.contains("delivery conflict")
            && m.contains(&format!("harness/{THIRD}"))
            && m.contains("no longer exists"),
        "{m}"
    );
    nothing_delivered(&w);
}

#[tokio::test]
async fn an_old_run_that_opened_no_pull_request_leaves_main_as_the_destination_and_nothing_to_link()
{
    let w = world();
    let d = after_first_delivery(&w).await;
    w.gw.codecommit
        .delete_branch(FIXTURE, &format!("harness/{OLD}"));
    let mut f = follow(&d);
    f.pull_request_url = None;
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, Some(f)))
            .await
            .unwrap();
    let (pr, _) = pr_of_new_run(&w);
    assert_eq!(pr.destination_ref, "refs/heads/main");
    let o = opened(&out);
    assert_eq!(
        (
            o.aim,
            o.old_pull_request_id.as_deref(),
            o.old_pull_request_linked
        ),
        (Aim::Deleted, None, false)
    );
    assert!(
        comments_on(&w, "1")
            .iter()
            .all(|c| !c.client_request_token.contains(RUN)),
        "no link was posted on the old pull request"
    );
}

// ---- the links --------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_two_pull_requests_link_to_each_other() {
    let w = world();
    let d = after_first_delivery(&w).await;
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
            .await
            .unwrap();
    // New pull request: the table comment, then a comment that names the old pull request, the branch it targets and why.
    let on_new = comments_on(&w, &out.pull_request_id);
    assert_eq!(on_new.len(), 2, "{on_new:?}");
    assert_eq!(on_new[0].content, "| table |");
    assert_eq!(
        on_new[0].before_commit_id, d.old_head,
        "the table starts at the delivered commit"
    );
    let note = &on_new[1].content;
    for want in [
        d.old_url.as_str(),
        OLD,
        &format!("harness/{OLD}"),
        &d.old_head[..12],
    ] {
        assert!(note.contains(want), "{want} in {note}");
    }
    // Old pull request: a comment with the link to the new one, on its own two commits.
    let on_old = comments_on(&w, "1");
    let link = on_old
        .iter()
        .find(|c| c.client_request_token == format!("hamh-pr-{RUN}-link"))
        .expect("the old pull request links to the new one");
    assert!(
        link.content.contains(&out.url) && link.content.contains(RUN),
        "{}",
        link.content
    );
    assert_eq!(
        (
            link.before_commit_id.as_str(),
            link.after_commit_id.as_str()
        ),
        (d.base.as_str(), d.old_head.as_str())
    );
}

#[tokio::test]
async fn a_merged_old_pull_request_whose_commits_read_equal_is_linked_through_its_merge_base() {
    let w = world();
    let d = after_first_delivery(&w).await;
    w.gw.codecommit.merge_pull_request("1", false);
    // After a fast-forward merge the destination tip can be the source commit; a comment needs two different commits.
    w.gw.codecommit.edit_pull_request("1", |i| {
        i.destination_commit = i.source_commit.clone();
    });
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
            .await
            .unwrap();
    assert!(opened(&out).old_pull_request_linked);
    let link = &comments_on(&w, "1")
        .into_iter()
        .find(|c| c.client_request_token == format!("hamh-pr-{RUN}-link-mb"))
        .expect("the merge-base pair has its own token");
    assert_eq!(link.before_commit_id, d.base, "the merge base");
    assert_eq!(link.after_commit_id, d.old_head);
}

#[tokio::test]
async fn an_old_pull_request_that_takes_no_comment_does_not_stop_the_new_one() {
    let w = world();
    let d = after_first_delivery(&w).await;
    w.gw.codecommit.merge_pull_request("1", false);
    // Every pair of commits the old pull request reports is equal: CodeCommit refuses any comment on it.
    w.gw.codecommit.edit_pull_request("1", |i| {
        i.destination_commit = i.source_commit.clone();
        i.merge_base = i.source_commit.clone();
    });
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
            .await
            .unwrap();
    let o = opened(&out);
    assert!(!o.old_pull_request_linked);
    assert_eq!(o.aim, Aim::Merged);
    let (_, _) = pr_of_new_run(&w);
    // The new pull request still names the old one, so the link exists on one side.
    let on_new = comments_on(&w, &out.pull_request_id);
    assert!(
        on_new.iter().any(|c| c.content.contains(&d.old_url)),
        "{on_new:?}"
    );
}

#[tokio::test]
async fn a_second_delivery_of_the_same_follow_up_posts_nothing_twice() {
    let w = world();
    let d = after_first_delivery(&w).await;
    let req = request(RUN, &d.new_head, Some(follow(&d)));
    let a = w.gw.push_pr(&req).await.unwrap();
    let comments_after_one = w.gw.codecommit.comments.lock().unwrap().len();
    let b = w.gw.push_pr(&req).await.unwrap();
    assert_eq!(a.pull_request_id, b.pull_request_id);
    assert_eq!(
        w.gw.codecommit.prs.lock().unwrap().len(),
        2,
        "PR 1 and PR 2"
    );
    assert_eq!(
        w.gw.codecommit.comments.lock().unwrap().len(),
        comments_after_one
    );
    assert_eq!(opened(&a), opened(&b));
}

// ---- refused before anything is read or pushed -----------------------------------------------------------------------------

#[tokio::test]
async fn a_follow_up_member_that_cannot_be_right_is_invalid_before_any_read_or_push() {
    let w = world();
    let d = after_first_delivery(&w).await;
    let reads = w.gw.blobs.reads.lock().unwrap().len();
    let calls = w.gw.codecommit.calls().len();
    let bad = |edit: &dyn Fn(&mut FollowUp)| {
        let mut f = follow(&d);
        edit(&mut f);
        request(RUN, &d.new_head, Some(f))
    };
    for (what, req) in [
        ("the run itself", bad(&|f| f.run_id = RUN.into())),
        ("not a ULID", bad(&|f| f.run_id = "not-a-ulid".into())),
        ("a short head", bad(&|f| f.head_sha = "abc".into())),
        (
            "an upper-case head",
            bad(&|f| f.head_sha = d.old_head.to_uppercase()),
        ),
        (
            "a link that is not ours",
            bad(&|f| {
                f.pull_request_url = Some("https://example.invalid/pull-requests/1/details".into())
            }),
        ),
        (
            "a link of another repository",
            bad(&|f| f.pull_request_url = Some(pr_console_url("hamh-e2e-other-20260925", "1"))),
        ),
        (
            "a link with a text id",
            bad(&|f| f.pull_request_url = Some(pr_console_url(FIXTURE, "1x"))),
        ),
    ] {
        let m = invalid(w.gw.push_pr(&req).await);
        assert!(!m.is_empty(), "{what}");
    }
    assert_eq!(
        w.gw.blobs.reads.lock().unwrap().len(),
        reads,
        "no read of the bucket"
    );
    assert_eq!(w.gw.codecommit.calls().len(), calls, "no CodeCommit call");
    nothing_delivered(&w);
}

#[tokio::test]
async fn a_pull_request_that_is_not_the_old_runs_is_invalid_and_nothing_is_pushed() {
    // PR 2: a pull request of another branch of the same repository. PR ids are global, so the link of PR 1 could be edited to it.
    let w = world();
    let d = after_first_delivery(&w).await;
    w.gw.codecommit
        .set_branch(FIXTURE, &format!("harness/{THIRD}"), &d.old_head);
    w.gw.codecommit
        .create_pull_request(&NewPullRequest {
            repository: FIXTURE.into(),
            title: "t".into(),
            description: "d".into(),
            source_ref: format!("refs/heads/harness/{THIRD}"),
            destination_ref: "refs/heads/main".into(),
            client_request_token: format!("hamh-pr-{THIRD}"),
        })
        .await
        .unwrap();
    let mut f = follow(&d);
    f.pull_request_url = Some(pr_console_url(FIXTURE, "2"));
    let m = invalid(w.gw.push_pr(&request(RUN, &d.new_head, Some(f))).await);
    assert!(m.contains("2") && m.contains(OLD), "{m}");
    nothing_delivered(&w);
}

#[tokio::test]
async fn a_pull_request_of_another_repository_is_invalid() {
    let w = world();
    let d = after_first_delivery(&w).await;
    // The same ids in a second repository.
    let other = "hamh-e2e-other-20260925";
    git(
        &w.remotes_dir,
        &[
            "clone",
            "-q",
            "--bare",
            bare(&w).to_str().unwrap(),
            w.remotes_dir.join(format!("{other}.git")).to_str().unwrap(),
        ],
    );
    w.gw.codecommit
        .create_pull_request(&NewPullRequest {
            repository: other.into(),
            title: "t".into(),
            description: "d".into(),
            source_ref: format!("refs/heads/harness/{OLD}"),
            destination_ref: "refs/heads/main".into(),
            client_request_token: "x".into(),
        })
        .await
        .unwrap();
    let mut f = follow(&d);
    f.pull_request_url = Some(pr_console_url(FIXTURE, "2"));
    let m = invalid(w.gw.push_pr(&request(RUN, &d.new_head, Some(f))).await);
    assert!(m.contains(other) || m.contains("repository"), "{m}");
    nothing_delivered(&w);
}

// ---- the wire and the unchanged run ----------------------------------------------------------------------------------------

#[test]
fn the_request_reads_its_follow_up_member_off_the_wire_and_refuses_unknown_members() {
    let wire = |extra: Value| {
        let mut v = json!({
            "operation": "push-pr", "runId": RUN, "target": format!("cc:{FIXTURE}"), "headSha": "a".repeat(40),
            "title": "t", "description": "d"
        });
        v["followUp"] = extra;
        serde_json::from_value::<GatewayRequest>(v)
    };
    let ok = wire(json!({"runId": OLD, "headSha": "b".repeat(40), "pullRequestUrl": pr_console_url(FIXTURE, "7")})).unwrap();
    let GatewayRequest::PushPr(r) = ok else {
        panic!("a push-pr request")
    };
    assert_eq!(
        r.follow_up,
        Some(FollowUp {
            run_id: OLD.into(),
            head_sha: "b".repeat(40),
            pull_request_url: Some(pr_console_url(FIXTURE, "7")),
        })
    );
    let bare_one = wire(json!({"runId": OLD, "headSha": "b".repeat(40)})).unwrap();
    assert!(matches!(
        bare_one,
        GatewayRequest::PushPr(PushPrRequest {
            follow_up: Some(FollowUp {
                pull_request_url: None,
                ..
            }),
            ..
        })
    ));
    assert!(wire(json!({"runId": OLD, "headSha": "b".repeat(40), "branch": "x"})).is_err());
}

#[tokio::test]
async fn a_run_that_continues_nothing_is_delivered_as_before() {
    let w = world();
    let d = after_first_delivery(&w).await;
    // PR 1 of the old run is the run that continues nothing: main as destination, no follow-up member in the answer.
    let (pr, _) = w.gw.codecommit.prs.lock().unwrap()[0].clone();
    assert_eq!(pr.destination_ref, "refs/heads/main");
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, None))
            .await
            .unwrap();
    assert!(out.follow_up.is_none());
    assert!(
        !serde_json::to_value(&out)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("followUp")
    );
    let (pr, _) = pr_of_new_run(&w);
    assert_eq!(pr.destination_ref, "refs/heads/main");
}

// ---- the link in a console URL ----------------------------------------------------------------------------------------------

#[test]
fn the_pull_request_id_is_read_only_from_the_link_this_crate_writes() {
    assert_eq!(
        pull_request_id_in(&pr_console_url(FIXTURE, "405"), FIXTURE).as_deref(),
        Some("405")
    );
    for (what, url) in [
        (
            "another repository",
            pr_console_url("hamh-e2e-other", "405"),
        ),
        ("a text id", pr_console_url(FIXTURE, "4o5")),
        ("an empty id", pr_console_url(FIXTURE, "")),
        ("a very long id", pr_console_url(FIXTURE, &"7".repeat(40))),
        ("an id with a path", pr_console_url(FIXTURE, "1/../2")),
        (
            "another page",
            pr_console_url(FIXTURE, "405").replace("/details", "/activity"),
        ),
        (
            "another host",
            pr_console_url(FIXTURE, "405").replace("console.aws.amazon.com", "example.invalid"),
        ),
        (
            "a trailing query",
            format!("{}?x=1", pr_console_url(FIXTURE, "405")),
        ),
        (
            "a trailing space",
            format!("{} ", pr_console_url(FIXTURE, "405")),
        ),
        ("nothing", String::new()),
    ] {
        assert_eq!(pull_request_id_in(&url, FIXTURE), None, "{what}: {url}");
    }
}

// ---- the decision, by itself ------------------------------------------------------------------------------------------------

fn facts<'a>(old_tip: Option<&'a str>, dest_tip: Option<&'a str>, merged: bool) -> Facts<'a> {
    Facts {
        old_run: OLD,
        recorded_head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        old_branch_tip: old_tip,
        destination_ref: "refs/heads/main",
        destination_tip: dest_tip,
        merged,
    }
}

#[test]
fn the_decision_table() {
    let h = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let dest = "refs/heads/main".to_string();
    let old = format!("refs/heads/harness/{OLD}");
    let open = |r: &str, aim| Plan::Open {
        destination_ref: r.to_string(),
        aim,
    };
    // The branch holds the delivered commit.
    assert_eq!(
        decide(&facts(Some(h), Some(other), false)),
        open(&old, Aim::OldBranch)
    );
    assert_eq!(
        decide(&facts(Some(h), None, false)),
        open(&old, Aim::OldBranch),
        "the destination is not needed to stack"
    );
    // Merged, with the branch kept, moved or deleted.
    assert_eq!(
        decide(&facts(Some(h), Some(other), true)),
        open(&dest, Aim::Merged)
    );
    assert_eq!(
        decide(&facts(Some(other), Some(other), true)),
        open(&dest, Aim::Merged),
        "merged outranks a moved branch"
    );
    assert_eq!(
        decide(&facts(None, Some(other), true)),
        open(&dest, Aim::Merged)
    );
    // Deleted and not merged.
    assert_eq!(
        decide(&facts(None, Some(other), false)),
        open(&dest, Aim::Deleted)
    );
    // Alive but moved.
    let Plan::Conflict(m) = decide(&facts(Some(other), Some(other), false)) else {
        panic!("moved is a conflict")
    };
    assert!(m.starts_with("delivery conflict"), "{m}");
    // A destination that is gone is a conflict whenever it is needed.
    for f in [facts(None, None, false), facts(Some(h), None, true)] {
        let Plan::Conflict(m) = decide(&f) else {
            panic!("a missing destination is a conflict: {f:?}")
        };
        assert!(m.contains("no longer exists") && m.contains("main"), "{m}");
    }
}

// ---- the fake holds to the API reference ------------------------------------------------------------------------------------

#[tokio::test]
async fn the_fake_refuses_what_codecommit_refuses() {
    let w = world();
    let d = after_first_delivery(&w).await;
    let cc = &w.gw.codecommit;
    let pr = |src: &str, dest: &str, token: &str| NewPullRequest {
        repository: FIXTURE.into(),
        title: "t".into(),
        description: "d".into(),
        source_ref: src.into(),
        destination_ref: dest.into(),
        client_request_token: token.into(),
    };
    // SourceAndDestinationAreSameException.
    let e = cc
        .create_pull_request(&pr("refs/heads/main", "refs/heads/main", "t1"))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, DeliveryError::Invalid(m) if m.contains("SourceAndDestinationAreSameException")),
        "{e}"
    );
    // InvalidReferenceNameException: a bare branch name is not a reference.
    let e = cc
        .create_pull_request(&pr("harness/x", "refs/heads/main", "t2"))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, DeliveryError::Invalid(m) if m.contains("InvalidReferenceNameException")),
        "{e}"
    );
    // IdempotencyParameterMismatchException: the token of PR 1 with another destination.
    let e = cc
        .create_pull_request(&pr(
            &format!("refs/heads/harness/{OLD}"),
            &format!("refs/heads/harness/{RUN}"),
            &format!("hamh-pr-{OLD}"),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, DeliveryError::Invalid(m) if m.contains("IdempotencyParameterMismatchException")),
        "{e}"
    );
    // The same parameters return the first pull request.
    let (sent, _) = cc.prs.lock().unwrap()[0].clone();
    let first = cc.create_pull_request(&sent).await.unwrap();
    assert_eq!(first.pull_request_id, "1");
    // RepositoryNotAssociatedWithPullRequestException.
    let note = |repo: &str, id: &str, before: &str, after: &str| NewComment {
        repository: repo.into(),
        pull_request_id: id.into(),
        before_commit_id: before.into(),
        after_commit_id: after.into(),
        content: "c".into(),
        client_request_token: format!("tok-{repo}-{id}"),
    };
    let e = cc
        .post_comment(&note("hamh-e2e-other", "1", &d.base, &d.old_head))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, DeliveryError::Invalid(m) if m.contains("RepositoryNotAssociatedWithPullRequestException")),
        "{e}"
    );
    // PullRequestDoesNotExistException and CommitDoesNotExistException are the races the adapter retries.
    let e = cc
        .post_comment(&note(FIXTURE, "99", &d.base, &d.old_head))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, DeliveryError::Unavailable(m) if m.contains("PullRequestDoesNotExistException")),
        "{e}"
    );
    let e = cc
        .post_comment(&note(FIXTURE, "1", &"c".repeat(40), &d.old_head))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, DeliveryError::Unavailable(m) if m.contains("CommitDoesNotExistException")),
        "{e}"
    );
    // GetPullRequest of an id nobody made.
    let e = cc.get_pull_request("99").await.unwrap_err();
    assert!(
        matches!(&e, DeliveryError::Unavailable(m) if m.contains("PullRequestDoesNotExistException")),
        "{e}"
    );
    // GetPullRequest answers the fields a follow-up reads.
    let got = cc.get_pull_request("1").await.unwrap();
    assert_eq!(
        (
            got.repository.as_str(),
            got.source_ref.as_str(),
            got.destination_ref.as_str(),
            got.merged
        ),
        (
            FIXTURE,
            format!("refs/heads/harness/{OLD}").as_str(),
            "refs/heads/main",
            false
        )
    );
    assert_eq!(got.merge_base, d.base);
}

// ---- review findings ---------------------------------------------------------

#[tokio::test]
async fn a_follow_up_head_the_new_head_does_not_descend_from_is_invalid_and_nothing_is_pushed() {
    // Every Open plan lists the changes since the recorded commit; if that commit is not in the history of the new head, any list is wrong.
    let w = world();
    let d = after_first_delivery(&w).await;
    let mut f = follow(&d);
    f.head_sha = "f".repeat(40);
    let m = invalid(w.gw.push_pr(&request(RUN, &d.new_head, Some(f))).await);
    assert!(m.contains("history") && m.contains(&"f".repeat(12)), "{m}");
    nothing_delivered(&w);
}

#[tokio::test]
async fn the_link_on_the_old_pull_request_tries_the_documented_pair_first() {
    // CreatePullRequest's destination commit is the pair the harness has proven live on new pull requests; the merge base is the fallback.
    let w = world();
    let d = after_first_delivery(&w).await;
    let first_commit = git(&bare(&w), &["rev-parse", &format!("{}~1", d.base)]);
    w.gw.codecommit.edit_pull_request("1", |i| {
        i.merge_base = first_commit.clone();
    });
    w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
        .await
        .unwrap();
    let link = comments_on(&w, "1")
        .into_iter()
        .find(|c| c.client_request_token == format!("hamh-pr-{RUN}-link"))
        .expect("posted with the destination pair");
    assert_eq!(
        link.before_commit_id, d.base,
        "the destination commit, not the merge base"
    );
    assert!(
        !comments_on(&w, "1")
            .iter()
            .any(|c| c.client_request_token == format!("hamh-pr-{RUN}-link-mb")),
        "the fallback pair was not needed"
    );
}

#[tokio::test]
async fn after_a_squash_merge_the_new_pull_request_says_its_list_repeats_the_old_changes() {
    // A squash merge puts the old work in main under a new commit, so the commits of the old run are not in main's history and the new pull
    // request lists them again (their content merges cleanly). The note says so; after a fast-forward merge it says nothing of the kind.
    for (squash, repeats) in [(true, true), (false, false)] {
        let w = world();
        let d = after_first_delivery(&w).await;
        w.gw.codecommit.merge_pull_request("1", squash);
        let out =
            w.gw.push_pr(&request(RUN, &d.new_head, Some(follow(&d))))
                .await
                .unwrap();
        let note = comments_on(&w, &out.pull_request_id)
            .into_iter()
            .map(|c| c.content)
            .find(|c| c.contains("Follow-up of run"))
            .unwrap();
        assert_eq!(
            note.contains("squash") && note.contains("listed again"),
            repeats,
            "squash {squash}: {note}"
        );
    }
}

#[tokio::test]
async fn the_note_does_not_claim_a_merged_pull_request_when_only_the_history_says_so() {
    let w = world();
    let d = after_first_delivery(&w).await;
    w.gw.codecommit.set_branch(FIXTURE, "main", &d.old_head);
    let mut f = follow(&d);
    f.pull_request_url = None;
    let out =
        w.gw.push_pr(&request(RUN, &d.new_head, Some(f)))
            .await
            .unwrap();
    let note = comments_on(&w, &out.pull_request_id)
        .into_iter()
        .map(|c| c.content)
        .find(|c| c.contains("Follow-up of run"))
        .unwrap();
    assert!(note.contains("already in main"), "{note}");
    assert!(!note.contains("is merged"), "{note}");
}
