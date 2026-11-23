//! The git processes and the files of the engine's store that a keep of the run's commits costs and saves, and what a keep
//! that fails, is starved of disk, is disturbed by the engine or is cut leaves behind.
//!
//! Two counts here are the gate of every build for the bound (the time a look costs a long command is measured on the
//! bench and in the live proof, never by a build): the git processes a look and a keep start, and the files of the engine's object
//! store that a look opens. The other tests steer the keep's own `pack-objects` with a stand-in for git that leads the search path
//! of a second process (`b860_kept_support::second_process`): they latch on the call agentd makes in the private repository of the
//! copies, never on the `pack-objects` that `git bundle create` starts by itself, which does not go through the search path.
//!
//! Root invariant 9 binds every case: a keep that fails, is cut or meets a full disk is a miss that costs one try, leaves no file
//! behind and changes no copy.
#![allow(clippy::unwrap_used)]

mod b860_kept_support;
mod b860_thin_support;
mod common;
mod direct_support;

use agentd::direct::snapshot::{Keep, Snapshotter, TidyMarks};
use b860_kept_support::{
    alive, changed_look, command_of, edit, engine_commits, git_calls, incoming_folder, is_the_keep,
    keeps_started, kept_folder, kept_markers, limit, look, names, private, second_process, work_of,
};
use b860_thin_support::{lab, noise, restored, thin};
use common::git;
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, Reader, WatchFlags};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---- what a look opens in the engine's store --------------------------------------------------------------------------------------

/// Counts the files of an engine's object store that are opened: an inotify watch on `.git/objects` and on every folder in it.
struct Opens(OwnedFd);

impl Opens {
    fn watch(repo: &Path) -> Self {
        let fd = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)
            .expect("an inotify instance for the count of opened files");
        let objects = repo.join(".git/objects");
        let mut folders = vec![objects.clone()];
        folders.extend(
            std::fs::read_dir(&objects)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir()),
        );
        for folder in &folders {
            inotify::add_watch(&fd, folder, WatchFlags::OPEN)
                .expect("a watch on a folder of the engine's object store");
        }
        Self(fd)
    }

    /// The files opened since the watch was set or last counted; a folder that was opened is not counted.
    fn count(&mut self) -> usize {
        let mut buf = [MaybeUninit::<u8>::uninit(); 64 * 1024];
        let mut reader = Reader::new(&self.0, &mut buf);
        let mut files = 0;
        loop {
            match reader.next() {
                Ok(event) => {
                    let flags = event.events();
                    assert!(
                        !flags.contains(ReadFlags::QUEUE_OVERFLOW),
                        "the count of opened files overflowed its queue"
                    );
                    if flags.contains(ReadFlags::OPEN) && !flags.contains(ReadFlags::ISDIR) {
                        files += 1;
                    }
                }
                Err(e) if e == rustix::io::Errno::AGAIN => return files,
                Err(e) => panic!("the count of opened files: {e}"),
            }
        }
    }
}

/// The bound of every build, as a count: after 200 loose commits of the run and a keep, a look opens no more files of the engine's
/// object store than the same look on a repository whose run made no commit, plus 16. The release's look, made for the same state
/// in a private repository without a kept pack, opens every object of the run there, which shows what the count is about.
#[tokio::test]
async fn b860_kept_later_looks_do_not_open_the_engines_objects() {
    // The reference: the third look of a session whose run made no commit.
    let plain = lab("b860-kept-opens-plain");
    let mut copies = thin(&plain, "data").await;
    changed_look(&mut copies, &plain.repo, 1).await;
    changed_look(&mut copies, &plain.repo, 2).await;
    let mut watch = Opens::watch(&plain.repo);
    changed_look(&mut copies, &plain.repo, 3).await;
    let reference = watch.count();

    let l = lab("b860-kept-opens");
    engine_commits(&l.repo, 0, 200);
    let mut kept = thin(&l, "data-kept").await;
    let mut release = thin(&l, "data-release").await;
    release.set_keep_commits(false);
    for round in 1..=2 {
        changed_look(&mut kept, &l.repo, round).await;
        look(&mut release).await;
    }
    let keep = kept.keep_commits(&limit()).await;
    let mut watch = Opens::watch(&l.repo);
    edit(&l.repo, 3);
    look(&mut release).await;
    let released = watch.count();
    look(&mut kept).await;
    let opened = watch.count();
    println!(
        "files of the engine's object store opened by one look: {opened} beside the kept pack, {released} in the release, \
         {reference} on a repository without commits of the run"
    );
    assert!(
        released > reference + 16 + 200,
        "TRIGGER PRESENT is not shown: the release's look opened {released} files of the engine's object store, and the same \
         look on a repository without commits of the run {reference}"
    );
    assert!(
        opened <= reference + 16,
        "TRIGGER PRESENT: the look opened {opened} files of the engine's object store; the same look on a repository without \
         commits of the run opens {reference}, and the release's look {released}"
    );
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "a pack was kept: {keep:?}"
    );
}

