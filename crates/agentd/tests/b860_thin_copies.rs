//! A copy of the work that leaves out what the bundle the run started from holds.
//!
//! Every copy of a run's work used to be a bundle of the whole history: its size followed the repository, not the change. With the
//! form `base`, which is the form of the release (`AGENTD_DIRECT_COPY_FORM`, tests only, names it or the form `whole`), a copy holds
//! what the run changed since its base, git names the commits it left out in the bundle's header, and a reader takes the base first. Root invariant 9 binds every case
//! here: a copy that cannot be written in that form is written whole, never dropped, and the copy of the end still names the
//! session's final head.
//!
//! The first half drives a [`Snapshotter`] with the host's git. The second half runs whole turns with the fake AWS and the fake
//! engine of `direct_support`.
#![allow(clippy::unwrap_used)]

mod b860_thin_support;
mod common;
mod direct_support;

use agentd::direct::snapshot::{BundleForm, GiveUp, Snapshotter, THIN_HEADER_MAX};
use b860_thin_support::{
    Lab, base_form, lab, later_session, needed, noise, prefix, record_of, restored, stage_base,
    stored_copy, thin, whole_form,
};
use common::{git, python_calc, scratch};
use direct_support::{Direct, Opts, RUN, checkout_of_copy, needs_the_base};
use harness_protocol::bundle_header::{heads, needs};
use harness_protocol::direct::s3 as keys;
use harness_protocol::runtime::TurnOutcome;

// ---- the host's git and a Snapshotter ------------------------------------------------------------------------------------------

#[tokio::test]
async fn b860_copy_bytes_follow_the_change_not_the_history() {
    // A repository with a history: 200 commits, each with 50 KiB that no pack makes smaller.
    let dir = scratch("b860-bytes");
    let repo = python_calc(&dir);
    for n in 0..200u64 {
        std::fs::write(
            repo.join(format!("data-{}.bin", n % 20)),
            noise(n, 50 * 1024),
        )
        .unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", &format!("history {n}")]);
    }
    git(&repo, &["gc", "-q"]);
    let pack_bytes: u64 = std::fs::read_dir(repo.join(".git/objects/pack"))
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".pack"))
        .map(|e| e.metadata().unwrap().len())
        .sum();
    assert!(
        pack_bytes > 8 * 1024 * 1024,
        "the pack is {pack_bytes} bytes"
    );
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let base_bundle = dir.join("repo.bundle");
    stage_base(&repo, &base_bundle);
    let l = Lab {
        dir,
        repo,
        base_bundle,
        base,
    };

    // One file of 1 KiB changes.
    std::fs::write(l.repo.join("src/notes.txt"), noise(7, 1024)).unwrap();

    // The form of every release so far: the copy holds the whole history.
    let data = l.dir.join("whole-data");
    std::fs::create_dir_all(&data).unwrap();
    let mut whole = Snapshotter::new(&data, &l.repo, &l.base).await.unwrap();
    let whole_copy = whole.snapshot().await.unwrap().expect("the tree changed");
    println!(
        "B860 TRIGGER PRESENT: a pack of {pack_bytes} bytes, and a whole copy of one changed kilobyte is {} bytes",
        whole_copy.bundle_bytes
    );
    assert!(whole_copy.bundle_bytes > pack_bytes / 2);
    assert!(needed(&whole_copy.bundle).is_empty());

    // The form `base`: the copy holds the change.
    let mut s = thin(&l, "thin-data").await;
    let copy = s.snapshot().await.unwrap().expect("the tree changed");
    assert!(
        copy.bundle_bytes <= 64 * 1024,
        "the copy of one changed kilobyte is {} bytes; the pack is {pack_bytes} bytes",
        copy.bundle_bytes
    );
    assert_eq!(
        copy.bundle_bytes,
        std::fs::metadata(&copy.bundle).unwrap().len()
    );
    assert!(
        !needed(&copy.bundle).is_empty(),
        "the header names what the copy needs"
    );
    // The base and the copy give the copy's commit, with the changed file in it.
    let got = restored(&l, "bytes", &copy.bundle, &copy.commit).await;
    assert_eq!(
        std::fs::read(got.join("src/notes.txt")).unwrap(),
        noise(7, 1024)
    );
    assert_eq!(
        std::fs::read(got.join("data-3.bin")).unwrap(),
        std::fs::read(l.repo.join("data-3.bin")).unwrap()
    );
}

