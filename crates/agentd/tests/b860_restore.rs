//! The restore of a session from a copy of the work that needs the bundle the run started from.
//!
//! A copy may hold what the run changed and leave out what the base holds; its header then names the commits it needs. A session that
//! starts from such a copy downloads the base too, clones it without a checkout, fetches the copy into it and checks out the commit
//! the service named, so the tree is written once. A restore object that names no commit it needs is restored as it always was, with
//! no read of the base. A base that cannot be read ends the setup with a sentence that says so, and no engine starts.
#![allow(clippy::unwrap_used)]

mod b860_thin_support;
mod common;
mod direct_support;

use agentd::workspace::prepare_from_bundle_state;
use b860_thin_support::{
    Lab, base_form, lab, later_session, needed, record_of, restored, stage_base, stored_copy,
    summary_of, thin, whole_form,
};
use common::{git, python_calc, scratch};
use direct_support::{Direct, Opts, RUN, needs_the_base};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::runtime::TurnOutcome;
use std::path::Path;

/// The names in the folder `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A repository whose base has commits below its head, so that a copy of the base's own head still names a commit it needs.
fn lab_with_history(label: &str) -> Lab {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    for n in 1..=2 {
        std::fs::write(repo.join(format!("src/h{n}.py")), format!("h = {n}\n")).unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", &format!("history {n}")]);
    }
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let base_bundle = dir.join("repo.bundle");
    stage_base(&repo, &base_bundle);
    Lab {
        dir,
        repo,
        base_bundle,
        base,
    }
}

/// Session 1 of a run whose copies have the form `base`, run to its end: the turn, its stored copy, that copy's commit and the
/// run's base.
async fn first_session_with_a_thin_copy() -> (Direct, Vec<u8>, String, String) {
    let d1 = Direct::start(base_form(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    }))
    .await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let saved = stored_copy(&d1, 1);
    assert!(
        needs_the_base(&saved),
        "TRIGGER PRESENT: the copy to restore names a commit it needs"
    );
    let head = record_of(&d1, 1)["headSha"].as_str().unwrap().to_string();
    let base = d1.summary().git.base_sha.clone().unwrap();
    (d1, saved, head, base)
}

#[tokio::test(flavor = "multi_thread")]
async fn b860_restore_takes_the_base_first() {
    let (d1, saved, head, base) = first_session_with_a_thin_copy().await;
    // Session 2 in a new machine, whose own copies hold the whole history: reading a copy does not depend on the
    // form this session writes.
    let d2 = later_session(
        whole_form(Opts {
            scenario: "end_turn_done",
            ..Opts::default()
        }),
        &d1,
        &keys::snapshot_bundle_key_t(RUN, 1),
        saved,
        &head,
        &base,
    )
    .await;
    let (_, resp) = d2.invoke().await;
    assert_ne!(
        summary_of(&d2, 2).outcome,
        DirectOutcome::StoppedSetupFailed,
        "{resp:?}"
    );
    // The engine started and got its prompt.
    assert!(
        !d2.prompts_received().is_empty(),
        "the engine of session 2 started: {resp:?}"
    );
    // The base was downloaded once, whole, and nothing else of it was read.
    assert_eq!(d2.aws.gets_of(&keys::repo_bundle_key(RUN)), vec![None]);
    // The repository is the one the service named: the commit of the copy is in it, on the harness branch, and every object is there.
    let wt = d2.worktree();
    git(&wt, &["merge-base", "--is-ancestor", &head, "HEAD"]);
    git(&wt, &["merge-base", "--is-ancestor", &base, "HEAD"]);
    git(&wt, &["fsck", "--no-progress", "--connectivity-only"]);
    assert!(
        std::fs::read_to_string(wt.join("src/calc.py"))
            .unwrap()
            .contains("percent_change"),
        "the work of session 1 is in the tree of session 2"
    );
    // The tree was written once: the clone made no checkout, and one checkout put the copy's commit on the harness branch.
    let moves = git(&wt, &["reflog", "show", "--format=%gs", "HEAD"]);
    let checkouts: Vec<&str> = moves
        .lines()
        .filter(|l| l.starts_with("checkout:"))
        .collect();
    assert_eq!(checkouts.len(), 1, "{moves}");
    assert!(
        checkouts[0].ends_with(&format!("to harness/{RUN}/direct")),
        "{moves}"
    );
}