/// Ruling 23 (d) of the lead: git's check of a kept pack at a worker's start reads the pack on the worker's own disk and nothing
/// in the engine's store on the session's storage. After 200 loose commits of the run and a keep, a worker that starts on the
/// kept pack opens no more files of the engine's object store than a worker that starts on a private repository of the same looks
/// that holds no kept pack. A check that is given the repository's own objects compares every object of the pack with the
/// engine's copy of it, and fails here.
#[tokio::test]
async fn b860_kept_a_start_on_a_kept_pack_opens_no_more_of_the_engines_store() {
    let l = lab("b860-kept-start-opens");
    engine_commits(&l.repo, 0, 200);
    {
        let mut kept = thin(&l, "data-kept").await;
        let mut release = thin(&l, "data-release").await;
        release.set_keep_commits(false);
        for round in 1..=2 {
            changed_look(&mut kept, &l.repo, round).await;
            look(&mut release).await;
        }
        let keep = kept.keep_commits(&limit()).await;
        assert!(
            matches!(keep, Keep::Kept { .. }),
            "TRIGGER PRESENT is not shown: no pack was kept: {keep:?}"
        );
    }
    let mut watch = Opens::watch(&l.repo);
    drop(thin(&l, "data-release").await);
    let without = watch.count();
    let started = thin(&l, "data-kept").await;
    let with = watch.count();
    println!(
        "files of the engine's object store opened by the start of a worker: {with} on a kept pack, {without} without one"
    );
    assert!(
        started.kept_bytes() > 0,
        "TRIGGER PRESENT is not shown: the worker did not adopt the kept pack"
    );
    assert!(
        with <= without,
        "TRIGGER PRESENT: the start of a worker on a kept pack opened {with} files of the engine's object store; a start \
         without a kept pack opens {without}"
    );
}

// ---- the git processes of a look and of a keep -------------------------------------------------------------------------------------

/// The most git processes a look that makes a copy above the run's base may start, as in release 5.25
/// (`b860_a_look_above_the_base_stays_inside_its_budget`).
const LOOK_GITS: usize = 10;
/// The git commands such a look starts, as in release 5.25.
const LOOK_COMMANDS: [&str; 9] = [
    "add",
    "write-tree",
    "count-objects",
    "rev-parse",
    "cat-file",
    "commit-tree",
    "update-ref",
    "rev-list",
    "bundle",
];
/// The most git processes the copy of the end above the run's base may start, as in release 5.25.
const END_COPY_GITS: usize = 6;

/// The calls the stand-in logged since the last question.
struct Counted {
    dir: PathBuf,
    seen: usize,
}

impl Counted {
    fn since(&mut self) -> Vec<String> {
        let all = git_calls(&self.dir);
        let new = all[self.seen.min(all.len())..].to_vec();
        self.seen = all.len();
        new
    }

    /// One look that makes a copy, held to the git processes of release 5.25: at most ten, each of a known command, none the keep.
    async fn look(&mut self, copies: &mut Snapshotter, repo: &Path, round: u32, what: &str) {
        edit(repo, round);
        self.since();
        look(copies).await;
        let calls = self.since();
        let listed = calls.join("\n");
        for call in &calls {
            assert!(
                LOOK_COMMANDS.contains(&command_of(call)),
                "TRIGGER PRESENT: {what} started git {}, which no look of release 5.25 starts:\n{listed}",
                command_of(call)
            );
            assert!(
                !is_the_keep(call),
                "TRIGGER PRESENT: {what} packed the run's commits inside the look:\n{listed}"
            );
        }
        assert!(
            calls.len() <= LOOK_GITS,
            "TRIGGER PRESENT: {what} started {} git processes; the budget of a look is {LOOK_GITS}:\n{listed}",
            calls.len()
        );
    }