#[tokio::test]
async fn b860_copy_restores_to_the_same_tree() {
    let l = lab("b860-same-tree");
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let first = s.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(needed(&first.bundle), vec![l.base.clone()]);
    let got = restored(&l, "first", &first.bundle, &first.commit).await;
    // The tree of the copy is the tree of the work as it was.
    git(&l.repo, &["add", "-A"]);
    let work_tree = git(&l.repo, &["write-tree"]);
    assert_eq!(git(&got, &["rev-parse", "HEAD^{tree}"]), work_tree);
    assert_eq!(
        std::fs::read_to_string(got.join("src/a.py")).unwrap(),
        "a = 1\n"
    );

    // The engine commits and goes on: the copy holds the engine's commit and the change after it, and still needs only the base.
    git(&l.repo, &["add", "-A"]);
    git(&l.repo, &["commit", "-q", "-m", "engine: a"]);
    let engine_commit = git(&l.repo, &["rev-parse", "HEAD"]);
    std::fs::write(l.repo.join("src/b.py"), "b = 2\n").unwrap();
    let second = s.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(needed(&second.bundle), vec![l.base.clone()]);
    let got = restored(&l, "second", &second.bundle, &second.commit).await;
    assert_eq!(git(&got, &["rev-parse", "HEAD^"]), engine_commit);
    assert_eq!(
        std::fs::read_to_string(got.join("src/b.py")).unwrap(),
        "b = 2\n"
    );

    // A starter commit of the harness on the base, made before the engine starts: the session's own base is that commit, the floor
    // is still what the base bundle names, and the copy carries the starter commit with it.
    let l2 = lab("b860-same-tree-starter");
    std::fs::write(l2.repo.join("AGENTS.md"), "# starter\n").unwrap();
    git(&l2.repo, &["add", "-A"]);
    git(&l2.repo, &["commit", "-q", "-m", "harness: DOX starter"]);
    let starter = git(&l2.repo, &["rev-parse", "HEAD"]);
    let data = l2.dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut s2 = Snapshotter::new(&data, &l2.repo, &starter).await.unwrap();
    s2.set_form(BundleForm::AboveFloor);
    s2.set_floors(heads(&prefix(&l2.base_bundle)).unwrap());
    std::fs::write(l2.repo.join("src/c.py"), "c = 3\n").unwrap();
    let copy = s2.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(needed(&copy.bundle), vec![l2.base.clone()]);
    let got = restored(&l2, "starter", &copy.bundle, &copy.commit).await;
    assert_eq!(git(&got, &["rev-parse", "HEAD^"]), starter);
    assert!(got.join("AGENTS.md").is_file());

    // A later session whose restore object was that copy: its own copies leave out the base again and restore from it.
    let data = l2.dir.join("data-later");
    std::fs::create_dir_all(&data).unwrap();
    let mut later = Snapshotter::new(&data, &got, &starter).await.unwrap();
    later.set_form(BundleForm::AboveFloor);
    later.set_floors(heads(&prefix(&l2.base_bundle)).unwrap());
    assert!(
        later.snapshot().await.unwrap().is_none(),
        "nothing changed since the copy the session restored"
    );
    std::fs::write(got.join("src/d.py"), "d = 4\n").unwrap();
    let next = later.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(needed(&next.bundle), vec![l2.base.clone()]);
    let again = restored(&l2, "later", &next.bundle, &next.commit).await;
    assert_eq!(
        std::fs::read_to_string(again.join("src/d.py")).unwrap(),
        "d = 4\n"
    );
    assert_eq!(
        std::fs::read_to_string(again.join("src/c.py")).unwrap(),
        "c = 3\n"
    );
}