#[tokio::test]
async fn b860_restore_writes_the_tree_once_and_checks_the_copy() {
    // The restore itself, without a turn: the base first, no checkout by the clone, the copy fetched, one checkout.
    let l = lab("b860-restore-once");
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let copy = s.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(needed(&copy.bundle), vec![l.base.clone()]);
    let got = restored(&l, "once", &copy.bundle, &copy.commit).await;
    assert_eq!(
        git(&got, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "harness/run/direct"
    );
    let moves = git(&got, &["reflog", "show", "--format=%gs", "HEAD"]);
    assert_eq!(
        moves.lines().filter(|m| m.starts_with("checkout:")).count(),
        1,
        "{moves}"
    );
    // A checkout an earlier invocation left is found first and returned as it is, whatever the bundles say.
    let (again, fresh) = prepare_from_bundle_state(
        &l.dir.join("restore-once"),
        "run",
        "direct",
        &l.dir.join("no-such.bundle"),
        Some(&l.dir.join("no-such-base.bundle")),
    )
    .await
    .unwrap();
    assert!(!fresh);
    assert_eq!(again.repo, got);

    // A copy that needs the base, restored without it or onto another repository, is refused by git and no checkout is left behind
    // that could be taken for the work.
    let alone =
        prepare_from_bundle_state(&l.dir.join("alone"), "run", "direct", &copy.bundle, None).await;
    assert!(
        alone.is_err(),
        "a copy that needs the base does not clone alone"
    );
    assert!(
        !l.dir.join("alone/run/direct/repo/.git").exists(),
        "a clone that failed leaves no repository"
    );
    let other = scratch("b860-restore-other");
    let other_repo = python_calc(&other);
    // The fixture makes its first commit from the same files, author and message every time, so two fixture repositories made within
    // one second hold the same first commit, and this one would then hold the commit the copy needs. Its first commit gets a message
    // of its own, so that it is another history whatever the clock says.
    git(
        &other_repo,
        &["commit", "-q", "--amend", "-m", "another base"],
    );
    assert_ne!(
        git(&other_repo, &["rev-list", "--max-parents=0", "HEAD"]),
        l.base,
        "the other repository does not hold the commit the copy needs"
    );
    std::fs::write(other_repo.join("src/z.py"), "z = 0\n").unwrap();
    git(&other_repo, &["add", "-A"]);
    git(&other_repo, &["commit", "-q", "-m", "another history"]);
    let other_base = other.join("repo.bundle");
    stage_base(&other_repo, &other_base);
    let wrong = prepare_from_bundle_state(
        &l.dir.join("wrong-base"),
        "run",
        "direct",
        &copy.bundle,
        Some(&other_base),
    )
    .await;
    let why = wrong
        .expect_err("the copy needs a commit that this base does not hold")
        .to_string();
    assert!(why.contains("prerequisite"), "{why}");
    // The base was cloned before git refused the copy: that clone is not left where the next invocation looks for a checkout.
    assert!(
        !l.dir.join("wrong-base/run/direct/repo/.git").exists(),
        "a restore that failed after the base was cloned leaves no repository"
    );
}

/// A restore that takes the base first has three steps: the clone of the base without a checkout, the fetch of the copy, the
/// checkout. A failure after the first step must not leave a repository in the place of the checkout: the next invocation on that
/// machine returns whatever it finds there as the work of an earlier invocation, and a clone of the base alone has no file in its
/// tree. So the restore is all or nothing, and the next invocation restores again.
#[tokio::test]
async fn b860_a_restore_that_failed_after_the_base_was_cloned_is_made_again() {
    // A. An ordinary copy. The first call gets the copy cut short: the base is cloned, and the fetch of the copy fails.
    let l = lab("b860-restore-again");
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let copy = s.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(needed(&copy.bundle), vec![l.base.clone()]);
    let whole = std::fs::read(&copy.bundle).unwrap();
    let cut = l.dir.join("cut.bundle");
    std::fs::write(&cut, &whole[..whole.len() - 20]).unwrap();
    let root = l.dir.join("restore-again");
    let place = root.join("run").join("direct");
    let first = prepare_from_bundle_state(&root, "run", "direct", &cut, Some(&l.base_bundle)).await;
    let why = first
        .expect_err(
            "TRIGGER PRESENT: the fetch of a copy that was cut fails after the base was cloned",
        )
        .to_string();
    assert!(why.contains("git fetch"), "{why}");
    assert!(
        !place.join("repo").join(".git").exists(),
        "a restore that failed leaves no repository that the next call takes for a checkout"
    );
    let (wt, fresh) =
        prepare_from_bundle_state(&root, "run", "direct", &copy.bundle, Some(&l.base_bundle))
            .await
            .expect("the next call restores the copy");
    assert!(fresh, "the next call made the checkout itself");
    assert_eq!(git(&wt.repo, &["rev-parse", "HEAD"]), copy.commit);
    assert_eq!(git(&wt.repo, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(wt.repo.join("src/a.py")).unwrap(),
        "a = 1\n"
    );
    git(&wt.repo, &["fsck", "--no-progress", "--connectivity-only"]);
    assert_eq!(
        entries(&place),
        ["repo"],
        "nothing of the restore that failed is left beside the checkout"
    );

    // B. The copy of the end of a session that changed nothing: its commit is the base's own head. A clone of the base alone
    // stands at that very commit, with the base behind it, and has no file in its tree: the two things the restore check asks
    // would hold for it.
    let l = lab_with_history("b860-restore-again-end");
    let mut s = thin(&l, "data").await;
    let end = s.final_copy(Some(&l.base)).await.unwrap();
    assert_eq!(
        end.commit, l.base,
        "TRIGGER PRESENT: the copy of the end names the base's own head"
    );
    assert!(
        !needed(&end.bundle).is_empty(),
        "TRIGGER PRESENT: and it names a commit it needs, so the restore takes the base first"
    );
    let no_bundle = l.dir.join("no.bundle");
    std::fs::write(&no_bundle, b"this file is no bundle\n").unwrap();
    let root = l.dir.join("restore-again-end");
    let place = root.join("run").join("direct");
    let first =
        prepare_from_bundle_state(&root, "run", "direct", &no_bundle, Some(&l.base_bundle)).await;
    assert!(
        first.is_err(),
        "TRIGGER PRESENT: the restore failed after the base was cloned"
    );
    let (wt, fresh) =
        prepare_from_bundle_state(&root, "run", "direct", &end.bundle, Some(&l.base_bundle))
            .await
            .expect("the next call restores the copy of the end");
    assert!(fresh, "the next call made the checkout itself");
    assert_eq!(git(&wt.repo, &["rev-parse", "HEAD"]), l.base);
    assert_eq!(
        git(&wt.repo, &["status", "--porcelain"]),
        "",
        "every file of the head is in the tree"
    );
    assert!(wt.repo.join("src/calc.py").is_file());
    assert!(wt.repo.join("src/h2.py").is_file());
    assert_eq!(entries(&place), ["repo"]);
}

/// An invocation that finds the checkout an earlier invocation of the run left on its machine restores nothing, so it does not
/// download the bundle the run started from: a session whose copies leave that bundle out asks for its first 65,536 bytes, and a
/// session whose copies are whole asks for nothing of it and goes to work when the bucket cannot give it.
#[tokio::test(flavor = "multi_thread")]
async fn b860_an_invocation_that_finds_its_checkout_does_not_download_the_base() {
    let (d1, saved, head, base) = first_session_with_a_thin_copy().await;
    let base_object = d1.aws.get(&keys::repo_bundle_key(RUN)).unwrap();
    for (name, thin_copies, base_in_bucket, reads) in [
        (
            "copies above the base",
            true,
            true,
            vec![Some("bytes=0-65535".to_string())],
        ),
        ("whole copies, no base in the bucket", false, false, vec![]),
    ] {
        let opts = Opts {
            scenario: "end_turn_done",
            ..Opts::default()
        };
        let d2 = later_session(
            if thin_copies {
                base_form(opts)
            } else {
                whole_form(opts)
            },
            &d1,
            &keys::snapshot_bundle_key_t(RUN, 1),
            saved.clone(),
            &head,
            &base,
        )
        .await;
        // The checkout of an earlier invocation of session 2 on this machine: the product's own restore, at the commit the plan names.
        let (copy_file, base_file) = (
            d2.dir.join("earlier.bundle"),
            d2.dir.join("earlier-base.bundle"),
        );
        std::fs::write(&copy_file, &saved).unwrap();
        std::fs::write(&base_file, &base_object).unwrap();
        let (left, fresh) = prepare_from_bundle_state(
            &d2.app.cfg.workspace_root,
            RUN,
            "direct",
            &copy_file,
            Some(&base_file),
        )
        .await
        .unwrap();
        assert!(fresh);
        assert_eq!(left.repo, d2.worktree());
        assert_eq!(git(&left.repo, &["rev-parse", "HEAD"]), head);
        if !base_in_bucket {
            d2.aws.remove(&keys::repo_bundle_key(RUN));
        }
        let (_, resp) = d2.invoke().await;
        assert_ne!(
            summary_of(&d2, 2).outcome,
            DirectOutcome::StoppedSetupFailed,
            "{name}: {resp:?}"
        );
        assert!(
            !d2.prompts_received().is_empty(),
            "{name}: the engine went to work in the checkout it found: {resp:?}"
        );
        assert_eq!(
            d2.aws.gets_of(&keys::repo_bundle_key(RUN)),
            reads,
            "{name}: what the invocation asked of the bundle the run started from"
        );
        assert!(
            !d2.agent_data().join("base.bundle").exists(),
            "{name}: no base file"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b860_restore_of_a_whole_bundle_is_unchanged() {
    // Session 1 with copies that hold the whole history.
    let d1 = Direct::start(whole_form(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    }))
    .await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    // Session 1 restored the base itself: one download, as always.
    assert_eq!(d1.aws.gets_of(&keys::repo_bundle_key(RUN)), vec![None]);
    let s = d1.summary();
    let head = s.git.head_sha.clone().unwrap();
    let base = s.git.base_sha.clone().unwrap();
    let whole_copy = stored_copy(&d1, 1);
    assert!(!needs_the_base(&whole_copy));
    let copy_head = record_of(&d1, 1)["headSha"].as_str().unwrap().to_string();
    // A later session restores a whole object of each kind: the earlier session's own bundle, and its saved copy. The base is
    // neither downloaded nor asked for.
    for (name, key, object, object_head) in [
        (
            "session bundle",
            keys::bundle_key_t(RUN, 1),
            d1.aws.get(&keys::bundle_key(RUN)).unwrap(),
            head.clone(),
        ),
        (
            "saved copy",
            keys::snapshot_bundle_key_t(RUN, 1),
            whole_copy.clone(),
            copy_head.clone(),
        ),
    ] {
        // The later session writes whole copies too: a session whose copies leave out the base asks for the base's header, which
        // is the writer's read and none of the restore's.
        let d2 = later_session(
            whole_form(Opts::default()),
            &d1,
            &key,
            object,
            &object_head,
            &base,
        )
        .await;
        let (_, resp) = d2.invoke().await;
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{name}: {resp:?}");
        assert_eq!(
            d2.aws.gets_of(&keys::repo_bundle_key(RUN)),
            Vec::<Option<String>>::new(),
            "{name}: a whole restore object reads nothing of the base"
        );
        assert!(
            !d2.agent_data().join("base.bundle").exists(),
            "{name}: no base file"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b860_restore_says_when_the_base_is_missing() {
    let (d1, saved, head, base) = first_session_with_a_thin_copy().await;
    // The bucket of session 2 holds the copy and no base.
    let d2 = later_session(
        Opts::default(),
        &d1,
        &keys::snapshot_bundle_key_t(RUN, 1),
        saved.clone(),
        &head,
        &base,
    )
    .await;
    d2.aws.remove(&keys::repo_bundle_key(RUN));
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    let summary = summary_of(&d2, 2);
    assert_eq!(summary.outcome, DirectOutcome::StoppedSetupFailed);
    let error = resp.error.clone().unwrap_or_default();
    assert!(
        error.contains("worktree: the copy to restore needs the run's base bundle: "),
        "{error}"
    );
    // No engine started on a repository that is not the work.
    assert!(d2.prompts_received().is_empty(), "{resp:?}");
    assert!(d2.engine_sessions().is_empty(), "{resp:?}");
    assert!(!d2.worktree().join(".git").exists(), "no checkout was made");

    // A bundle the run is to start from that itself needs commits is no base: the setup says so.
    let d = Direct::start(Opts::default()).await;
    d.aws.put(&keys::repo_bundle_key(RUN), saved);
    let (_, resp) = d.invoke().await;
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedSetupFailed);
    let error = resp.error.clone().unwrap_or_default();
    assert!(
        error.contains("worktree: the run's base bundle needs commits it does not hold"),
        "{error}"
    );
    assert!(d.prompts_received().is_empty(), "{resp:?}");
}

#[tokio::test]
async fn b860_restore_from_a_rewritten_and_an_orphan_history() {
    // A base with three commits, so that a rewritten head has something below it.
    let make = |label: &str| -> Lab {
        let dir = scratch(label);
        let repo = python_calc(&dir);
        for n in 1..=2 {
            std::fs::write(repo.join(format!("src/h{n}.py")), format!("h = {n}\n")).unwrap();
            git(&repo, &["add", "-A"]);
            git(&repo, &["commit", "-q", "-m", &format!("history {n}")]);
        }
        let base = git(&repo, &["rev-parse", "HEAD"]);
        let base_bundle = dir.join("repo.bundle");
        stage_base(&repo, &base_bundle);
        Lab {
            dir,
            repo,
            base_bundle,
            base,
        }
    };

    // The engine amends the base commit: the base is no ancestor of its head any more.
    let l = make("b860-amend");
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/h2.py"), "h = 22\n").unwrap();
    git(&l.repo, &["add", "-A"]);
    git(
        &l.repo,
        &["commit", "-q", "--amend", "-m", "history 2, amended"],
    );
    std::fs::write(l.repo.join("src/work.py"), "work = 1\n").unwrap();
    let copy = s.snapshot().await.unwrap().expect("the tree changed");
    assert!(!needed(&copy.bundle).is_empty());
    let got = restored(&l, "amend", &copy.bundle, &copy.commit).await;
    assert_eq!(
        std::fs::read_to_string(got.join("src/h2.py")).unwrap(),
        "h = 22\n"
    );
    assert_eq!(
        std::fs::read_to_string(got.join("src/work.py")).unwrap(),
        "work = 1\n"
    );
    // The tree of the copy is the tree of the work as it is.
    git(&l.repo, &["add", "-A"]);
    assert_eq!(
        git(&got, &["rev-parse", "HEAD^{tree}"]),
        git(&l.repo, &["write-tree"])
    );

    // The engine resets to an earlier commit and builds another line of history on it.
    let l = make("b860-reset");
    let mut s = thin(&l, "data").await;
    git(&l.repo, &["reset", "-q", "--hard", "HEAD~2"]);
    std::fs::write(l.repo.join("src/other.py"), "other = 1\n").unwrap();
    git(&l.repo, &["add", "-A"]);
    git(&l.repo, &["commit", "-q", "-m", "another line"]);
    std::fs::write(l.repo.join("src/work.py"), "work = 2\n").unwrap();
    let copy = s.snapshot().await.unwrap().expect("the tree changed");
    let got = restored(&l, "reset", &copy.bundle, &copy.commit).await;
    assert!(!got.join("src/h2.py").exists(), "the reset is in the copy");
    assert_eq!(
        std::fs::read_to_string(got.join("src/other.py")).unwrap(),
        "other = 1\n"
    );
    assert_eq!(
        std::fs::read_to_string(got.join("src/work.py")).unwrap(),
        "work = 2\n"
    );

    // The engine starts an orphan branch: its history shares no commit with the base, and the copy needs nothing.
    let l = make("b860-orphan");
    let mut s = thin(&l, "data").await;
    git(&l.repo, &["checkout", "-q", "--orphan", "fresh"]);
    git(&l.repo, &["commit", "-q", "-m", "a new root"]);
    std::fs::write(l.repo.join("src/work.py"), "work = 3\n").unwrap();
    let copy = s.snapshot().await.unwrap().expect("the tree changed");
    assert!(
        needed(&copy.bundle).is_empty(),
        "an orphan history has nothing below it that the base holds as a commit"
    );
    let got = restored(&l, "orphan", &copy.bundle, &copy.commit).await;
    assert_eq!(
        std::fs::read_to_string(got.join("src/work.py")).unwrap(),
        "work = 3\n"
    );
    assert_eq!(
        std::fs::read_to_string(got.join("src/h2.py")).unwrap(),
        "h = 2\n"
    );
}