    /// The keep after a look: what came of it, and the git processes it started.
    async fn keep(&mut self, copies: &mut Snapshotter) -> (Keep, Vec<String>) {
        self.since();
        let keep = copies.keep_commits(&limit()).await;
        (keep, self.since())
    }
}

/// The git processes of a look are those of release 5.25 whatever the keep does: at most ten on a moved head, on the second look
/// of a head and on a head at rest after the keep, and none of them packs the run's commits. A keep that is not due starts no git
/// process; the keep after the second look in a row of the same head starts one, `pack-objects`, after the look has ended. The copy
/// of the end starts at most six and no `pack-objects`. A look that is followed by no keep (a look after a tool call, a look for a
/// stop) is the look alone.
#[tokio::test]
async fn b860_kept_a_moved_head_costs_the_releases_look() {
    let Some(dir) = second_process("b860_kept_a_moved_head_costs_the_releases_look") else {
        return;
    };
    let l = lab("b860-kept-cost");
    let mut copies = thin(&l, "data").await;
    engine_commits(&l.repo, 0, 30);
    let mut counted = Counted { dir, seen: 0 };

    counted
        .look(&mut copies, &l.repo, 1, "the first look of a head")
        .await;
    let (keep, calls) = counted.keep(&mut copies).await;
    assert_eq!(
        (keep, calls.len()),
        (Keep::NotDue, 0),
        "a keep that is not due starts no git: {calls:?}"
    );

    counted
        .look(&mut copies, &l.repo, 2, "the second look of the same head")
        .await;
    let (keep, calls) = counted.keep(&mut copies).await;
    assert!(
        calls.len() == 1 && is_the_keep(&calls[0]),
        "TRIGGER PRESENT: the keep after the second look of the same head started {} git processes; it packs the run's commits \
         with one `pack-objects`: {calls:?}",
        calls.len()
    );
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");

    counted
        .look(
            &mut copies,
            &l.repo,
            3,
            "a look of a head at rest after the keep",
        )
        .await;
    let (keep, calls) = counted.keep(&mut copies).await;
    assert_eq!(
        (keep, calls.len()),
        (Keep::NotDue, 0),
        "a head that has its pack: {calls:?}"
    );

    engine_commits(&l.repo, 30, 1);
    counted
        .look(&mut copies, &l.repo, 4, "a look on a moved head")
        .await;
    let (keep, calls) = counted.keep(&mut copies).await;
    assert_eq!(
        (keep, calls.len()),
        (Keep::NotDue, 0),
        "a moved head: {calls:?}"
    );

    // A look after a tool call and a look for a stop are followed by no keep: the look alone, also on a head at rest.
    counted
        .look(&mut copies, &l.repo, 5, "a look that no keep follows")
        .await;

    git(&l.repo, &["add", "-A"]);
    git(&l.repo, &["commit", "-q", "-m", "the work of the session"]);
    let head = git(&l.repo, &["rev-parse", "HEAD"]);
    counted.since();
    let end = copies.final_copy(Some(&head)).await.unwrap();
    let calls = counted.since();
    assert!(end.final_copy);
    assert!(
        calls.len() <= END_COPY_GITS && !calls.iter().any(|c| command_of(c) == "pack-objects"),
        "TRIGGER PRESENT: the copy of the end started {} git processes, or packed the run's commits: {calls:?}",
        calls.len()
    );
}

// ---- a keep that fails, is starved, is disturbed or is cut ------------------------------------------------------------------------