#[tokio::test]
async fn b860_copy_is_whole_when_the_base_cannot_be_read() {
    // The engine removes `.git`: neither its commits nor the base can be read, and the copy is the whole tree with no parent.
    let l = lab("b860-no-git");
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let before = s.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(needed(&before.bundle), vec![l.base.clone()]);
    std::fs::rename(l.repo.join(".git"), l.dir.join("moved-git")).unwrap();
    std::fs::write(l.repo.join("src/a.py"), "a = 2\n").unwrap();
    let copy = s.snapshot().await.unwrap().expect("the tree changed");
    assert!(
        needed(&copy.bundle).is_empty(),
        "a copy without the engine's repository needs nothing"
    );
    let got = restored(&l, "no-git", &copy.bundle, &copy.commit).await;
    assert_eq!(
        std::fs::read_to_string(got.join("src/a.py")).unwrap(),
        "a = 2\n"
    );
    assert!(got.join("tests/test_calc.py").is_file(), "the whole tree");

    // The engine removes the objects of the base from its store after the session started (a new root, then a prune): the floor
    // cannot be read any more, and the copy is whole.
    let l = lab("b860-base-pruned");
    let mut s = thin(&l, "data").await;
    git(&l.repo, &["checkout", "-q", "--orphan", "fresh"]);
    git(&l.repo, &["commit", "-q", "-m", "a new root"]);
    git(&l.repo, &["branch", "-q", "-D", "main"]);
    git(&l.repo, &["reflog", "expire", "--expire=now", "--all"]);
    git(&l.repo, &["gc", "-q", "--prune=now"]);
    assert!(
        std::process::Command::new("git")
            .current_dir(&l.repo)
            .args(["cat-file", "-e", &l.base])
            .output()
            .unwrap()
            .status
            .code()
            != Some(0),
        "TRIGGER PRESENT: the engine's store no longer holds the base commit"
    );
    std::fs::write(l.repo.join("src/a.py"), "a = 3\n").unwrap();
    let copy = s.snapshot().await.unwrap().expect("the tree changed");
    assert!(
        needed(&copy.bundle).is_empty(),
        "{:?}",
        needed(&copy.bundle)
    );
    let got = restored(&l, "pruned", &copy.bundle, &copy.commit).await;
    assert_eq!(
        std::fs::read_to_string(got.join("src/a.py")).unwrap(),
        "a = 3\n"
    );
}

#[tokio::test]
async fn b860_copy_is_whole_when_its_header_would_be_long() {
    // The commit the copy would name as what it needs has a subject of 10 KiB: the header would not end inside the bytes a reader
    // takes, so the copy is written whole.
    let dir = scratch("b860-long-header");
    let repo = python_calc(&dir);
    std::fs::write(repo.join("src/x.py"), "x = 1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", &"s".repeat(10 * 1024)]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let base_bundle = dir.join("repo.bundle");
    stage_base(&repo, &base_bundle);
    let l = Lab {
        dir,
        repo,
        base_bundle,
        base,
    };
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let copy = s.snapshot().await.unwrap().expect("the tree changed");
    assert!(
        needed(&copy.bundle).is_empty(),
        "a copy whose header would be longer than {THIN_HEADER_MAX} bytes is whole"
    );
    restored(&l, "long", &copy.bundle, &copy.commit).await;
}

#[tokio::test]
async fn b860_end_copy_of_an_unchanged_session_names_the_head() {
    // The final head is the base's own head: the session changed no file and made no commit.
    let l = lab("b860-end-base");
    let mut s = thin(&l, "data").await;
    let made = s.final_copy(Some(&l.base)).await.unwrap();
    println!(
        "B860 TRIGGER PRESENT: the final head {} is a head of the base, so nothing of it is above the floor",
        &l.base[..12]
    );
    assert!(made.final_copy);
    assert_eq!(made.commit, l.base, "the copy of the end is the final head");
    // python_calc has one commit: the head is a root, and the copy of a root holds the whole of it.
    assert!(needed(&made.bundle).is_empty());
    restored(&l, "end-root", &made.bundle, &l.base).await;

    // A base with history. The head is the base's head: the copy holds that one commit and needs its parent, which the base holds.
    let dir = scratch("b860-end-history");
    let repo = python_calc(&dir);
    std::fs::write(repo.join("src/one.py"), "one = 1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let behind = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("src/side.py"), "side = 1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "side"]);
    git(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("src/two.py"), "two = 2\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "two"]);
    git(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let merge = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("src/three.py"), "three = 3\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "three"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let root = git(&repo, &["rev-list", "--max-parents=0", "HEAD"]);
    let base_bundle = dir.join("repo.bundle");
    stage_base(&repo, &base_bundle);
    let l = Lab {
        dir,
        repo,
        base_bundle,
        base,
    };
    for (name, head, parents) in [
        ("head", l.base.clone(), 1),
        ("behind", behind, 1),
        ("merge", merge, 2),
        ("root", root, 0),
    ] {
        let mut s = thin(&l, &format!("data-{name}")).await;
        let made = s
            .final_copy(Some(&head))
            .await
            .unwrap_or_else(|e| panic!("{name}: the copy of the end failed: {e}"));
        assert_eq!(made.commit, head, "{name}");
        assert!(made.final_copy, "{name}");
        // The ref of the bundle names the final head itself.
        assert_eq!(
            harness_protocol::bundle_header::bundle_head(&prefix(&made.bundle)).unwrap(),
            head,
            "{name}"
        );
        assert_eq!(needed(&made.bundle).len(), parents, "{name}");
        // A copy of one commit, not of the history (a root holds its whole tree).
        let whole_bytes = std::fs::metadata(&l.base_bundle).unwrap().len();
        assert!(
            made.bundle_bytes < whole_bytes,
            "{name}: {} bytes; the base is {whole_bytes} bytes",
            made.bundle_bytes
        );
        restored(&l, &format!("end-{name}"), &made.bundle, &head).await;
    }

    // A count that fails is no zero: a floor this repository cannot read makes `git rev-list --count` fail, and the copy is whole.
    let mut s = thin(&l, "data-count").await;
    s.set_floors(vec!["0123456789abcdef0123456789abcdef01234567".to_string()]);
    let made = s.final_copy(Some(&l.base)).await.unwrap();
    assert_eq!(made.commit, l.base);
    assert!(
        needed(&made.bundle).is_empty(),
        "a failed count writes the bundle whole"
    );
    restored(&l, "end-count", &made.bundle, &l.base).await;
    // The same for a copy made while the engine works.
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let mut s = thin(&l, "data-count-look").await;
    s.set_floors(vec!["0123456789abcdef0123456789abcdef01234567".to_string()]);
    let copy = s.snapshot().await.unwrap().expect("the tree changed");
    assert!(needed(&copy.bundle).is_empty());
    restored(&l, "look-count", &copy.bundle, &copy.commit).await;
}

#[tokio::test]
async fn b860_read_back_bundle_stays_whole() {
    // The private repository of a read of a stopped session's storage, told the form `base` and the floors: the bundle it writes
    // still holds the whole history.
    let l = lab("b860-read-back");
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    git(&l.repo, &["add", "-A"]);
    git(&l.repo, &["commit", "-q", "-m", "engine: a"]);
    let start = git(&l.repo, &["rev-parse", "HEAD"]);
    std::fs::write(l.repo.join("src/b.py"), "b = 2\n").unwrap();
    let dir = l.dir.join("read");
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = Snapshotter::for_read_back(&dir, &l.repo, &l.base)
        .await
        .unwrap();
    s.set_form(BundleForm::AboveFloor);
    s.set_floors(heads(&prefix(&l.base_bundle)).unwrap());
    let read = s.read_back(&start, 1, &GiveUp::at(None)).await.unwrap();
    assert!(
        needed(&read.bundle).is_empty(),
        "the read-back bundle clones alone"
    );
    restored(&l, "read-back", &read.bundle, &read.commit).await;
}

#[tokio::test]
async fn b860_read_back_compares_with_a_thin_copy() {
    // A session saved a copy above the floor and was stopped. The read of its storage fetches that copy into a repository that
    // reads the stopped session's own objects, and compares.
    let l = lab("b860-compare");
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let saved = s.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(needed(&saved.bundle), vec![l.base.clone()]);
    let saved_bundle = l.dir.join("saved.bundle");
    std::fs::copy(&saved.bundle, &saved_bundle).unwrap();

    let dir = l.dir.join("read");
    std::fs::create_dir_all(&dir).unwrap();
    let mut read = Snapshotter::for_read_back(&dir, &l.repo, &l.base)
        .await
        .unwrap();
    let back = read.read_back(&l.base, 1, &GiveUp::at(None)).await.unwrap();
    let baseline = read
        .fetch_copy(&saved_bundle)
        .await
        .expect("the saved copy is fetched while the storage holds the base");
    assert_eq!(baseline, saved.commit);
    let compared = read
        .compare(&back.tree, &baseline, 1 << 20, 10)
        .await
        .unwrap();
    assert!(compared.same, "{compared:?}");

    // The engine had removed `.git` before the stop: the saved copy cannot be fetched, so the comparison cannot be made. The tree is
    // still read back, whole.
    std::fs::rename(l.repo.join(".git"), l.dir.join("moved-git")).unwrap();
    let dir = l.dir.join("read-no-git");
    std::fs::create_dir_all(&dir).unwrap();
    let mut read = Snapshotter::for_read_back(&dir, &l.repo, &l.base)
        .await
        .unwrap();
    let back = read.read_back(&l.base, 1, &GiveUp::at(None)).await.unwrap();
    assert!(back.parent.is_none());
    assert!(needed(&back.bundle).is_empty());
    assert!(
        read.fetch_copy(&saved_bundle).await.is_err(),
        "a copy above the floor cannot be fetched without the base"
    );
}

#[tokio::test]
async fn b860_copy_form_whole_is_todays_bundle() {
    // The word of the tests-only variable.
    assert_eq!(BundleForm::from_word(Some("whole")), BundleForm::Whole);
    assert_eq!(BundleForm::from_word(Some("base")), BundleForm::AboveFloor);
    assert_eq!(
        BundleForm::from_word(Some(" base ")),
        BundleForm::AboveFloor
    );
    for other in [
        None,
        Some(""),
        Some("thin"),
        Some("BASE"),
        Some("above-floor"),
    ] {
        assert_eq!(
            BundleForm::from_word(other),
            BundleForm::RELEASE,
            "{other:?}"
        );
    }
    let from = |word: Option<&'static str>| {
        agentd::config::DirectConfig::from_vars(&|key: &str| {
            (key == "AGENTD_DIRECT_COPY_FORM")
                .then(|| word.map(str::to_string))
                .flatten()
        })
        .copy_form
    };
    assert_eq!(from(Some("whole")), BundleForm::Whole);
    assert_eq!(from(Some("base")), BundleForm::AboveFloor);
    assert_eq!(from(None), BundleForm::RELEASE);
    assert_eq!(from(Some("something")), BundleForm::RELEASE);

    // The form `whole` with floors set, and the form `base` without a floor: the bundle of every release so far.
    let l = lab("b860-whole");
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    for (name, form, floors) in [
        ("whole-with-floors", BundleForm::Whole, true),
        ("base-without-floors", BundleForm::AboveFloor, false),
    ] {
        let data = l.dir.join(name);
        std::fs::create_dir_all(&data).unwrap();
        let mut s = Snapshotter::new(&data, &l.repo, &l.base).await.unwrap();
        s.set_form(form);
        if floors {
            s.set_floors(heads(&prefix(&l.base_bundle)).unwrap());
        }
        let copy = s.snapshot().await.unwrap().expect("the tree changed");
        assert!(needed(&copy.bundle).is_empty(), "{name}");
        // It clones alone, as the client's line says.
        let clone = l.dir.join(format!("clone-{name}"));
        git(
            &l.dir,
            &[
                "clone",
                "-q",
                "-b",
                "snapshot",
                copy.bundle.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), copy.commit, "{name}");
        let end = s.final_copy(None).await.unwrap();
        assert!(needed(&end.bundle).is_empty(), "{name}");
    }
}