/// A keep that git fails costs one try: the copy of its look was made before it, the miss is said once, five more looks of the
/// same head start no `pack-objects`, and a head that moved and came to rest is tried again.
#[tokio::test]
async fn b860_kept_a_keep_that_fails_costs_one_try() {
    let Some(dir) = second_process("b860_kept_a_keep_that_fails_costs_one_try") else {
        return;
    };
    let l = lab("b860-kept-fails");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 10);
    changed_look(&mut copies, &l.repo, 1).await;
    std::fs::write(dir.join("keep-fail"), "").unwrap();
    let made = changed_look(&mut copies, &l.repo, 2).await;
    let keep = copies.keep_commits(&limit()).await;
    assert_eq!(
        keeps_started(&dir),
        1,
        "TRIGGER PRESENT is not shown: the keep's own pack-objects was not started"
    );
    assert!(
        matches!(&keep, Keep::Missed(why) if why.contains("pack-objects")),
        "TRIGGER PRESENT: a keep that git failed is a miss that names the call: {keep:?}"
    );
    assert_eq!(kept_markers(&git_dir), vec![]);
    assert_eq!(names(&incoming_folder(&l, "data")), Vec::<String>::new());
    // The copy of that look was made before the keep, and restores.
    let wt = restored(&l, "before-the-miss", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(2)
    );

    for round in 3..=7 {
        let made = changed_look(&mut copies, &l.repo, round).await;
        assert_eq!(
            copies.keep_commits(&limit()).await,
            Keep::NotDue,
            "look {round} of the head whose keep failed"
        );
        restored(
            &l,
            &format!("after-the-miss-{round}"),
            &made.bundle,
            &made.commit,
        )
        .await;
    }
    assert_eq!(
        keeps_started(&dir),
        1,
        "TRIGGER PRESENT: the keep that failed was tried again for the same head"
    );

    // The head moves and comes to rest: the keep is tried again, and this time git does not fail it.
    std::fs::rename(dir.join("keep-fail"), dir.join("keep-failed")).unwrap();
    let head = engine_commits(&l.repo, 10, 1);
    changed_look(&mut copies, &l.repo, 8).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "a moved head"
    );
    changed_look(&mut copies, &l.repo, 9).await;
    assert!(matches!(
        copies.keep_commits(&limit()).await,
        Keep::Kept { .. }
    ));
    assert_eq!(keeps_started(&dir), 2);
    assert_eq!(
        kept_markers(&git_dir)
            .iter()
            .map(|(_, h)| h.clone())
            .collect::<Vec<_>>(),
        vec![head]
    );
}

/// One commit of the engine with a file of `kib` KiB that no pack makes smaller.
fn big_commit(repo: &Path, round: u32, kib: usize) -> String {
    std::fs::create_dir_all(repo.join("run")).unwrap();
    std::fs::write(
        repo.join(format!("run/big{round:04}.bin")),
        noise(u64::from(round), kib * 1024),
    )
    .unwrap();
    git(repo, &["add", "run"]);
    git(repo, &["commit", "-q", "-m", &format!("big {round}")]);
    git(repo, &["rev-parse", "HEAD"])
}

/// Ruling 11 of the lead: the disk. A keep whose write fails part-way, as on a disk that fills in the middle of it, is a miss that
/// leaves no file behind, and the copies after it are made and restore. No keep is started while the disk of the private repository
/// has less free than the mark of the clean-up and twice the last bundle. And what the keep holds is one pack: after a head moved
/// and came to rest the folder holds the new pack alone, and the folder a keep writes into is empty.
#[tokio::test]
async fn b860_kept_a_disk_that_fills_in_mid_write_leaves_nothing_behind() {
    let Some(dir) =
        second_process("b860_kept_a_disk_that_fills_in_mid_write_leaves_nothing_behind")
    else {
        return;
    };
    let l = lab("b860-kept-disk");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    for round in 0..3 {
        big_commit(&l.repo, round, 150);
    }
    changed_look(&mut copies, &l.repo, 1).await;
    // The write of the pack fails once it has written a few dozen KiB of its 450.
    std::fs::write(dir.join("keep-short-disk"), "").unwrap();
    changed_look(&mut copies, &l.repo, 2).await;
    let keep = copies.keep_commits(&limit()).await;
    assert_eq!(
        keeps_started(&dir),
        1,
        "TRIGGER PRESENT is not shown: the keep's own pack-objects was not started"
    );
    assert!(
        matches!(keep, Keep::Missed(_)),
        "TRIGGER PRESENT: a keep whose write failed part-way is a miss: {keep:?}"
    );
    assert_eq!(
        (
            names(&incoming_folder(&l, "data")),
            names(&kept_folder(&l, "data"))
        ),
        (Vec::<String>::new(), Vec::<String>::new()),
        "TRIGGER PRESENT: the keep that met a full disk left files behind"
    );
    assert_eq!(kept_markers(&git_dir), vec![]);
    std::fs::rename(
        dir.join("keep-short-disk"),
        dir.join("keep-short-disk-over"),
    )
    .unwrap();
    for round in 3..=4 {
        let made = changed_look(&mut copies, &l.repo, round).await;
        assert_eq!(
            copies.keep_commits(&limit()).await,
            Keep::NotDue,
            "one try for this head"
        );
        let wt = restored(
            &l,
            &format!("after-the-full-disk-{round}"),
            &made.bundle,
            &made.commit,
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(wt.join("work.txt")).unwrap(),
            work_of(round)
        );
    }

    // A disk with less free than the mark of the clean-up and twice the last bundle: no keep is started.
    copies.set_tidy_marks(TidyMarks {
        free_bytes: u64::MAX,
        ..TidyMarks::RUN
    });
    big_commit(&l.repo, 3, 150);
    changed_look(&mut copies, &l.repo, 5).await;
    changed_look(&mut copies, &l.repo, 6).await;
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        matches!(&keep, Keep::Missed(why) if why.contains("free")),
        "a disk without room: {keep:?}"
    );
    assert_eq!(
        keeps_started(&dir),
        1,
        "no pack-objects is started on a disk without room"
    );
    assert_eq!(names(&kept_folder(&l, "data")), Vec::<String>::new());

    // With room again: one pack, and after the head moved and came to rest, the new pack alone.
    copies.set_tidy_marks(TidyMarks::RUN);
    let mut last = String::new();
    for (round, commit) in [(7, 4), (9, 5)] {
        last = big_commit(&l.repo, commit, 150);
        changed_look(&mut copies, &l.repo, round).await;
        let made = changed_look(&mut copies, &l.repo, round + 1).await;
        let keep = copies.keep_commits(&limit()).await;
        assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
        let files = names(&kept_folder(&l, "data"));
        assert_eq!(
            files.iter().filter(|n| n.ends_with(".pack")).count(),
            1,
            "one kept pack on the disk after a keep: {files:?}"
        );
        assert_eq!(names(&incoming_folder(&l, "data")), Vec::<String>::new());
        assert!(
            copies.kept_bytes() <= made.bundle_bytes,
            "the kept pack ({} bytes) is no larger than the bundle of the look ({} bytes)",
            copies.kept_bytes(),
            made.bundle_bytes
        );
    }
    assert_eq!(
        kept_markers(&git_dir)
            .iter()
            .map(|(_, h)| h.clone())
            .collect::<Vec<_>>(),
        vec![last]
    );
}