// ---- whole turns -----------------------------------------------------------------------------------------------------------------

/// What agentd logged in this turn, one line each.
fn log_text(d: &Direct) -> String {
    d.log.lines().join("\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn b860_end_copy_is_thin_and_final() {
    // The copies while the engine works are off: the one copy is the copy of the end, of a session whose engine changed files.
    let d = Direct::start(base_form(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    }))
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let head = resp.last_commit_sha.clone().unwrap();
    let r = record_of(&d, 1);
    assert_eq!(r["final"], true, "{r}");
    assert_eq!(
        r["headSha"],
        head.as_str(),
        "the copy of the end is the final head"
    );
    let copy = stored_copy(&d, 1);
    assert!(
        needs_the_base(&copy),
        "the copy of the end leaves out what the base holds"
    );
    assert_eq!(
        harness_protocol::bundle_header::bundle_head(&copy[..copy.len().min(8192)]).unwrap(),
        head
    );
    // The base and the copy give the final head with the engine's work in it.
    let clone = checkout_of_copy(&d.aws, &d.dir, &keys::snapshot_bundle_key(RUN), "end");
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
    assert!(
        std::fs::read_to_string(clone.join("src/calc.py"))
            .unwrap()
            .contains("percent_change")
    );
    // The session's own bundle still holds the whole history.
    let own = d
        .aws
        .get(&keys::bundle_key(RUN))
        .expect("the session's bundle");
    assert!(!needs_the_base(&own), "the session's own bundle is whole");
    // The header of the base came from the file the session restored: the bucket was asked for the base once, whole.
    assert_eq!(d.aws.gets_of(&keys::repo_bundle_key(RUN)), vec![None]);
    assert!(
        !log_text(&d).contains("copies are whole in this session"),
        "{}",
        log_text(&d)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b860_end_copy_of_an_unchanged_session_is_stored() {
    // The engine ends its turn without changing a file, and the harness makes no starter commit: the final head is the base's head.
    let d = Direct::start(base_form(Opts {
        scenario: "end_turn_done",
        snapshot_every_s: Some(0),
        ..Opts::default()
    }))
    .await;
    let (_, resp) = d.invoke().await;
    let s = d.summary();
    let head = s.git.head_sha.clone().expect("the session has a head");
    println!(
        "B860 TRIGGER PRESENT: the session's final head {} is its base {:?}",
        &head[..12],
        s.git.base_sha
    );
    assert_eq!(s.git.base_sha.as_deref(), Some(head.as_str()), "{resp:?}");
    let r = record_of(&d, 1);
    assert_eq!(r["final"], true, "{r}");
    assert_eq!(r["headSha"], head.as_str(), "{r}");
    assert!(
        !log_text(&d).contains("final copy of the head failed"),
        "{}",
        log_text(&d)
    );
    // The final head is a head of the base object itself: nothing of it is above the floor.
    let base_object = d.aws.get(&keys::repo_bundle_key(RUN)).unwrap();
    assert!(
        heads(&base_object[..base_object.len().min(65_536)])
            .unwrap()
            .contains(&head),
        "TRIGGER PRESENT: the final head is a head of the base object"
    );
    assert_eq!(s.snapshots.failures, 0, "{:?}", s.snapshots);
    let clone = checkout_of_copy(&d.aws, &d.dir, &keys::snapshot_bundle_key(RUN), "unchanged");
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&clone, &["status", "--porcelain"]), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn b860_floors_come_from_the_base_object() {
    // Session 1 restores the base itself: its header is read from that file, and the bucket is asked for the base once, whole.
    let d1 = Direct::start(base_form(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    }))
    .await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d1.aws.gets_of(&keys::repo_bundle_key(RUN)), vec![None]);
    let s = d1.summary();
    let head = s.git.head_sha.clone().unwrap();
    let base = s.git.base_sha.clone().unwrap();
    let own = d1.aws.get(&keys::bundle_key(RUN)).unwrap();

    // A later session restores the earlier session's own bundle, which is whole: the base is not downloaded, and its header is asked
    // for with one ranged read of at most 65,536 bytes.
    let d2 = later_session(
        base_form(Opts::default()),
        &d1,
        &keys::bundle_key_t(RUN, 1),
        own.clone(),
        &head,
        &base,
    )
    .await;
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        d2.aws.gets_of(&keys::repo_bundle_key(RUN)),
        vec![Some("bytes=0-65535".to_string())]
    );
    assert!(
        needs_the_base(&stored_copy(&d2, 2)),
        "the copies of the later session leave out what the base holds"
    );
    let clone = checkout_of_copy(
        &d2.aws,
        &d2.dir,
        &keys::snapshot_bundle_key_t(RUN, 2),
        "later",
    );
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD"]),
        record_of(&d2, 2)["headSha"].as_str().unwrap()
    );

    // No floor, for three reasons; each time the copies of the session are whole and the log says why, once.
    let thin_header = {
        let mut b =
            format!("# v2 git bundle\n-{base} below\n{head} HEAD\n{head} refs/heads/main\n\nPACK")
                .into_bytes();
        b.extend(std::iter::repeat_n(0u8, 64));
        b
    };
    let cut_header = format!("# v2 git bundle\n{head} HEAD\n{head} refs/heads/main\n").into_bytes();
    for (name, base_object, failing, why) in [
        (
            "needs",
            Some(thin_header),
            false,
            "needs commits it does not hold",
        ),
        (
            "cut",
            Some(cut_header),
            false,
            "does not end within the bytes read",
        ),
        ("down", None, true, "could not be read"),
    ] {
        let d = later_session(
            base_form(Opts::default()),
            &d1,
            &keys::bundle_key_t(RUN, 1),
            own.clone(),
            &head,
            &base,
        )
        .await;
        if let Some(bytes) = base_object {
            d.aws.put(&keys::repo_bundle_key(RUN), bytes);
        }
        if failing {
            d.aws.fail_gets("input/repo.bundle", u32::MAX);
        }
        let (_, resp) = d.invoke().await;
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{name}: {resp:?}");
        assert!(
            !needs_the_base(&stored_copy(&d, 2)),
            "{name}: the copies are whole"
        );
        let log = log_text(&d);
        let said: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("direct turn copies are whole in this session: "))
            .collect();
        assert_eq!(said.len(), 1, "{name}: {log}");
        assert!(said[0].contains(why), "{name}: {}", said[0]);
        let asked = d.aws.gets_of(&keys::repo_bundle_key(RUN));
        assert_eq!(
            asked.len(),
            if failing { 3 } else { 1 },
            "{name}: {asked:?}"
        );
        assert!(
            asked.iter().all(|r| r.as_deref() == Some("bytes=0-65535")),
            "{name}: {asked:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b860_copy_in_a_reused_checkout_restores_from_the_base() {
    // Two invocations of turn 1 on one machine. The first ends with its turn commit, which the base object does not hold; the
    // second finds that checkout and works in it.
    let d = Direct::start(base_form(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    }))
    .await;
    let deps = agentd::direct::Deps::default();
    let first = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(first.outcome, TurnOutcome::Completed, "{first:?}");
    let first_head = first.last_commit_sha.clone().unwrap();
    let base_object = d.aws.get(&keys::repo_bundle_key(RUN)).unwrap();
    let base_heads = heads(&base_object[..base_object.len().min(65_536)]).unwrap();
    assert!(
        !base_heads.contains(&first_head),
        "TRIGGER PRESENT: the checkout the second invocation finds stands at a commit the base object does not hold"
    );
    let second = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(
        second.stop_reason.as_deref(),
        Some("end_turn"),
        "the second invocation ran its engine: {second:?}"
    );
    // The copy the second invocation stored needs commits of the base object, never the first invocation's commit, and the base
    // and the copy give its commit.
    assert!(
        d.aws.versions_of(&keys::snapshot_bundle_key(RUN)).len() >= 2,
        "TRIGGER PRESENT: the second invocation stored a copy of its own"
    );
    let copy = stored_copy(&d, 1);
    let named = needs(&copy[..copy.len().min(65_536)]).unwrap();
    assert!(!named.is_empty(), "the copy leaves out what the base holds");
    assert!(
        !named.contains(&first_head),
        "the copy needs a commit the base object does not hold: {named:?}"
    );
    let clone = checkout_of_copy(&d.aws, &d.dir, &keys::snapshot_bundle_key(RUN), "reused");
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD"]),
        record_of(&d, 1)["headSha"].as_str().unwrap()
    );
    git(&clone, &["fsck", "--no-progress", "--connectivity-only"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn b860_lost_session_goes_on_from_a_thin_copy() {
    // Session 1 saves copies above the floor and is lost: what the service hands to session 2 is the newest saved copy.
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
        "TRIGGER PRESENT: the saved copy needs the base"
    );
    let r1 = record_of(&d1, 1);
    let head = r1["headSha"].as_str().unwrap().to_string();
    let base = d1.summary().git.base_sha.clone().unwrap();

    let d2 = later_session(
        base_form(Opts::default()),
        &d1,
        &keys::snapshot_bundle_key_t(RUN, 1),
        saved,
        &head,
        &base,
    )
    .await;
    d2.aws.put(
        &keys::snapshot_json_key_t(RUN, 1),
        serde_json::to_vec(&r1).unwrap(),
    );
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    // The session restored the copy with the base first: the bucket was asked for the base once, whole, and its header came from
    // that file.
    assert_eq!(d2.aws.gets_of(&keys::repo_bundle_key(RUN)), vec![None]);
    // It went on counting, and its own copy needs the base and restores from it.
    let r2 = record_of(&d2, 2);
    assert_eq!(
        r2["count"].as_u64().unwrap(),
        r1["count"].as_u64().unwrap() + 1,
        "{r2}"
    );
    assert!(needs_the_base(&stored_copy(&d2, 2)));
    let clone = checkout_of_copy(
        &d2.aws,
        &d2.dir,
        &keys::snapshot_bundle_key_t(RUN, 2),
        "next",
    );
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD"]),
        r2["headSha"].as_str().unwrap()
    );
    assert!(
        std::fs::read_to_string(clone.join("src/calc.py"))
            .unwrap()
            .contains("percent_change")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b860_default_copies_are_thin() {
    // The form of the release, with nothing set: a copy leaves out what the run's base holds.
    assert_eq!(BundleForm::RELEASE, BundleForm::AboveFloor);
    assert_eq!(BundleForm::from_word(None), BundleForm::AboveFloor);
    let form_of = |word: Option<&'static str>| {
        agentd::config::DirectConfig::from_vars(&|key: &str| {
            (key == "AGENTD_DIRECT_COPY_FORM")
                .then(|| word.map(str::to_string))
                .flatten()
        })
        .copy_form
    };
    assert_eq!(form_of(None), BundleForm::AboveFloor);
    assert_eq!(form_of(Some("base")), BundleForm::AboveFloor);
    assert_eq!(form_of(Some("whole")), BundleForm::Whole);

    // A whole turn with nothing set: the stored copy needs the base, and the base and the copy give the session's final head.
    let d = Direct::start(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let head = resp.last_commit_sha.clone().unwrap();
    assert!(
        needs_the_base(&stored_copy(&d, 1)),
        "the copy of a session with no setting leaves out what the run's base holds"
    );
    assert_eq!(record_of(&d, 1)["headSha"], head.as_str());
    let clone = checkout_of_copy(&d.aws, &d.dir, &keys::snapshot_bundle_key(RUN), "default");
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
    git(&clone, &["fsck", "--no-progress", "--connectivity-only"]);
    // The session's own bundle holds the whole history, as it always did.
    assert!(!needs_the_base(
        &d.aws
            .get(&keys::bundle_key(RUN))
            .expect("the session's bundle")
    ));

    // The form `whole` still writes the bundle of every earlier release: it needs nothing and clones alone, and the session asks
    // for nothing of the base beyond its restore.
    let d = Direct::start(whole_form(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    }))
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let copy = stored_copy(&d, 1);
    assert!(!needs_the_base(&copy));
    let file = d.dir.join("whole.bundle");
    std::fs::write(&file, &copy).unwrap();
    let clone = d.dir.join("whole-clone");
    git(
        &d.dir,
        &[
            "clone",
            "-q",
            "-b",
            "snapshot",
            file.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD"]),
        resp.last_commit_sha.clone().unwrap()
    );
    assert_eq!(d.aws.gets_of(&keys::repo_bundle_key(RUN)), vec![None]);
}