/// Ruling 9 of the lead: the keep reads the commits it captured, and a read that fails is a miss. The engine rewrites its history
/// and prunes the old one while the keep is built: no pack, no marker, one miss, and the next look makes a copy of the new history
/// that restores. Then the same after a pack is installed: the engine rewrites and prunes under a kept pack, the looks that follow
/// make copies that restore, and the pack of the head that is gone does not stay.
#[tokio::test]
async fn b860_kept_a_history_that_is_rewritten_and_pruned_under_the_keep_is_a_miss() {
    let Some(dir) =
        second_process("b860_kept_a_history_that_is_rewritten_and_pruned_under_the_keep_is_a_miss")
    else {
        return;
    };
    let l = lab("b860-kept-rewrite");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let gone = engine_commits(&l.repo, 0, 10);
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;

    // While the keep is built: the stand-in lets the engine rewrite and prune first, then is the real pack-objects.
    std::fs::write(
        dir.join("disturb.sh"),
        format!(
            "set -e\ncd '{repo}'\ngit reset -q --hard {base}\necho 'written after the history was rewritten' > other.txt\n\
             git add other.txt\ngit commit -q -m rewritten\ngit reflog expire --expire=now --all\ngit gc -q --prune=now\n",
            repo = l.repo.display(),
            base = l.base
        ),
    )
    .unwrap();
    std::fs::write(dir.join("keep-disturb"), "").unwrap();
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        dir.join("keep-disturbed").exists(),
        "TRIGGER PRESENT is not shown: the keep's own pack-objects was not started; {}",
        std::fs::read_to_string(dir.join("disturb.log")).unwrap_or_default()
    );
    let rewritten = git(&l.repo, &["rev-parse", "HEAD"]);
    assert_ne!(rewritten, gone);
    assert!(
        git(&l.repo, &["cat-file", "-t", &rewritten]) == "commit"
            && !std::process::Command::new("git")
                .current_dir(&l.repo)
                .args(["cat-file", "-e", &gone])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success(),
        "TRIGGER PRESENT is not shown: the engine's store still holds the head the keep captured; {}",
        std::fs::read_to_string(dir.join("disturb.log")).unwrap_or_default()
    );
    assert!(
        matches!(keep, Keep::Missed(_)),
        "TRIGGER PRESENT: a keep whose read failed is a miss: {keep:?}"
    );
    assert_eq!(kept_markers(&git_dir), vec![], "no marker");
    assert_eq!(
        (
            names(&incoming_folder(&l, "data")),
            names(&kept_folder(&l, "data"))
        ),
        (Vec::<String>::new(), Vec::<String>::new()),
        "no pack and nothing left behind"
    );
    let made = changed_look(&mut copies, &l.repo, 3).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "a moved head"
    );
    let wt = restored(&l, "rewritten-under-the-keep", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("other.txt")).unwrap(),
        "written after the history was rewritten\n"
    );
    assert_eq!(
        git(&wt, &["rev-parse", "HEAD^"]),
        rewritten,
        "the copy stands on the engine's new head"
    );

    // After a pack is installed: new commits, a keep, then the engine rewrites and prunes again.
    let kept_for = engine_commits(&l.repo, 10, 5);
    changed_look(&mut copies, &l.repo, 4).await;
    changed_look(&mut copies, &l.repo, 5).await;
    let keep = copies.keep_commits(&limit()).await;
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    git(&l.repo, &["reset", "-q", "--hard", &l.base]);
    std::fs::write(l.repo.join("third.txt"), "the third history\n").unwrap();
    git(&l.repo, &["add", "third.txt"]);
    git(&l.repo, &["commit", "-q", "-m", "rewritten again"]);
    git(&l.repo, &["reflog", "expire", "--expire=now", "--all"]);
    git(&l.repo, &["gc", "-q", "--prune=now"]);
    let third = git(&l.repo, &["rev-parse", "HEAD"]);
    for round in 6..=8 {
        let made = changed_look(&mut copies, &l.repo, round).await;
        let keep = copies.keep_commits(&limit()).await;
        assert!(!matches!(keep, Keep::Missed(_)), "look {round}: {keep:?}");
        let wt = restored(
            &l,
            &format!("rewritten-under-the-pack-{round}"),
            &made.bundle,
            &made.commit,
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(wt.join("work.txt")).unwrap(),
            work_of(round)
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("third.txt")).unwrap(),
            "the third history\n"
        );
        assert_eq!(git(&wt, &["rev-parse", "HEAD^"]), third);
    }
    assert!(
        kept_markers(&git_dir)
            .iter()
            .all(|(_, head)| *head != kept_for),
        "the pack of the head that is gone does not stay"
    );
}

/// An engine that packs its own commits while the pack of a keep is written makes that keep a miss: the pack would hold objects
/// that the engine's new pack holds too. No pack and no marker are left, the miss is the one try for this head, and the copy of
/// the next look restores.
#[tokio::test]
async fn b860_kept_an_engine_that_repacks_while_the_pack_is_written_is_a_miss() {
    let Some(dir) =
        second_process("b860_kept_an_engine_that_repacks_while_the_pack_is_written_is_a_miss")
    else {
        return;
    };
    let l = lab("b860-kept-repacks");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 10);
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;
    std::fs::write(
        dir.join("disturb.sh"),
        format!("set -e\ncd '{}'\ngit gc -q\n", l.repo.display()),
    )
    .unwrap();
    std::fs::write(dir.join("keep-disturb"), "").unwrap();
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        dir.join("keep-disturbed").exists(),
        "TRIGGER PRESENT is not shown: the keep's own pack-objects was not started; {}",
        std::fs::read_to_string(dir.join("disturb.log")).unwrap_or_default()
    );
    assert!(
        matches!(&keep, Keep::Missed(why) if why.contains("repacked")),
        "TRIGGER PRESENT: a keep during which the engine repacked is a miss: {keep:?}"
    );
    assert_eq!(kept_markers(&git_dir), vec![]);
    assert_eq!(
        (
            names(&incoming_folder(&l, "data")),
            names(&kept_folder(&l, "data"))
        ),
        (Vec::<String>::new(), Vec::<String>::new()),
        "no pack and nothing left behind"
    );
    let made = changed_look(&mut copies, &l.repo, 3).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "one try for this head"
    );
    let wt = restored(&l, "repacked-under-the-keep", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
}

/// Waits until the stand-in holds the keep's `pack-objects`, then drops the keep: what the copy task does when a look is due, a
/// tool call ends or agentd is asked to end. The answer is the process id of the held call.
async fn cut_the_keep(copies: &mut Snapshotter, dir: &Path) -> u32 {
    let _ = std::fs::remove_file(dir.join("keep-running"));
    std::fs::write(dir.join("keep-hold"), "").unwrap();
    let far = limit();
    let ended = {
        let keep = copies.keep_commits(&far);
        tokio::pin!(keep);
        let began = Instant::now();
        loop {
            tokio::select! {
                out = &mut keep => break Some(out),
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if dir.join("keep-running").exists() {
                        break None;
                    }
                    assert!(began.elapsed() < Duration::from_secs(60), "the keep neither ended nor reached git");
                }
            }
        }
    };
    assert!(
        ended.is_none(),
        "TRIGGER PRESENT is not shown: the keep ended before it could be cut: {ended:?}"
    );
    let pid: u32 = std::fs::read_to_string(dir.join("keep-running"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::fs::remove_file(dir.join("keep-hold")).unwrap();
    pid
}

/// Ruling 14 of the lead, the worst moment inside one worker: a keep that is cut while its `pack-objects` runs. Its git is ended
/// with it, nothing is left in the private repository, and the copy of the next look is whole and restores. A keep that a tool
/// call or a stop cut is tried again at the next look that qualifies; a keep that was cut because the next look was due is not
/// tried again until the head has moved.
#[tokio::test]
async fn b860_kept_a_keep_that_is_cut_leaves_the_next_copy_whole() {
    let Some(dir) = second_process("b860_kept_a_keep_that_is_cut_leaves_the_next_copy_whole")
    else {
        return;
    };
    let l = lab("b860-kept-cut");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 20);
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;

    let pid = cut_the_keep(&mut copies, &dir).await;
    let began = Instant::now();
    while alive(pid) && began.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !alive(pid),
        "TRIGGER PRESENT: the git of a keep that was cut is still running"
    );
    assert_eq!(
        (names(&incoming_folder(&l, "data")), kept_markers(&git_dir)),
        (Vec::<String>::new(), vec![]),
        "a keep that was cut leaves nothing behind"
    );
    assert_eq!(keeps_started(&dir), 1);

    // A tool call or a stop cut it: the copy of the look that follows is whole, and the keep is tried again after that look.
    copies.keep_cut(false);
    let made = changed_look(&mut copies, &l.repo, 3).await;
    let wt = restored(&l, "after-the-cut", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "a keep that a tool call cut is tried again at the next look of the same head: {keep:?}"
    );
    assert_eq!(keeps_started(&dir), 2);

    // The head moves and comes to rest; this keep is cut because the next look is due: one try for this head.
    engine_commits(&l.repo, 20, 1);
    changed_look(&mut copies, &l.repo, 4).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "a moved head"
    );
    changed_look(&mut copies, &l.repo, 5).await;
    cut_the_keep(&mut copies, &dir).await;
    assert_eq!(keeps_started(&dir), 3);
    copies.keep_cut(true);
    for round in 6..=7 {
        let made = changed_look(&mut copies, &l.repo, round).await;
        assert_eq!(
            copies.keep_commits(&limit()).await,
            Keep::NotDue,
            "a keep that the next look cut is not tried again for the same head"
        );
        restored(
            &l,
            &format!("after-the-second-cut-{round}"),
            &made.bundle,
            &made.commit,
        )
        .await;
    }
    assert_eq!(keeps_started(&dir), 3);
    assert_eq!(names(&incoming_folder(&l, "data")), Vec::<String>::new());

    // A head that moved is tried again.
    let head = engine_commits(&l.repo, 21, 1);
    changed_look(&mut copies, &l.repo, 8).await;
    changed_look(&mut copies, &l.repo, 9).await;
    assert!(matches!(
        copies.keep_commits(&limit()).await,
        Keep::Kept { .. }
    ));
    assert_eq!(keeps_started(&dir), 4);
    assert!(kept_markers(&git_dir).iter().any(|(_, h)| *h == head));
}

/// Finding 2 of the review of this branch: the guard that removes what a cut keep wrote (`Incoming` in the worker's
/// `direct/snapshot.rs`) has a test that fails when the guard does nothing. In the test above the keep is cut while the stand-in
/// still holds, before the real git has started: nothing was written yet, and the folder is empty with or without the guard. Here
/// the real `pack-objects` runs to its end and writes its pack, and the stand-in holds the call after that: the keep is cut after
/// its files exist, as a `pack-objects` that the next look ends in mid-write is in a session. Right after the cut the folder the
/// keep writes into is empty, the git is gone, and no pack stands. The copy of the look that follows is whole and restores, and
/// the keep that a tool call cut is tried again and keeps.
#[tokio::test]
async fn b860_kept_a_keep_that_is_cut_after_its_git_wrote_leaves_no_file() {
    let Some(dir) =
        second_process("b860_kept_a_keep_that_is_cut_after_its_git_wrote_leaves_no_file")
    else {
        return;
    };
    let l = lab("b860-kept-cut-late");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 20);
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;

    // The stand-in lets the real pack-objects write its pack and holds the call after it.
    let _ = std::fs::remove_file(dir.join("keep-running"));
    std::fs::write(dir.join("keep-hold-after"), "").unwrap();
    let incoming = incoming_folder(&l, "data");
    let far = limit();
    let written = {
        let keep = copies.keep_commits(&far);
        tokio::pin!(keep);
        let began = Instant::now();
        loop {
            tokio::select! {
                out = &mut keep => panic!("TRIGGER PRESENT is not shown: the keep ended before it could be cut: {out:?}"),
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if dir.join("keep-running").exists() {
                        break names(&incoming);
                    }
                    assert!(began.elapsed() < Duration::from_secs(60), "the keep neither ended nor reached git");
                }
            }
        }
        // The keep is dropped here: what the copy task does when a look is due, a tool call ends or agentd is asked to end.
    };
    let left = names(&incoming);
    assert!(
        written
            .iter()
            .any(|name| name.starts_with("pack-") && name.ends_with(".pack")),
        "TRIGGER PRESENT is not shown: the keep's pack-objects had written no pack when the keep was cut: {written:?}"
    );
    assert_eq!(
        left,
        Vec::<String>::new(),
        "TRIGGER PRESENT: a keep that was cut after its git had written {written:?} left files behind"
    );
    let pid: u32 = std::fs::read_to_string(dir.join("keep-running"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::fs::remove_file(dir.join("keep-hold-after")).unwrap();
    let began = Instant::now();
    while alive(pid) && began.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !alive(pid),
        "TRIGGER PRESENT: the git of a keep that was cut is still running"
    );
    assert_eq!(
        (names(&kept_folder(&l, "data")), kept_markers(&git_dir)),
        (Vec::<String>::new(), vec![]),
        "no pack stands after a keep that was cut"
    );
    assert_eq!(keeps_started(&dir), 1);

    // A tool call or a stop cut it: the copy of the look that follows is whole, and the keep is tried again and keeps.
    copies.keep_cut(false);
    let made = changed_look(&mut copies, &l.repo, 3).await;
    let wt = restored(&l, "after-the-late-cut", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "a keep that a tool call cut is tried again at the next look of the same head: {keep:?}"
    );
    assert_eq!(keeps_started(&dir), 2);
    assert_eq!(names(&incoming), Vec::<String>::new());
    assert_eq!(kept_markers(&git_dir).len(), 1);
}
