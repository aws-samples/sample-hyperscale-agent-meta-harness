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

use agentd::direct::snapshot::{
    GiveUp, Keep, SnapshotMade, Snapshotter, TIDY_FREE_BYTES, TidyMarks, loose_object_path,
};
use b860_kept_support::{
    KeepCall, alive, changed_look, command_of, edit, engine_commits, filters_in, filters_started,
    git_calls, incoming_folder, index_stdin, is_the_bundle_step, is_the_filter, is_the_keep,
    is_the_older_keep, keep_calls, keeps_in, keeps_started, kept_folder, kept_markers, limit, look,
    names, objects_of, older_keeps_in, older_keeps_started, private, second_process, work_of,
};
use b860_thin_support::{lab, noise, restored, thin};
use common::{git, scratch};
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

    /// The same watch on the folders of loose objects alone (`.git/objects/<two hexadecimal characters>`): neither the
    /// pack folder nor `info` is watched.
    fn watch_loose(repo: &Path) -> Self {
        let fd = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)
            .expect("an inotify instance for the count of opened files");
        let folders = std::fs::read_dir(repo.join(".git/objects"))
            .unwrap()
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit()) && e.path().is_dir()
            })
            .map(|e| e.path());
        let mut watched = 0;
        for folder in folders {
            inotify::add_watch(&fd, &folder, WatchFlags::OPEN)
                .expect("a watch on a folder of loose objects of the engine's store");
            watched += 1;
        }
        assert!(
            watched > 0,
            "the engine's store holds folders of loose objects"
        );
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

// ----: the rig's count by role (ruling 49 (b) of the lead) ------------------------------------------------------------------

/// A logged filter call of a keep by the form, as agentd starts it.
fn a_filter_call() -> String {
    "-C /data -c core.hooksPath=/dev/null pack-objects --stdin-packs --non-empty -q /data/snapshot.git/kept/incoming/pack"
        .to_string()
}

/// A logged call of the older keep (release 5.28's), as agentd starts it.
fn an_older_keeps_call() -> String {
    "-C /data -c core.hooksPath=/dev/null pack-objects --revs --unpacked --incremental --non-empty -q \
     /data/snapshot.git/kept/incoming/pack"
        .to_string()
}

/// The logged bundle step of a look.
fn a_bundle_step() -> String {
    "-C /data -c core.hooksPath=/dev/null bundle create -q --version=2 /data/snapshot.git/snapshot.bundle \
     refs/heads/snapshot ^0123456789012345678901234567890123456789"
        .to_string()
}

/// Logged calls of a look that are no keep call and no bundle step.
fn other_calls() -> Vec<String> {
    vec![
        "-C /data -c core.hooksPath=/dev/null add -A".to_string(),
        "-C /data -c core.hooksPath=/dev/null rev-list --count refs/heads/snapshot".to_string(),
        "-C /data -c core.hooksPath=/dev/null index-pack --stdin".to_string(),
    ]
}

/// The three numbers of a written log: filter calls, calls of the older keep, keeps.
fn counted(calls: &[String]) -> (usize, usize, usize) {
    (filters_in(calls), older_keeps_in(calls), keeps_in(calls))
}

/// Test 18 of the build brief: the two exact counters tell the two keep calls apart by the words of the logged call,
/// and a `pack-objects` that names no file of the private repository is neither.
#[test]
fn b936_the_two_counters_tell_the_filter_call_from_the_older_keeps_call() {
    let (filter, older) = (a_filter_call(), an_older_keeps_call());
    assert!(is_the_filter(&filter) && !is_the_older_keep(&filter));
    assert!(is_the_older_keep(&older) && !is_the_filter(&older));
    assert!(
        is_the_keep(&filter) && is_the_keep(&older),
        "a keep call of either role"
    );
    assert!(is_the_bundle_step(&a_bundle_step()));
    // The `pack-objects` that `git bundle create` or the engine starts names no file of the private repository.
    for none in [
        "pack-objects --revs --stdout --thin --delta-base-offset",
        "-C /work/repo pack-objects --stdin-packs --non-empty -q /work/repo/.git/objects/pack/pack",
        "-C /data -c core.hooksPath=/dev/null pack-objects --all --stdout",
        // The option is read as a word of its own, and the file is the last argument.
        "-C /data pack-objects --revs-of /data/snapshot.git/kept/incoming/pack",
        "-C /data/snapshot.git/x pack-objects --revs --stdout",
    ] {
        assert!(
            !is_the_filter(none) && !is_the_older_keep(none),
            "no keep call: {none}"
        );
        assert_eq!(
            counted(&[none.to_string()]),
            (0, 0, 0),
            "counts as none: {none}"
        );
    }
    // The bundle step of another repository, and another git command that names the bundle, are no bundle step of a look.
    assert!(!is_the_bundle_step(
        "-C /data bundle create -q /data/goal.git/snapshot.bundle refs/heads/x"
    ));
    assert!(!is_the_bundle_step(
        "-C /data ls-remote /data/snapshot.git/snapshot.bundle"
    ));
}

/// Test 18 of the build brief, ruling 49 (b) of the lead: `keeps_started` is the number of keeps, defined from the two
/// counters: every filter call, plus every older keep's call that is not the next keep call after a filter call; the older
/// keep's call belongs to the filter call before it only while no bundle step of a look stands between the two.
#[test]
fn b936_the_number_of_keeps_is_defined_from_the_two_counters() {
    let (f, o, b) = (a_filter_call(), an_older_keeps_call(), a_bundle_step());
    let with_noise = |keeps: &[&String]| -> Vec<String> {
        let mut calls = other_calls();
        for call in keeps {
            calls.push((*call).clone());
            calls.extend(other_calls());
        }
        calls
    };
    // The form alone: one filter call, no older keep's call, one keep.
    assert_eq!(counted(&with_noise(&[&f])), (1, 0, 1), "the form alone");
    // Off: no filter call, one older keep's call, one keep.
    assert_eq!(counted(&with_noise(&[&o])), (0, 1, 1), "the form off");
    // A give-way behind the filter: one filter call, then one older keep's call, one keep.
    assert_eq!(
        counted(&with_noise(&[&f, &o])),
        (1, 1, 1),
        "TRIGGER PRESENT: a filter call with the older keep's call behind it is one keep"
    );
    // A filter call, a bundle step of a look, then an older keep's call: two keeps.
    assert_eq!(
        counted(&with_noise(&[&f, &b, &o])),
        (1, 1, 2),
        "a keep by the form and a later keep with the form off, a look between them"
    );
    // Two calls of the older keep in a row: two keeps.
    assert_eq!(
        counted(&with_noise(&[&o, &o])),
        (0, 2, 2),
        "the older keep twice"
    );
    // The filter call and the older keep's call twice in one wait: two keeps, whatever the order.
    assert_eq!(counted(&with_noise(&[&f, &o, &f, &o])), (2, 2, 2));
    assert_eq!(counted(&with_noise(&[&f, &o, &o])), (1, 2, 2));
    assert_eq!(counted(&with_noise(&[&f, &f])), (2, 0, 2));
    assert_eq!(counted(&with_noise(&[&o, &f])), (1, 1, 2));
    // Keeps of several looks: each look's bundle step bounds the keep before it.
    assert_eq!(
        counted(&with_noise(&[&b, &f, &o, &b, &o, &b, &f, &b, &f, &o])),
        (3, 3, 4)
    );
    // No call at all, and a log of looks alone.
    assert_eq!(counted(&[]), (0, 0, 0));
    assert_eq!(counted(&with_noise(&[&b, &b])), (0, 0, 0));
}

// ----: the kept pack made from the look's own bundle (the form) -------------------------------------------------------------

/// The first line of each marker in the folder of the kept pack of the private repository `git_dir`, sorted.
fn marker_lines(git_dir: &Path) -> Vec<String> {
    let folder = git_dir.join("kept/pack");
    let mut lines: Vec<String> = names(&folder)
        .iter()
        .filter(|name| name.ends_with(".keep"))
        .map(|name| {
            std::fs::read_to_string(folder.join(name))
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    lines.sort();
    lines
}

/// The folder the form indexes a look's bundle into.
fn indexed_folder(git_dir: &Path) -> PathBuf {
    git_dir.join("kept/indexed/pack")
}

/// A lab whose base stands in a pack, as the product makes an engine's store: a clone of the bundle a run starts from stands in
/// one pack. The form is on there once the engine's head is a loose object.
fn lab_on_a_packed_base(label: &str) -> b860_thin_support::Lab {
    let l = lab(label);
    git(&l.repo, &["repack", "-a", "-d", "-q"]);
    l
}

/// Test 17 of the build brief (ruling 49 (d) of the lead, the challenge's M3): the twin of the build's gate
/// `b860_kept_a_moved_head_costs_the_releases_look` for a keep by the form, in a lab whose base stands in a pack. The git
/// processes of a look are those of release 5.25, none of them a keep call; a keep that is not due starts none; the keep after
/// the second look of the same head starts exactly two, `index-pack --stdin` and then `pack-objects --stdin-packs`, and no
/// third call to ask for the head: the index of the filtered pack is read by the worker. The copy of the end starts at most six
/// and no `pack-objects`.
#[tokio::test]
async fn b936_a_keep_by_the_form_costs_two_git_calls_and_the_look_stays_the_releases() {
    let Some(dir) = second_process(
        "b936_a_keep_by_the_form_costs_two_git_calls_and_the_look_stays_the_releases",
    ) else {
        return;
    };
    let l = lab_on_a_packed_base("b936-form-cost");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = engine_commits(&l.repo, 0, 30);
    let mut counted = Counted {
        dir: dir.clone(),
        seen: 0,
    };

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
    let commands: Vec<&str> = calls.iter().map(|c| command_of(c)).collect();
    assert_eq!(
        commands,
        ["index-pack", "pack-objects"],
        "TRIGGER PRESENT: a keep by the form starts exactly `index-pack --stdin` and `pack-objects --stdin-packs`: {calls:?}"
    );
    assert!(
        calls[0].split_whitespace().any(|w| w == "--stdin") && is_the_filter(&calls[1]),
        "{calls:?}"
    );
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(
        marker_lines(&git_dir),
        vec![format!("agentd kept bundle {head}")],
        "the marker of a pack that was made from the look's bundle"
    );
    let note = copies.take_form_note().unwrap_or_default();
    assert!(note.starts_with("made index="), "the form's note: {note:?}");

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

/// Test 5 of the build brief, with the roles of ruling 49 (b) for the form alone: a burst is kept by the form, and
/// after one more commit and two looks of the new head the form makes the second keep again. Each marker reads
/// `agentd kept bundle <head>`, each keep is one `index-pack --stdin` and one filter call in that order, no call of the older
/// keep runs, the pack holds the run's commits and the copy's own, and the form's folders are empty after each keep. The bundle
/// reaches git as an open file at the byte after its header.
#[tokio::test]
async fn b936_a_burst_and_the_commit_after_it_are_each_kept_by_the_form() {
    let Some(dir) =
        second_process("b936_a_burst_and_the_commit_after_it_are_each_kept_by_the_form")
    else {
        return;
    };
    let l = lab_on_a_packed_base("b936-form-twice");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let first = engine_commits(&l.repo, 0, 20);
    changed_look(&mut copies, &l.repo, 1).await;
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    assert_eq!(copies.take_form_note(), None, "no keep was due: no note");
    let made = changed_look(&mut copies, &l.repo, 2).await;
    let keep = copies.keep_commits(&limit()).await;
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(
        keep_calls(&dir, &git_dir),
        vec![KeepCall::Index, KeepCall::Filter],
        "TRIGGER PRESENT: the form alone: one indexing, then one filter call, and no call of the older keep"
    );
    assert_eq!(
        (
            filters_started(&dir),
            older_keeps_started(&dir),
            keeps_started(&dir)
        ),
        (1, 0, 1)
    );
    assert_eq!(
        marker_lines(&git_dir),
        vec![format!("agentd kept bundle {first}")]
    );
    let note = copies.take_form_note().unwrap_or_default();
    assert!(note.starts_with("made index="), "the form's note: {note:?}");
    assert_eq!(copies.take_form_note(), None, "the note is taken once");
    assert_eq!(
        (
            names(&indexed_folder(&git_dir)),
            names(&incoming_folder(&l, "data"))
        ),
        (vec![], vec![]),
        "the form's folders are empty after the keep"
    );
    // The pack holds the 20 commits of the run and the copy's own commit, and nothing of the base.
    let marker = kept_folder(&l, "data").join(
        names(&kept_folder(&l, "data"))
            .into_iter()
            .find(|n| n.ends_with(".keep"))
            .unwrap(),
    );
    let held = objects_of(&l.repo, &marker);
    let commits: Vec<&str> = held
        .iter()
        .filter(|(_, kind)| kind == "commit")
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(
        commits.len(),
        21,
        "20 commits of the run and the copy's own: {commits:?}"
    );
    assert!(commits.contains(&first.as_str()) && commits.contains(&made.commit.as_str()));
    assert!(
        !held.iter().any(|(id, _)| *id == l.base),
        "nothing the engine's pack holds is in the kept pack"
    );
    // The bundle reached git as an open file, read from the byte after its header: the pack's first bytes.
    let fed = index_stdin(&dir);
    assert_eq!(fed.len(), 1, "{fed:?}");
    assert!(
        fed[0].0 && fed[0].1.is_some_and(|at| at > 0) && fed[0].2 == b"PACK",
        "TRIGGER PRESENT: the standard input of the indexing is a regular file at the byte after the header: {fed:?}"
    );

    // One more commit, and two looks of the new head: the form makes the second keep again.
    let second = engine_commits(&l.repo, 20, 1);
    changed_look(&mut copies, &l.repo, 3).await;
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    changed_look(&mut copies, &l.repo, 4).await;
    let keep = copies.keep_commits(&limit()).await;
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(
        marker_lines(&git_dir),
        vec![format!("agentd kept bundle {second}")],
        "one pack, of the new head, made by the form"
    );
    let note = copies.take_form_note().unwrap_or_default();
    assert!(note.starts_with("made index="), "the second keep: {note:?}");
    assert_eq!(
        keep_calls(&dir, &git_dir),
        vec![
            KeepCall::Index,
            KeepCall::Filter,
            KeepCall::Index,
            KeepCall::Filter
        ]
    );
    assert_eq!(
        (older_keeps_started(&dir), keeps_started(&dir)),
        (0, 2),
        "no `pack-objects --revs` ran"
    );
    assert_eq!(
        (
            names(&indexed_folder(&git_dir)),
            names(&incoming_folder(&l, "data"))
        ),
        (vec![], vec![])
    );
}

/// Two looks of the engine's head as it stands, each on a changed tree (rounds `round` and `round + 1`): after the second a keep
/// by the rule of the second look is due. The answer is the second look's copy.
async fn two_looks(copies: &mut Snapshotter, repo: &Path, round: u32) -> SnapshotMade {
    changed_look(copies, repo, round).await;
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    changed_look(copies, repo, round + 1).await
}

/// The keep that is due now is made with the form off: release 5.28's keep runs alone (one call of the older keep, no
/// `index-pack --stdin` and no filter call since the `skip`-th call of the repository), the form's note is `off: ` with `why`
/// in it, and the answer is the keep's.
async fn assert_the_form_is_off(
    copies: &mut Snapshotter,
    dir: &Path,
    git_dir: &Path,
    row: &str,
    why: &str,
) -> Keep {
    let skip = b860_kept_support::kept_seen(dir, git_dir).len();
    let keep = copies.keep_commits(&limit()).await;
    assert_eq!(
        b860_kept_support::keep_calls_from(dir, git_dir, skip),
        vec![KeepCall::Older],
        "TRIGGER PRESENT: {row}: the form is off, so no `index-pack --stdin` and no filter call start, and release 5.28's \
         keep runs: {keep:?}"
    );
    let note = copies.take_form_note().unwrap_or_default();
    assert!(
        note.starts_with("off: ") && note.contains(why),
        "{row}: the form's note names the row: {note:?}"
    );
    assert_eq!(
        (
            names(&indexed_folder(git_dir)),
            names(&git_dir.join("kept/incoming"))
        ),
        (vec![], vec![]),
        "{row}: nothing is left in the folders of a keep"
    );
    keep
}

/// Test 10 of the build brief: each row where the form is off, and the form's disk rule. Release 5.28's keep runs in
/// the same wait, no `index-pack --stdin` starts, and the form's `off:` note names the row: a bundle above the form's limit
/// (row a; at the limit the form runs), a look whose bundle is whole or above the head's own parents (row b, both kinds,
/// through the stand-in's `count-fail` and `count-zero`), a session restored from a saved copy (row d: at its first keep and
/// at the keep of a new head after it, ruling 69 (b) of the lead), an engine's store with alternates of
/// its own (row c), a store without a pack (row f), a head that stands in a pack only (row g), an engine's `.git` that is not
/// the repository's own directory (row h), and a disk that has room for the keep's rule but not for the form's.
#[tokio::test]
async fn b936_where_the_form_is_off_the_older_keep_runs_alone() {
    let Some(dir) = second_process("b936_where_the_form_is_off_the_older_keep_runs_alone") else {
        return;
    };
    // Row a: one byte above the limit the form is off; at the limit it runs.
    let l = lab_on_a_packed_base("b936-off-a");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = engine_commits(&l.repo, 0, 5);
    let made = two_looks(&mut copies, &l.repo, 1).await;
    copies.set_keep_form_limit(Some(made.bundle_bytes - 1));
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "row a",
        "the form indexes at most",
    )
    .await;
    assert!(matches!(keep, Keep::Kept { .. }), "row a: {keep:?}");
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    let head = engine_commits(&l.repo, 5, 1);
    let made = two_looks(&mut copies, &l.repo, 3).await;
    copies.set_keep_form_limit(Some(made.bundle_bytes));
    let skip = b860_kept_support::kept_seen(&dir, &git_dir).len();
    let keep = copies.keep_commits(&limit()).await;
    assert!(matches!(keep, Keep::Kept { .. }), "at the limit: {keep:?}");
    assert_eq!(
        b860_kept_support::keep_calls_from(&dir, &git_dir, skip),
        vec![KeepCall::Index, KeepCall::Filter],
        "at the limit the form runs"
    );
    assert_eq!(
        marker_lines(&git_dir),
        vec![format!("agentd kept bundle {head}")]
    );

    // Row b, both kinds: the look before the keep wrote no bundle above the floors. A look whose count of what is above the
    // floors fails writes its bundle whole; a count of 0 makes the look leave out the head's own parents.
    for (steer, row, why) in [
        ("count-fail", "row b, whole", "the look's bundle is whole"),
        (
            "count-zero",
            "row b, above the parents",
            "the look's bundle is above the head's parents",
        ),
    ] {
        let l = lab_on_a_packed_base(&format!("b936-off-b-{steer}"));
        let mut copies = thin(&l, "data").await;
        let git_dir = private(&l, "data");
        let head = engine_commits(&l.repo, 0, 5);
        changed_look(&mut copies, &l.repo, 1).await;
        assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
        std::fs::write(dir.join(steer), "").unwrap();
        let made = changed_look(&mut copies, &l.repo, 2).await;
        std::fs::remove_file(dir.join(steer)).unwrap();
        let needs = b860_thin_support::needed(&made.bundle);
        let wanted = if steer == "count-fail" {
            vec![]
        } else {
            vec![head.clone()]
        };
        assert_eq!(
            needs, wanted,
            "TRIGGER PRESENT is not shown: {row}: what the look's bundle needs (the base is {})",
            l.base
        );
        let keep = assert_the_form_is_off(&mut copies, &dir, &git_dir, row, why).await;
        assert!(matches!(keep, Keep::Kept { .. }), "{row}: {keep:?}");
        assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    }

    // Row d: a session restored from a saved copy.
    let l = lab_on_a_packed_base("b936-off-d");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    copies.set_session_restored(true);
    let head = engine_commits(&l.repo, 0, 5);
    two_looks(&mut copies, &l.repo, 1).await;
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "row d",
        "restored from a saved copy",
    )
    .await;
    assert!(matches!(keep, Keep::Kept { .. }), "row d: {keep:?}");
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    // The form is off for the session, not for its first keep only: the keep of a new head in the same session is
    // release 5.28's too, with the same `off:` line.
    let head = engine_commits(&l.repo, 5, 1);
    two_looks(&mut copies, &l.repo, 3).await;
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "row d, the second keep of the session",
        "restored from a saved copy",
    )
    .await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "row d, the second keep of the session: {keep:?}"
    );
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);

    // Row c: the engine's store has alternates of its own.
    let l = lab_on_a_packed_base("b936-off-c");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 5);
    two_looks(&mut copies, &l.repo, 1).await;
    let elsewhere = l.dir.join("another-store/objects");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(
        l.repo.join(".git/objects/info/alternates"),
        format!("{}\n", elsewhere.display()),
    )
    .unwrap();
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "row c",
        "alternates of its own",
    )
    .await;
    assert!(matches!(keep, Keep::Kept { .. }), "row c: {keep:?}");

    // Row f: the engine's store holds no pack.
    let l = lab("b936-off-f");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 5);
    two_looks(&mut copies, &l.repo, 1).await;
    let keep = assert_the_form_is_off(&mut copies, &dir, &git_dir, "row f", "holds no pack").await;
    assert!(matches!(keep, Keep::Kept { .. }), "row f: {keep:?}");

    // Row g: the head stands in a pack only. Release 5.28's keep finds nothing to pack.
    let l = lab_on_a_packed_base("b936-off-g");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 5);
    git(&l.repo, &["repack", "-a", "-d", "-q"]);
    two_looks(&mut copies, &l.repo, 1).await;
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "row g",
        "the head stands in a pack only",
    )
    .await;
    assert_eq!(
        keep,
        Keep::Nothing,
        "row g: the engine packed every commit of the run"
    );
    assert_eq!(marker_lines(&git_dir), Vec::<String>::new());

    // Row h: the engine's `.git` is not the repository's own directory when the keep asks.
    let l = lab_on_a_packed_base("b936-off-h");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 5);
    two_looks(&mut copies, &l.repo, 1).await;
    copies.set_git_dir_reader(std::sync::Arc::new(|_| false));
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "row h",
        "not the repository's own directory",
    )
    .await;
    assert!(matches!(keep, Keep::Kept { .. }), "row h: {keep:?}");

    // The form's disk rule: room for the keep's rule (the mark, twice the last bundle and one more) and not for the form's
    // (one bundle more than that). One answer of the disk decides both.
    let l = lab_on_a_packed_base("b936-off-disk");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 5);
    let made = two_looks(&mut copies, &l.repo, 1).await;
    let free = TIDY_FREE_BYTES + 3 * made.bundle_bytes + made.bundle_bytes / 2;
    // The second answer would let the form run: a keep that asked the disk twice would take it for the form's rule.
    copies.set_keep_free_answers(vec![free, u64::MAX]);
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "the disk rule",
        "the form runs only above",
    )
    .await;
    assert!(matches!(keep, Keep::Kept { .. }), "the disk rule: {keep:?}");
}

/// The keep that is due now: its answer, the roles of the git calls it started, in order, and the form's note.
async fn keep_now(
    copies: &mut Snapshotter,
    dir: &Path,
    git_dir: &Path,
) -> (Keep, Vec<KeepCall>, String) {
    let skip = b860_kept_support::kept_seen(dir, git_dir).len();
    let keep = copies.keep_commits(&limit()).await;
    (
        keep,
        b860_kept_support::keep_calls_from(dir, git_dir, skip),
        copies.take_form_note().unwrap_or_default(),
    )
}

/// The engine commits once more (round `from` of its commits) and two looks follow (rounds `round` and `round + 1`): a keep is
/// due for the new head, which is a loose object. The answer is that head.
async fn a_new_head_at_rest(
    copies: &mut Snapshotter,
    l: &b860_thin_support::Lab,
    from: u32,
    round: u32,
) -> String {
    let head = engine_commits(&l.repo, from, 1);
    two_looks(copies, &l.repo, round).await;
    head
}

/// Nothing of a keep is left in the private repository `git_dir` but what stands in the folder of the kept pack.
fn assert_the_keeps_folders_are_empty(git_dir: &Path, what: &str) {
    assert_eq!(
        (
            names(&indexed_folder(git_dir)),
            names(&git_dir.join("kept/incoming"))
        ),
        (vec![], vec![]),
        "{what}: the form's folders and the folder a keep writes into are empty"
    );
}

/// Test 3 (a) and (b) of the build brief: a refusal by git that is the form's own. An indexing that ends with a non-zero
/// exit, and a filter that ends with a non-zero exit while the names of the engine's packs stay the same, make the form give
/// way: release 5.28's keep runs in the same wait and its marker stands, the form's folders are empty, the note ends
/// `; off for this session`, and no `index-pack --stdin` starts at any later keep of the worker's session. The mark is in
/// memory only: a worker that starts on the same private repository uses the form again. When release 5.28's keep is missed
/// behind the refusal, the mark stands too.
#[tokio::test]
async fn b936_a_refusal_by_git_gives_way_to_the_older_keep_and_switches_the_form_off_for_the_session()
 {
    let Some(dir) = second_process(
        "b936_a_refusal_by_git_gives_way_to_the_older_keep_and_switches_the_form_off_for_the_session",
    ) else {
        return;
    };
    // The indexing is refused.
    let l = lab_on_a_packed_base("b936-refused-index");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    std::fs::write(dir.join("index-fail"), "").unwrap();
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    std::fs::remove_file(dir.join("index-fail")).unwrap();
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Older],
        "TRIGGER PRESENT: a refused indexing gives way before the filter, and release 5.28's keep runs in the same wait"
    );
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert!(
        note.starts_with("gave way index=")
            && note.contains(" filter=0: git index-pack: ")
            && note.ends_with("; off for this session"),
        "the form's note: {note:?}"
    );
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    assert_the_keeps_folders_are_empty(&git_dir, "a refused indexing");
    // No `index-pack --stdin` at a later keep of the session.
    let head = a_new_head_at_rest(&mut copies, &l, 1, 3).await;
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert_eq!(
        (calls, note.as_str()),
        (
            vec![KeepCall::Older],
            "off: git refused the form earlier in this session"
        ),
        "the session mark: {keep:?}"
    );
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    // The mark is in memory: a worker that starts on the same private repository has none.
    drop(copies);
    let mut copies = thin(&l, "data").await;
    let head = a_new_head_at_rest(&mut copies, &l, 2, 5).await;
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Filter],
        "a worker that starts has no session mark: {note:?}"
    );
    assert_eq!(
        marker_lines(&git_dir),
        vec![format!("agentd kept bundle {head}")]
    );

    // The filter is refused while the names of the engine's packs stay the same.
    let l = lab_on_a_packed_base("b936-refused-filter");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    std::fs::write(dir.join("filter-fail"), "").unwrap();
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    std::fs::remove_file(dir.join("filter-fail")).unwrap();
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Filter, KeepCall::Older],
        "a refused filter gives way behind the filter"
    );
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert!(
        note.starts_with("gave way index=")
            && note.contains(": git pack-objects: ")
            && !note.contains(" filter=0:")
            && note.ends_with("; off for this session"),
        "the form's note: {note:?}"
    );
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    assert_the_keeps_folders_are_empty(&git_dir, "a refused filter");
    a_new_head_at_rest(&mut copies, &l, 1, 3).await;
    let (_, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert_eq!(
        (calls, note.as_str()),
        (
            vec![KeepCall::Older],
            "off: git refused the form earlier in this session"
        )
    );

    // Release 5.28's keep is missed behind the refusal: the session mark stands, and the keep is a missed keep.
    let l = lab_on_a_packed_base("b936-refused-and-missed");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    std::fs::write(dir.join("index-fail"), "").unwrap();
    std::fs::write(dir.join("keep-fail"), "").unwrap();
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    std::fs::remove_file(dir.join("index-fail")).unwrap();
    std::fs::remove_file(dir.join("keep-fail")).unwrap();
    assert!(matches!(keep, Keep::Missed(_)), "{keep:?}");
    assert_eq!(calls, vec![KeepCall::Index, KeepCall::Older]);
    assert!(note.ends_with("; off for this session"), "{note:?}");
    assert_eq!(marker_lines(&git_dir), Vec::<String>::new());
    assert_the_keeps_folders_are_empty(&git_dir, "a refused indexing and a missed keep");
    assert!(
        !copies.keep_is_due(),
        "a missed keep at rest has tried its head"
    );
    changed_look(&mut copies, &l.repo, 50).await;
    assert!(
        !copies.keep_is_due(),
        "TRIGGER PRESENT: the marks behind a refusal are those of a missed keep: one more look of the same head is \
         followed by no keep"
    );
    a_new_head_at_rest(&mut copies, &l, 1, 3).await;
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(
        (calls, note.as_str()),
        (
            vec![KeepCall::Older],
            "off: git refused the form earlier in this session"
        )
    );
}

/// Tests 3 (c), 3 (d) and 4 of the build brief (rulings 42 (c)(i), (ii) and 44 (a) of the lead): what sets no session
/// mark. A form whose indexing is held until the keep's give-up gives way, and release 5.28's keep behind it has a give-up of
/// its own and keeps the pack. Packs of the engine that change around the filter, a filter that git ends with
/// `could not find pack`, a pack file without its index and a file of another shape whose name ends in `.pack` are git's own
/// `gc` in flight: the form gives way for that keep, release 5.28's keep runs, and at the next keep moment the form runs again.
#[tokio::test]
async fn b936_running_out_of_time_and_a_gc_in_flight_set_no_session_mark() {
    let Some(dir) =
        second_process("b936_running_out_of_time_and_a_gc_in_flight_set_no_session_mark")
    else {
        return;
    };
    let l = lab_on_a_packed_base("b936-no-mark");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let form_runs_at_the_next_keep = |calls: Vec<KeepCall>, note: String, after: &str| {
        assert_eq!(
            calls,
            vec![KeepCall::Index, KeepCall::Filter],
            "TRIGGER PRESENT: {after} set no session mark, so the form runs at the next keep moment: {note:?}"
        );
    };

    // Test 4: the keep's give-up comes while the indexing is held. Release 5.28's keep has a give-up of its own.
    let head = a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    std::fs::write(dir.join("check-hold"), "").unwrap();
    let skip = b860_kept_support::kept_seen(&dir, &git_dir).len();
    let short = GiveUp::at(Some(
        tokio::time::Instant::now() + Duration::from_millis(1_500),
    ));
    let keep = copies.keep_commits(&short).await;
    std::fs::remove_file(dir.join("check-hold")).unwrap();
    assert!(
        dir.join("check-running").exists(),
        "TRIGGER PRESENT is not shown: the indexing was not held"
    );
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "TRIGGER PRESENT: the keep behind a form that ran out of time has a give-up of its own: {keep:?}"
    );
    assert_eq!(
        b860_kept_support::keep_calls_from(&dir, &git_dir, skip),
        vec![KeepCall::Index, KeepCall::Older]
    );
    let note = copies.take_form_note().unwrap_or_default();
    assert!(
        note.starts_with("gave way index=")
            && note.contains(" filter=0: git index-pack was given up before it had ended")
            && !note.contains("off for this session"),
        "the form's note: {note:?}"
    );
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    assert_the_keeps_folders_are_empty(&git_dir, "an indexing that was given up");
    a_new_head_at_rest(&mut copies, &l, 1, 3).await;
    let (_, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    form_runs_at_the_next_keep(calls, note, "running out of time");

    // The filter ends with git's words for a named pack that is gone.
    a_new_head_at_rest(&mut copies, &l, 2, 5).await;
    std::fs::write(dir.join("filter-gone"), "").unwrap();
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    std::fs::remove_file(dir.join("filter-gone")).unwrap();
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Filter, KeepCall::Older]
    );
    assert!(
        note.contains("could not find pack") && !note.contains("off for this session"),
        "{note:?}"
    );
    a_new_head_at_rest(&mut copies, &l, 3, 7).await;
    let (_, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    form_runs_at_the_next_keep(calls, note, "`could not find pack`");

    // The names of the engine's packs change around the filter: the engine packs while the filter is held.
    a_new_head_at_rest(&mut copies, &l, 4, 9).await;
    let _ = std::fs::remove_file(dir.join("filter-running"));
    std::fs::write(dir.join("filter-hold"), "").unwrap();
    let skip = b860_kept_support::kept_seen(&dir, &git_dir).len();
    let far = limit();
    let keep = {
        let keep = copies.keep_commits(&far);
        tokio::pin!(keep);
        let began = Instant::now();
        loop {
            tokio::select! {
                out = &mut keep => break out,
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if dir.join("filter-hold").exists() && dir.join("filter-running").exists() {
                        // git's own packing lands a new pack while the filter runs.
                        git(&l.repo, &["repack", "-q"]);
                        std::fs::remove_file(dir.join("filter-hold")).unwrap();
                    }
                    assert!(began.elapsed() < Duration::from_secs(60), "the keep did not end");
                }
            }
        }
    };
    assert_eq!(
        b860_kept_support::keep_calls_from(&dir, &git_dir, skip),
        vec![KeepCall::Index, KeepCall::Filter, KeepCall::Older]
    );
    let note = copies.take_form_note().unwrap_or_default();
    assert!(
        note.contains("the engine's packs changed while the pack was made")
            && !note.contains("off for this session"),
        "{note:?}"
    );
    assert_eq!(
        keep,
        Keep::Nothing,
        "the engine packed every commit of the run meanwhile: release 5.28's keep finds nothing to pack"
    );
    assert_the_keeps_folders_are_empty(&git_dir, "packs that changed around the filter");
    a_new_head_at_rest(&mut copies, &l, 5, 11).await;
    let (_, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    form_runs_at_the_next_keep(calls, note, "packs that changed around the filter");

    // Test 3 (d): a pack file without its index, and a file of another shape whose name ends in `.pack`.
    let pack_folder = l.repo.join(".git/objects/pack");
    for (n, stray) in [
        "pack-0123456789012345678901234567890123456789.pack",
        "tmp_pack_in_flight.pack",
    ]
    .into_iter()
    .enumerate()
    {
        let n = u32::try_from(n).unwrap();
        std::fs::write(pack_folder.join(stray), "no pack").unwrap();
        a_new_head_at_rest(&mut copies, &l, 6 + 2 * n, 13 + 4 * n).await;
        let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
        std::fs::remove_file(pack_folder.join(stray)).unwrap();
        assert_eq!(
            calls,
            vec![KeepCall::Older],
            "TRIGGER PRESENT: {stray}: a name step a does not take starts neither the indexing nor the filter: {note:?}"
        );
        assert!(matches!(keep, Keep::Kept { .. }), "{stray}: {keep:?}");
        assert!(
            note.starts_with("gave way index=0 filter=0: the engine's pack folder holds ")
                && note.contains(stray)
                && !note.contains("off for this session"),
            "{stray}: {note:?}"
        );
        a_new_head_at_rest(&mut copies, &l, 7 + 2 * n, 15 + 4 * n).await;
        let (_, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
        form_runs_at_the_next_keep(calls, note, stray);
    }
}

/// The process id that a held call of the stand-in wrote into `file`, once it is there whole. The stand-in writes it with
/// `echo $$ > file`: the shell makes the file first and writes the number and its line end afterwards, so the file is there
/// and empty for a moment. A file that is not there, an empty one and a text that is no whole line with a number all answer
/// `None`, and the caller looks again.
fn held_pid(file: &Path) -> Option<u32> {
    std::fs::read_to_string(file)
        .ok()?
        .strip_suffix('\n')?
        .parse()
        .ok()
}

/// The helper's question says "not yet" until a whole line with a number stands in the file.
#[test]
fn b936_the_cut_takes_the_held_calls_process_id_only_when_it_is_written_whole() {
    let dir = scratch("b936-held-pid");
    let file = dir.join("check-running");
    assert_eq!(held_pid(&file), None, "no file yet");
    std::fs::write(&file, "").unwrap();
    assert_eq!(
        held_pid(&file),
        None,
        "TRIGGER PRESENT: the file is there and empty, as between the shell's opening of it and its write"
    );
    std::fs::write(&file, "4711").unwrap();
    assert_eq!(
        held_pid(&file),
        None,
        "a number whose line has not ended is not whole yet"
    );
    std::fs::write(&file, "soon\n").unwrap();
    assert_eq!(held_pid(&file), None, "a line that is no number");
    std::fs::write(&file, "4711\n").unwrap();
    assert_eq!(held_pid(&file), Some(4711), "the line the stand-in writes");
}

/// Waits until the stand-in holds a git call of the keep that is due (`hold` is the file that holds it, `running` the file the
/// held call writes) and the held call's process id stands in that file, then drops the keep, as the copy task does when the
/// next look starts. The answer is the process id of the held call.
async fn cut_the_form(copies: &mut Snapshotter, dir: &Path, hold: &str, running: &str) -> u32 {
    let _ = std::fs::remove_file(dir.join(running));
    std::fs::write(dir.join(hold), "").unwrap();
    let far = limit();
    let ended = {
        let keep = copies.keep_commits(&far);
        tokio::pin!(keep);
        let began = Instant::now();
        loop {
            tokio::select! {
                out = &mut keep => break Some(out),
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if held_pid(&dir.join(running)).is_some() {
                        break None;
                    }
                    assert!(began.elapsed() < Duration::from_secs(60), "the keep neither ended nor reached {hold}");
                }
            }
        }
    };
    assert!(
        ended.is_none(),
        "TRIGGER PRESENT is not shown: the keep ended before it could be cut: {ended:?}"
    );
    let pid = held_pid(&dir.join(running))
        .expect("the held call's process id stood in its file a moment ago");
    std::fs::remove_file(dir.join(hold)).unwrap();
    pid
}

/// Test 11 of the build brief, the lab's part (rulings 42 (c)(i) and 44 (f) of the lead): a form that is cut while
/// `index-pack` runs, and one that is cut while its filter runs, leave nothing: git's group is gone, the form's folders and
/// the folder a keep writes into are empty, no pack and no marker stands, no form note is left, and the next look makes its
/// copy. The form starts after the keep noted the head as being kept: a cut by a tool call's look gives the head its try
/// back, and a cut by the timer's look leaves the head tried.
#[tokio::test]
async fn b936_a_form_that_is_cut_leaves_nothing_and_keeps_the_rule_of_the_cut() {
    let Some(dir) =
        second_process("b936_a_form_that_is_cut_leaves_nothing_and_keeps_the_rule_of_the_cut")
    else {
        return;
    };
    let l = lab_on_a_packed_base("b936-form-cut");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    let mut round = 3;
    for (hold, running) in [
        ("check-hold", "check-running"),
        ("filter-hold", "filter-running"),
    ] {
        // Cut by a look that a tool call starts: the head gets its try back.
        assert!(copies.keep_is_due(), "{hold}: a keep is due");
        let stood = marker_lines(&git_dir);
        let pid = cut_the_form(&mut copies, &dir, hold, running).await;
        let began = Instant::now();
        while alive(pid) && began.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !alive(pid),
            "TRIGGER PRESENT: {hold}: the git of a form that was cut is ended with its group"
        );
        assert_the_keeps_folders_are_empty(&git_dir, hold);
        assert_eq!(
            marker_lines(&git_dir),
            stood,
            "{hold}: no pack and no marker of the form that was cut stands: what stood before it stands"
        );
        assert_eq!(
            copies.take_form_note(),
            None,
            "{hold}: a keep that was cut leaves no form note"
        );
        copies.keep_cut(false);
        changed_look(&mut copies, &l.repo, round).await;
        assert!(
            copies.keep_is_due(),
            "TRIGGER PRESENT: {hold}: a form that a tool call's look cut is tried again after the next look of that head"
        );
        // Cut by the timer's look: the head stays tried.
        let pid = cut_the_form(&mut copies, &dir, hold, running).await;
        let began = Instant::now();
        while alive(pid) && began.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(pid), "{hold}: the git of the second cut");
        copies.keep_cut(true);
        changed_look(&mut copies, &l.repo, round + 1).await;
        assert!(
            !copies.keep_is_due(),
            "{hold}: a form that the timer's look cut is not tried again until the head has moved"
        );
        assert_the_keeps_folders_are_empty(&git_dir, hold);
        // A head that moved and came to rest is kept by the form: a cut set no session mark.
        let head = a_new_head_at_rest(&mut copies, &l, round, round + 2).await;
        let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
        assert!(matches!(keep, Keep::Kept { .. }), "{hold}: {keep:?}");
        assert_eq!(
            calls,
            vec![KeepCall::Index, KeepCall::Filter],
            "{hold}: a cut set no session mark: {note:?}"
        );
        assert_eq!(
            marker_lines(&git_dir),
            vec![format!("agentd kept bundle {head}")]
        );
        // The next head for the second way to be cut.
        a_new_head_at_rest(&mut copies, &l, round + 1, round + 4).await;
        round += 6;
    }
}

/// A look of the timer on a new head that leaves the timer (round `round` of the work), and the questions of the keep at once
/// answered: a keep at once is due.
async fn a_keep_at_once_is_due(copies: &mut Snapshotter, repo: &Path, round: u32) {
    copies.look_is_spaced(true);
    changed_look(copies, repo, round).await;
    copies.look_left_the_timer(true);
    assert!(
        copies.keep_at_once_may_follow(),
        "TRIGGER PRESENT is not shown: no keep at once may follow a look of the timer that left the timer on a new head"
    );
    copies.keep_at_once(&limit()).await;
    assert!(
        copies.keep_is_due(),
        "TRIGGER PRESENT is not shown: the questions of the keep at once did not make the keep due"
    );
}

/// Test 15 (h) of the build brief (ruling 49 (a) of the lead, thing 8; the challenge's M1): a head that is a loose file
/// and also stands in a pack of the engine (`git repack -a` without `-d`). The form runs, because the loose question says "a
/// regular file"; its filter leaves the head out, the worker finds the head missing in the index of the filtered pack (row e)
/// and installs nothing; release 5.28's keep answers `Keep::Nothing` in the same wait; and the marks are as that answer sets
/// them: the keep at once marked no head, so the keep after the second look of that head is due, and after a later burst a
/// keep at once may follow the first slow look.
#[tokio::test]
async fn b936_a_head_that_is_loose_and_also_packed_gives_way_behind_the_filter() {
    let Some(dir) =
        second_process("b936_a_head_that_is_loose_and_also_packed_gives_way_behind_the_filter")
    else {
        return;
    };
    let l = lab_on_a_packed_base("b936-loose-and-packed");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = engine_commits(&l.repo, 0, 5);
    git(&l.repo, &["repack", "-a", "-q"]);
    assert!(
        loose_object_path(&l.repo, &head).unwrap().is_file(),
        "TRIGGER PRESENT is not shown: the head is no loose file after `git repack -a` without `-d`"
    );
    a_keep_at_once_is_due(&mut copies, &l.repo, 1).await;
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Filter, KeepCall::Older],
        "the form ran, gave way behind its filter, and release 5.28's keep ran in the same wait: {note:?}"
    );
    assert_eq!(
        keep,
        Keep::Nothing,
        "TRIGGER PRESENT: a filtered pack that lacks the head is not installed: {note:?}"
    );
    assert!(
        note.starts_with("gave way index=")
            && note.contains("the engine's packs hold")
            && !note.contains("off for this session"),
        "the form's note: {note:?}"
    );
    assert_eq!(marker_lines(&git_dir), Vec::<String>::new());
    assert_the_keeps_folders_are_empty(&git_dir, "a head that is loose and also packed");
    // The keep at once marked no head: the keep after the second look of that head is due.
    changed_look(&mut copies, &l.repo, 2).await;
    assert!(
        copies.keep_is_due(),
        "the keep after the second look of the head is due behind a keep at once that found nothing to pack"
    );
    let (keep, _, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert_eq!(keep, Keep::Nothing, "{note:?}");
    // After a later burst a keep at once may follow the first slow look, and the form makes it.
    let head = engine_commits(&l.repo, 5, 5);
    a_keep_at_once_is_due(&mut copies, &l.repo, 3).await;
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}: {note:?}");
    assert_eq!(calls, vec![KeepCall::Index, KeepCall::Filter], "{note:?}");
    assert_eq!(
        marker_lines(&git_dir),
        vec![format!("agentd kept bundle {head}")]
    );
}

/// Test 15 (e) and (g) of the build brief (ruling 49 (a) of the lead, things 5 and 7): in a store that the engine
/// packed with `git repack -a -d` the head stands in a pack only. A keep at once and a keep at rest there start no
/// `index-pack --stdin` and no filter call, the `off:` note says that the head stands in a pack only, and release 5.28's keep
/// finds nothing to pack. Off is no refusal: after a commit without a repack the new head is a loose file, and the form makes
/// its pack.
#[tokio::test]
async fn b936_a_head_in_a_pack_only_turns_the_form_off_and_a_later_loose_head_is_kept_by_the_form()
{
    let Some(dir) = second_process(
        "b936_a_head_in_a_pack_only_turns_the_form_off_and_a_later_loose_head_is_kept_by_the_form",
    ) else {
        return;
    };
    let l = lab_on_a_packed_base("b936-off-is-no-refusal");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = engine_commits(&l.repo, 0, 5);
    git(&l.repo, &["repack", "-a", "-d", "-q"]);
    assert!(!loose_object_path(&l.repo, &head).unwrap().exists());
    // A keep at once.
    a_keep_at_once_is_due(&mut copies, &l.repo, 1).await;
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "a keep at once in a packed store",
        "the head stands in a pack only",
    )
    .await;
    assert_eq!(keep, Keep::Nothing);
    // The keep at rest, after the second look of the same head.
    changed_look(&mut copies, &l.repo, 2).await;
    assert!(copies.keep_is_due());
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "a keep at rest in a packed store",
        "the head stands in a pack only",
    )
    .await;
    assert_eq!(keep, Keep::Nothing);
    // A fresh commit without a repack: the head is a loose file, and the form runs.
    let head = a_new_head_at_rest(&mut copies, &l, 5, 3).await;
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Filter],
        "TRIGGER PRESENT: off is no refusal: a later head that is a loose file is kept by the form: {note:?}"
    );
    assert!(
        matches!(keep, Keep::Kept { .. }) && note.starts_with("made index="),
        "{keep:?}: {note:?}"
    );
    assert_eq!(
        marker_lines(&git_dir),
        vec![format!("agentd kept bundle {head}")]
    );
}

/// Test 6 of the build brief: the marker of a pack by the form and what a worker that starts does with it. The first
/// line reads `agentd kept bundle <head>`; by the rule of a worker of releases 5.28 to 5.31 (strip `agentd kept `, then ask for
/// an object id) what is left is no object id, so such a worker removes the pack. A worker of this build adopts the pack at
/// open after git's check of it, and removes one with wrong bytes. The form's folder is emptied at open, at the first step of
/// the next look and at the first step of the copy of the end, also of a `tmp_pack_*` that a cut indexing left.
#[tokio::test]
async fn b936_a_pack_by_the_form_is_adopted_at_open_and_the_forms_folder_is_emptied() {
    let Some(dir) = second_process(
        "b936_a_pack_by_the_form_is_adopted_at_open_and_the_forms_folder_is_emptied",
    ) else {
        return;
    };
    let l = lab_on_a_packed_base("b936-open");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    let (keep, calls, _) = keep_now(&mut copies, &dir, &git_dir).await;
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(calls, vec![KeepCall::Index, KeepCall::Filter]);
    let lines = marker_lines(&git_dir);
    assert_eq!(lines, vec![format!("agentd kept bundle {head}")]);
    // The rule of a worker before the form: what follows `agentd kept ` is no object id (7 to 64 hexadecimal digits).
    let rest = lines[0].strip_prefix(b860_kept_support::KEPT_LINE).unwrap();
    assert!(
        !((7..=64).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_hexdigit())),
        "a worker before the form reads no object id in {rest:?}"
    );
    let kept_bytes = copies.kept_bytes();
    assert!(kept_bytes > 0);
    let left = |name: &str| {
        std::fs::create_dir_all(indexed_folder(&git_dir)).unwrap();
        std::fs::write(
            indexed_folder(&git_dir).join(name),
            "left by a cut indexing",
        )
        .unwrap();
    };

    // A worker that starts: the form's folder is emptied, and the pack is adopted after git's check.
    drop(copies);
    left("tmp_pack_left_at_open");
    let mut copies = thin(&l, "data").await;
    assert_eq!(
        (copies.kept_bytes(), copies.take_kept_note()),
        (kept_bytes, None),
        "TRIGGER PRESENT: a worker that starts adopts a good pack of the form"
    );
    assert_eq!(marker_lines(&git_dir), lines);
    assert_eq!(
        names(&indexed_folder(&git_dir)),
        Vec::<String>::new(),
        "TRIGGER PRESENT: the form's folder is emptied at open"
    );
    // The first step of the next look, and the first step of the copy of the end.
    left("tmp_pack_left_before_a_look");
    changed_look(&mut copies, &l.repo, 3).await;
    assert_eq!(
        names(&indexed_folder(&git_dir)),
        Vec::<String>::new(),
        "a look"
    );
    left("tmp_pack_left_before_the_end");
    copies.final_copy(None).await.unwrap();
    assert_eq!(
        names(&indexed_folder(&git_dir)),
        Vec::<String>::new(),
        "the copy of the end"
    );

    // A pack of the form with wrong bytes is not adopted.
    drop(copies);
    let folder = kept_folder(&l, "data");
    let pack = folder.join(
        names(&folder)
            .into_iter()
            .find(|n| n.ends_with(".pack"))
            .unwrap(),
    );
    let mut bytes = std::fs::read(&pack).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0xff;
    std::fs::remove_file(&pack).unwrap();
    std::fs::write(&pack, bytes).unwrap();
    let mut copies = thin(&l, "data").await;
    assert_eq!(copies.kept_bytes(), 0, "a pack with wrong bytes is removed");
    assert!(copies.take_kept_note().is_some());
    assert_eq!(marker_lines(&git_dir), Vec::<String>::new());
}

/// A storage that answers what the engine's `.git` is (`Snapshotter::set_git_dir_reader`) and holds one answer back when a
/// test asks it to: the `nth` call from then on does not return until the test lets the storage answer. The storage answers
/// when the test ends, also when it fails: the runtime of a test waits for the thread of a held call.
struct HeldGitDir {
    arm: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    held: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// How many held calls have returned: a held call counts itself here when the storage lets it answer.
    returned: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    answers: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl HeldGitDir {
    fn on(copies: &mut Snapshotter) -> Self {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let storage = Self {
            arm: Arc::new(AtomicUsize::new(0)),
            held: Arc::new(AtomicUsize::new(0)),
            returned: Arc::new(AtomicUsize::new(0)),
            answers: Arc::new(AtomicBool::new(true)),
        };
        let (arm, held, returned, answers) = (
            storage.arm.clone(),
            storage.held.clone(),
            storage.returned.clone(),
            storage.answers.clone(),
        );
        copies.set_git_dir_reader(Arc::new(move |_: &Path| {
            if arm.load(Ordering::SeqCst) > 0 && arm.fetch_sub(1, Ordering::SeqCst) == 1 {
                held.fetch_add(1, Ordering::SeqCst);
                while !answers.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                returned.fetch_add(1, Ordering::SeqCst);
            }
            true
        }));
        storage
    }

    /// The `nth` call from now on is held (1 is the next call).
    fn hold_call(&self, nth: usize) {
        use std::sync::atomic::Ordering;
        self.answers.store(false, Ordering::SeqCst);
        self.arm.store(nth, Ordering::SeqCst);
    }

    /// How many calls were held so far.
    fn held(&self) -> usize {
        self.held.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The storage answers again; the held call returns, and the test waits until it has: every call that was held has
    /// counted itself as returned. The wait is on that condition and holds no time of its own (before, it slept 300 ms);
    /// a held call that has not returned 30 s after the storage answered fails the test. That is the storage's side
    /// only. The worker clears its own flag of an outstanding read later, on the same thread, behind the reader's answer
    /// (one listing of a folder and two file calls later), and no test can see that flag: a keep that starts behind this
    /// call can still meet it and is then refused. Such a keep is made with [`keep_when_the_earlier_read_is_back`]. No
    /// look that starts behind this call fails for a read that has not returned. But a look that reads the engine's packs
    /// itself has that read refused while the flag stands: a look of the timer or of a tool call
    /// (`Snapshotter::look_is_spaced`) has then read no packs, and no keep at once may follow it; a look while a pack is
    /// kept then reads no kept pack. A test that asks whether a keep at once may follow a look behind this call shows the
    /// worker's thread back first: with [`a_keep_at_once_is_due_when_the_earlier_read_is_back`] where one may follow, and
    /// with a keep that is not refused, in front of that look, where none may.
    async fn let_go(&self) {
        use std::sync::atomic::Ordering;
        self.answers.store(true, Ordering::SeqCst);
        let began = Instant::now();
        while self.returned.load(Ordering::SeqCst) < self.held.load(Ordering::SeqCst) {
            assert!(
                began.elapsed() < Duration::from_secs(30),
                "a held call of the storage did not return within 30 s of its answer: {} held, {} returned",
                self.held.load(Ordering::SeqCst),
                self.returned.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Drop for HeldGitDir {
    fn drop(&mut self) {
        self.answers
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Drives the future `work` until the storage holds its `nth` call so far, then drops it, as the copy task drops a keep when
/// the next look starts.
async fn drop_when_held<F: std::future::Future>(work: F, storage: &HeldGitDir, nth: usize)
where
    F::Output: std::fmt::Debug,
{
    tokio::pin!(work);
    let began = Instant::now();
    loop {
        tokio::select! {
            out = &mut work => panic!("TRIGGER PRESENT is not shown: the call ended while its read was held: {out:?}"),
            () = tokio::time::sleep(Duration::from_millis(10)) => {
                if storage.held() >= nth {
                    break;
                }
                assert!(
                    began.elapsed() < Duration::from_secs(30),
                    "TRIGGER PRESENT is not shown: the storage was never asked"
                );
            }
        }
    }
}

/// The keep that is due now, behind [`HeldGitDir::let_go`] (ruling 69 (a) of the lead). The storage has answered, and the
/// worker's own thread may still be on its way back with the earlier read: it clears the worker's flag of an outstanding
/// read after the reader's answer, and no test can see that flag. What a caller can see is the keep's answer. While that is
/// the refusal for an outstanding read, the test goes on as the worker's loop does: a refused read is no try, so after the
/// next look of the same head (round `round`, then the rounds after it) the keep is due again and is made again. The wait
/// is on the keep's answer and holds no time of its own; a keep that is still refused 30 s after the first one fails the
/// test. A refused keep starts no git call. The answer is what [`keep_now`] answers for the first keep that was not refused
/// for an outstanding read.
async fn keep_when_the_earlier_read_is_back(
    copies: &mut Snapshotter,
    repo: &Path,
    dir: &Path,
    git_dir: &Path,
    round: u32,
) -> (Keep, Vec<KeepCall>, String) {
    let began = Instant::now();
    let mut refused = 0u32;
    loop {
        let (keep, calls, note) = keep_now(copies, dir, git_dir).await;
        if !matches!(&keep, Keep::Missed(why) if why.contains("has not returned")) {
            if refused > 0 {
                println!(
                    "THE WAIT FOR THE EARLIER READ: {refused} keeps were refused for an outstanding read before the \
                     worker's thread was back with it ({} ms)",
                    began.elapsed().as_millis()
                );
            }
            return (keep, calls, note);
        }
        refused += 1;
        assert_eq!(
            calls,
            vec![],
            "a keep that is refused for an outstanding read starts no git call (refusal {refused})"
        );
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "the wait for the earlier read of the engine's packs to return on the worker's side: the storage has answered, \
             and {refused} keeps in {} ms were all refused for an outstanding read: {keep:?}",
            began.elapsed().as_millis()
        );
        changed_look(copies, repo, round + refused - 1).await;
        assert!(
            copies.keep_is_due(),
            "a refused read is no try: the keep is due again after the next look of the same head (refusal {refused})"
        );
    }
}

/// [`a_keep_at_once_is_due`] behind [`HeldGitDir::let_go`] (ruling 72 (a) of the lead). The storage has answered, and the
/// worker's own thread may still be on its way back with the earlier read. A look of the timer that starts before it is
/// back fails nothing, but its own read of the engine's packs is refused: it has read no packs, and no keep at once may
/// follow it. No test can see the worker's flag; what a caller can see is the answer to whether a keep at once may
/// follow. While that is no, the test goes on as a run does: the engine commits once more (round `more` of its commits,
/// then the rounds after it) and the next look of the timer leaves the timer on that head (the same rounds of the work).
/// Every repetition needs a new head: a second look of the same head makes the keep due by the rule of the second look,
/// and then no keep at once may follow whatever the look read. The wait is on the worker's answer and holds no time of
/// its own; when no keep at once may follow 30 s after the first look, the test fails. A look after which one may follow
/// has read the engine's packs, so the worker's thread was back when it started: the questions of the keep at once and
/// the keep behind them meet no flag. The first look is the one of round `round`, on the head the caller's commits left.
async fn a_keep_at_once_is_due_when_the_earlier_read_is_back(
    copies: &mut Snapshotter,
    repo: &Path,
    round: u32,
    more: u32,
) {
    let began = Instant::now();
    let mut refused = 0u32;
    copies.look_is_spaced(true);
    changed_look(copies, repo, round).await;
    copies.look_left_the_timer(true);
    while !copies.keep_at_once_may_follow() {
        refused += 1;
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "TRIGGER PRESENT is not shown: the wait for the earlier read of the engine's packs to return on the worker's \
             side: the storage has answered, and no keep at once may follow any of {refused} looks of the timer that left \
             the timer on a new head in {} ms",
            began.elapsed().as_millis()
        );
        engine_commits(repo, more + refused - 1, 1);
        copies.look_is_spaced(true);
        changed_look(copies, repo, more + refused - 1).await;
        copies.look_left_the_timer(true);
    }
    if refused > 0 {
        println!(
            "THE WAIT FOR THE EARLIER READ: {refused} looks of the timer read no packs of the engine before the worker's \
             thread was back with it ({} ms)",
            began.elapsed().as_millis()
        );
    }
    copies.keep_at_once(&limit()).await;
    assert!(
        copies.keep_is_due(),
        "TRIGGER PRESENT is not shown: the questions of the keep at once did not make the keep due"
    );
}

/// Test 3 (g) of the build brief (ruling 49 (c) of the lead, the challenge's blocker B1; ruling 51 (d), N2): a give-up
/// that ends the form's own read of the engine's store ends the keep there as `Keep::Missed` with the read's own words.
/// Release 5.28's keep is not run: no `pack-objects --revs` starts, and for step a no git of the form either. The marks stay
/// as the keep's start set them: a head at rest has had its try, and after a keep at once no keep at once follows the next
/// look that left the timer. An answer of the storage that comes after the give-up is not used. The same holds for the read
/// behind the filter (step d): the form's folders are empty and no pack and no marker of the form stands.
///
/// Ruling 72 (a) of the lead: behind an answer of the storage the worker's own thread may still be on its way back with the
/// read that was given up, and a look of the timer that starts before it is back reads no packs of the engine, so that no
/// keep at once may follow it whatever the marks are. The test asks that question twice, and each time it first waits on
/// what a caller can see. Where a keep at once may follow, it repeats a commit and a look of the timer until one may
/// ([`a_keep_at_once_is_due_when_the_earlier_read_is_back`]). Where none may follow, because the mark of a keep at once is
/// spent, no answer of the worker tells a look that read the packs from one that did not: there a keep that is not refused
/// for an outstanding read comes first ([`keep_when_the_earlier_read_is_back`]), one that finds nothing to pack in a store
/// the engine packed and so leaves that mark as it was. Every read of a keep that ran to its end has returned, so the look
/// behind it reads the engine's packs, and the spent mark alone is why no keep at once may follow.
#[tokio::test]
async fn b936_a_give_up_that_ends_the_forms_own_read_ends_the_keep_as_missed() {
    let Some(dir) =
        second_process("b936_a_give_up_that_ends_the_forms_own_read_ends_the_keep_as_missed")
    else {
        return;
    };
    let l = lab_on_a_packed_base("b936-read-given-up");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let storage = HeldGitDir::on(&mut copies);
    let soon = |ms: u64| {
        GiveUp::at(Some(
            tokio::time::Instant::now() + Duration::from_millis(ms),
        ))
    };
    let calls_since = |skip: usize| b860_kept_support::keep_calls_from(&dir, &git_dir, skip);
    let seen = || b860_kept_support::kept_seen(&dir, &git_dir).len();

    // A keep at rest; the read of step a is held past the give-up.
    a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    storage.hold_call(1);
    let skip = seen();
    let keep = copies.keep_commits(&soon(500)).await;
    assert_eq!(
        storage.held(),
        1,
        "TRIGGER PRESENT is not shown: step a was not held"
    );
    assert!(
        matches!(&keep, Keep::Missed(why) if why.contains("the work tree did not answer")),
        "TRIGGER PRESENT: a give-up that ends the form's own read is the keep's answer, with the read's own words: {keep:?}"
    );
    assert_eq!(
        calls_since(skip),
        vec![],
        "no git of the form and no call of the older keep"
    );
    assert_eq!(
        copies.take_form_note(),
        None,
        "no form line for a keep that the form's own read ended"
    );
    // The storage answers after the give-up: nothing of the form starts for that keep.
    storage.let_go().await;
    assert_eq!(
        calls_since(skip),
        vec![],
        "an answer that comes after the give-up is not used"
    );
    changed_look(&mut copies, &l.repo, 3).await;
    assert!(
        !copies.keep_is_due(),
        "TRIGGER PRESENT: the head at rest has had its try: no keep is due after the next look of that head"
    );

    // A keep at once; the storage answers first, then a look that left the timer on a moved head: no keep at once may follow.
    // The storage has just answered for the keep at rest: the keep at once is made due with the wait for the worker's thread.
    engine_commits(&l.repo, 1, 3);
    a_keep_at_once_is_due_when_the_earlier_read_is_back(&mut copies, &l.repo, 4, 3000).await;
    storage.hold_call(1);
    let skip = seen();
    let keep = copies.keep_commits(&soon(500)).await;
    assert_eq!(storage.held(), 2);
    assert!(
        matches!(&keep, Keep::Missed(why) if why.contains("the work tree did not answer")),
        "a keep at once: {keep:?}"
    );
    assert_eq!(calls_since(skip), vec![]);
    storage.let_go().await;
    // The worker's thread is shown back before the look of the timer, by a keep that is not refused for an outstanding read
    // and leaves the mark of a keep at once as it was (ruling 72 (a)). The engine packs its store; the keep after the second
    // look of the same head is due, because a keep at once that was missed marked no head; and it finds nothing to pack
    // (ruling 45 (c)).
    git(&l.repo, &["repack", "-a", "-d", "-q"]);
    changed_look(&mut copies, &l.repo, 5).await;
    assert!(
        copies.keep_is_due(),
        "a keep at once whose read was given up marked no head: the keep after the second look of that head is due"
    );
    let (keep, calls, note) =
        keep_when_the_earlier_read_is_back(&mut copies, &l.repo, &dir, &git_dir, 4000).await;
    assert_eq!(
        (keep, calls),
        (Keep::Nothing, vec![KeepCall::Older]),
        "TRIGGER PRESENT is not shown: the keep that shows the worker's thread back did not run to its end with nothing to \
         pack, in a store that the engine packed: {note:?}"
    );
    engine_commits(&l.repo, 4, 1);
    copies.look_is_spaced(true);
    changed_look(&mut copies, &l.repo, 6).await;
    copies.look_left_the_timer(true);
    assert!(
        !copies.keep_at_once_may_follow(),
        "TRIGGER PRESENT: a keep at once whose read was given up has been tried: no keep at once follows the next look that \
         left the timer, a look that read the engine's packs"
    );

    // The read behind the filter (step d) is held past the give-up.
    changed_look(&mut copies, &l.repo, 7).await;
    assert!(copies.keep_is_due(), "the rule of the second look");
    storage.hold_call(2);
    let skip = seen();
    // The give-up is handed when the form's two git calls are in the log and the read behind the filter is held: no time
    // of the test has to outlast git (before, a give-up of 4 s had to).
    let far = limit();
    let keep = {
        let keep = copies.keep_commits(&far);
        tokio::pin!(keep);
        let began = Instant::now();
        let mut handed = false;
        loop {
            tokio::select! {
                out = &mut keep => break out,
                () = tokio::time::sleep(Duration::from_millis(10)) => {
                    if !handed && storage.held() >= 3 {
                        assert_eq!(
                            calls_since(skip),
                            vec![KeepCall::Index, KeepCall::Filter],
                            "TRIGGER PRESENT is not shown: the filter is in the log when the read behind it is held"
                        );
                        far.no_later_than(tokio::time::Instant::now());
                        handed = true;
                    }
                    assert!(began.elapsed() < Duration::from_secs(60), "the keep did not end");
                }
            }
        }
    };
    assert_eq!(
        storage.held(),
        3,
        "TRIGGER PRESENT is not shown: step d was not held"
    );
    assert!(
        matches!(&keep, Keep::Missed(why) if why.contains("the work tree did not answer")),
        "step d: {keep:?}"
    );
    assert_eq!(
        calls_since(skip),
        vec![KeepCall::Index, KeepCall::Filter],
        "the form's two calls ran, and no `pack-objects --revs` started"
    );
    assert_eq!(copies.take_form_note(), None);
    assert_the_keeps_folders_are_empty(&git_dir, "a read behind the filter that was given up");
    assert_eq!(marker_lines(&git_dir), Vec::<String>::new());
    storage.let_go().await;
    changed_look(&mut copies, &l.repo, 8).await;
    assert!(!copies.keep_is_due(), "step d: the head has had its try");
}

/// Tests 3 (f) and 16 of the build brief (rulings 43 (e) and 49 (d) of the lead, the challenge's M2): the form's read
/// of the engine's store runs under the flag of the packs' read, never under the flag of a look's file calls. A question of a
/// keep at once that has not returned refuses step a of the next keep at once: no `index-pack --stdin` starts, the answer is
/// `Keep::Missed` with the words of an outstanding read, both marks are what they were (the keep is due again after the next
/// look of the same head), and no session mark is set: when the storage answers, the form makes the keep. A read of step a
/// that has not returned when its keep is dropped fails no look: the look of the timer and the look that a request to end
/// asks for make their copies, the copy of the end is made, a keep that starts meanwhile is missed at once, and when the
/// storage answers the form makes the next keep. Both keeps behind an answer of the storage are made with
/// [`keep_when_the_earlier_read_is_back`]: the worker's own thread clears its flag of an outstanding read after the storage's
/// answer, and a keep that starts before it has is still refused, which is no try.
#[tokio::test]
async fn b936_a_read_of_the_form_that_has_not_returned_fails_no_look_and_a_refused_read_is_no_try()
{
    let Some(dir) = second_process(
        "b936_a_read_of_the_form_that_has_not_returned_fails_no_look_and_a_refused_read_is_no_try",
    ) else {
        return;
    };
    let l = lab_on_a_packed_base("b936-read-outstanding");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let storage = HeldGitDir::on(&mut copies);
    let calls_since = |skip: usize| b860_kept_support::keep_calls_from(&dir, &git_dir, skip);
    let seen = || b860_kept_support::kept_seen(&dir, &git_dir).len();
    let outstanding =
        |keep: &Keep| matches!(keep, Keep::Missed(why) if why.contains("has not returned"));

    // Test 3 (f): the question of a keep at once is held, and that keep at once is dropped.
    engine_commits(&l.repo, 0, 5);
    copies.look_is_spaced(true);
    changed_look(&mut copies, &l.repo, 1).await;
    copies.look_left_the_timer(true);
    assert!(copies.keep_at_once_may_follow());
    storage.hold_call(1);
    {
        let far = limit();
        drop_when_held(copies.keep_at_once(&far), &storage, 1).await;
    }
    // The keep after the next second look of the head meets the refusal in step a.
    changed_look(&mut copies, &l.repo, 2).await;
    assert!(copies.keep_is_due());
    let skip = seen();
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        outstanding(&keep),
        "TRIGGER PRESENT: step a is refused while an earlier read has not returned: {keep:?}"
    );
    assert_eq!(
        calls_since(skip),
        vec![],
        "no `index-pack --stdin` and no keep call started"
    );
    changed_look(&mut copies, &l.repo, 3).await;
    assert!(
        copies.keep_is_due(),
        "a refused read is no try: the keep is due again after the next look of the same head"
    );
    storage.let_go().await;
    // The storage has answered; the worker's own thread may still be on its way back with the read.
    let (keep, calls, note) =
        keep_when_the_earlier_read_is_back(&mut copies, &l.repo, &dir, &git_dir, 1000).await;
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Filter],
        "TRIGGER PRESENT: the refusal set no session mark: when the storage answers, the form makes the keep: {note:?}"
    );

    // Test 16: step a is held from inside the keep, and the keep is dropped while the call is outstanding.
    a_new_head_at_rest(&mut copies, &l, 5, 4).await;
    storage.hold_call(1);
    {
        let far = limit();
        drop_when_held(copies.keep_commits(&far), &storage, 2).await;
    }
    copies.keep_cut(false);
    // The look of the timer that starts next makes its copy.
    copies.look_is_spaced(true);
    changed_look(&mut copies, &l.repo, 6).await;
    // A keep that starts while the call is outstanding is missed at once.
    assert!(copies.keep_is_due());
    let skip = seen();
    let keep = copies.keep_commits(&limit()).await;
    assert!(outstanding(&keep), "{keep:?}");
    assert_eq!(calls_since(skip), vec![]);
    // The look that a request to end asks for makes its copy, and so does the copy of the end.
    copies.look_is_spaced(false);
    changed_look(&mut copies, &l.repo, 7).await;
    let end = copies.final_copy(None).await.unwrap();
    assert!(end.final_copy);
    assert_eq!(storage.held(), 2, "the call is still the one held call");
    // The storage answers: the form makes the next keep.
    storage.let_go().await;
    let head = a_new_head_at_rest(&mut copies, &l, 6, 8).await;
    let (keep, calls, note) =
        keep_when_the_earlier_read_is_back(&mut copies, &l.repo, &dir, &git_dir, 2000).await;
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(calls, vec![KeepCall::Index, KeepCall::Filter], "{note:?}");
    assert_eq!(
        marker_lines(&git_dir),
        vec![format!("agentd kept bundle {head}")]
    );
}

// ----: the tests of the build brief that the first build left out ------------------------------------------------------------

/// A long text file of `lines` lines; `changed` names one line that reads differently.
fn long_text(lines: u32, changed: Option<u32>) -> String {
    (0..lines)
        .map(|n| {
            if Some(n) == changed {
                format!("line {n} of the long file, changed\n")
            } else {
                format!("line {n} of the long file\n")
            }
        })
        .collect()
}

/// A lab whose base holds a long file (`long.txt`, 4,000 lines), with the base in one pack as the product makes an engine's
/// store.
fn lab_with_a_long_file_at_the_base(label: &str) -> b860_thin_support::Lab {
    let dir = scratch(label);
    let repo = common::python_calc(&dir);
    std::fs::write(repo.join("long.txt"), long_text(4_000, None)).unwrap();
    git(&repo, &["add", "long.txt"]);
    git(&repo, &["commit", "-q", "-m", "base with a long file"]);
    git(&repo, &["repack", "-a", "-d", "-q"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let base_bundle = dir.join("repo.bundle");
    b860_thin_support::stage_base(&repo, &base_bundle);
    b860_thin_support::Lab {
        dir,
        repo,
        base_bundle,
        base,
    }
}

/// Test 3 (b) of the build brief with real git: a refusal of the indexing that no stand-in makes. The run's commits
/// hold shorter versions of a long file of the base, and the engine packs them with a bitmap (`git repack -a -d -b`): its pack
/// holds them as deltas of the base's version, and with the bitmap git writes such a delta into the look's bundle although
/// `pack.window` is 0 there. The bundle needs an object it does not hold. After one more commit the head is a loose file and
/// the form runs: `git index-pack --stdin`, with no other store in reach, ends with a non-zero exit and names an unresolved
/// delta. The form gives way with the session mark, release 5.28's keep runs in the same wait and its marker stands, the
/// form's folders are empty (git's `tmp_pack_*` too), and no `index-pack --stdin` starts at the next keep of the session.
#[tokio::test]
async fn b936_a_bundle_that_needs_an_object_of_the_base_is_refused_by_gits_own_indexing() {
    let Some(dir) = second_process(
        "b936_a_bundle_that_needs_an_object_of_the_base_is_refused_by_gits_own_indexing",
    ) else {
        return;
    };
    let l = lab_with_a_long_file_at_the_base("b936-thin-bundle");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    for round in 0..3u32 {
        std::fs::write(
            l.repo.join("long.txt"),
            long_text(3_900 - 10 * round, Some(100 * round)),
        )
        .unwrap();
        git(&l.repo, &["add", "long.txt"]);
        git(
            &l.repo,
            &["commit", "-q", "-m", &format!("shorter, round {round}")],
        );
    }
    git(&l.repo, &["repack", "-a", "-d", "-b", "-q"]);
    let head = a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert!(
        note.contains("unresolved delta"),
        "TRIGGER PRESENT is not shown: git's indexing did not refuse the look's bundle for a delta whose base it lacks: \
         {note:?} {calls:?}"
    );
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Older],
        "TRIGGER PRESENT: git's refusal of the indexing gives way before the filter, and release 5.28's keep runs in the \
         same wait"
    );
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert!(
        note.starts_with("gave way index=")
            && note.contains(" filter=0: git index-pack: ")
            && note.ends_with("; off for this session"),
        "the form's note: {note:?}"
    );
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    assert_the_keeps_folders_are_empty(&git_dir, "an indexing that git refused");
    let head = a_new_head_at_rest(&mut copies, &l, 1, 3).await;
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert_eq!(
        (calls, note.as_str()),
        (
            vec![KeepCall::Older],
            "off: git refused the form earlier in this session"
        ),
        "TRIGGER PRESENT: the session mark: {keep:?}"
    );
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
}

/// One more pack in the engine's store, of an object that no commit holds: the names of the engine's packs change, as after
/// a repack or a fetch, and no commit of the run moves.
fn a_new_pack_in_the_engines_store(l: &b860_thin_support::Lab, n: u32) {
    use std::io::Write;
    let file = l.dir.join(format!("other-{n}.bin"));
    std::fs::write(&file, noise(u64::from(n) + 936, 4_096)).unwrap();
    let id = git(&l.repo, &["hash-object", "-w", file.to_str().unwrap()]);
    let mut child = std::process::Command::new("git")
        .current_dir(&l.repo)
        .args(["pack-objects", "-q", ".git/objects/pack/pack"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", id.trim()).as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

/// The head moves (round `from` of the engine's commits) and a look of the timer leaves the timer on the new head (round
/// `round` of the work). The answer is whether a keep at once may follow that look.
async fn may_a_keep_at_once_follow_a_new_head(
    copies: &mut Snapshotter,
    l: &b860_thin_support::Lab,
    from: u32,
    round: u32,
) -> bool {
    engine_commits(&l.repo, from, 1);
    copies.look_is_spaced(true);
    changed_look(copies, &l.repo, round).await;
    copies.look_left_the_timer(true);
    copies.keep_at_once_may_follow()
}

/// Test 3 (e) of the build brief (the challenge's finding 1 and M4, ruling 49 (d) of the lead): the marks
/// behind a form that gave way or was cut at a keep at once, shown as `a_keep_at_once_that_was_not_kept_is_not_tried_again`
/// shows them, in a lab whose base stands in a pack and with no packs reader, so that the form is on.
///
/// - Git refuses the indexing and release 5.28's keep keeps a pack in the same wait: the keep was kept, so the mark of the
///   keep at once is clear. Once a real pack has changed the engine's packs (the kept pack goes at the next look, and that
///   look reads none) and the head has moved, a keep at once may follow a look that left the timer.
/// - Git refuses the indexing and release 5.28's keep is missed in the same wait: the marks are those of a missed keep at
///   once. No keep at once may follow the next look that left the timer on a new head, and the rule of the second look
///   keeps.
/// - The form is cut while `index-pack` is held, and while its filter is held by the filter's own hold file: each time the
///   mark stays set, the rule of the second look keeps (by the form: a cut sets no session mark), and that kept pack
///   clears the mark.
#[tokio::test]
async fn b936_a_keep_at_once_behind_a_form_that_gave_way_or_was_cut_keeps_the_marks_of_b932() {
    let Some(dir) = second_process(
        "b936_a_keep_at_once_behind_a_form_that_gave_way_or_was_cut_keeps_the_marks_of_b932",
    ) else {
        return;
    };
    // The indexing is refused and release 5.28's keep keeps a pack: the mark is clear.
    let l = lab_on_a_packed_base("b936-at-once-refused-kept");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = engine_commits(&l.repo, 0, 5);
    a_keep_at_once_is_due(&mut copies, &l.repo, 1).await;
    std::fs::write(dir.join("index-fail"), "").unwrap();
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    std::fs::remove_file(dir.join("index-fail")).unwrap();
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Older],
        "TRIGGER PRESENT is not shown: the form of the keep at once did not give way at its indexing: {note:?}"
    );
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    a_new_pack_in_the_engines_store(&l, 1);
    assert!(
        may_a_keep_at_once_follow_a_new_head(&mut copies, &l, 5, 2).await,
        "a keep at once that release 5.28's keep kept behind a form that gave way left no mark"
    );
    assert_eq!(
        marker_lines(&git_dir),
        Vec::<String>::new(),
        "TRIGGER PRESENT is not shown: the look after the engine's packs changed read a kept pack, so no keep at once may \
         follow it whatever the mark is"
    );

    // The indexing is refused and release 5.28's keep is missed: the marks of a missed keep at once.
    let l = lab_on_a_packed_base("b936-at-once-refused-missed");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 5);
    a_keep_at_once_is_due(&mut copies, &l.repo, 1).await;
    std::fs::write(dir.join("index-fail"), "").unwrap();
    std::fs::write(dir.join("keep-fail"), "").unwrap();
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    std::fs::remove_file(dir.join("index-fail")).unwrap();
    std::fs::remove_file(dir.join("keep-fail")).unwrap();
    assert!(
        matches!(keep, Keep::Missed(_)) && calls == vec![KeepCall::Index, KeepCall::Older],
        "TRIGGER PRESENT is not shown: release 5.28's keep was not missed behind a form that gave way: {keep:?} {calls:?} \
         {note:?}"
    );
    assert!(
        !may_a_keep_at_once_follow_a_new_head(&mut copies, &l, 5, 2).await,
        "TRIGGER PRESENT: a keep at once that was missed behind a form that gave way is not tried again: no keep at once \
         may follow the next look that left the timer on a new head"
    );
    copies.keep_at_once(&limit()).await;
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    copies.look_is_spaced(true);
    changed_look(&mut copies, &l.repo, 3).await;
    assert!(
        copies.keep_is_due(),
        "the keep after the second look of the same head does not ask the mark"
    );
    let (keep, calls, _) = keep_now(&mut copies, &dir, &git_dir).await;
    assert!(
        matches!(keep, Keep::Kept { .. }) && calls == vec![KeepCall::Older],
        "the rule of the second look keeps, by release 5.28's keep after the refusal: {keep:?} {calls:?}"
    );

    // The form of a keep at once is cut in its indexing and in its filter: the mark stays set.
    for (n, (hold, running)) in [
        ("check-hold", "check-running"),
        ("filter-hold", "filter-running"),
    ]
    .into_iter()
    .enumerate()
    {
        let l = lab_on_a_packed_base(&format!("b936-at-once-cut-{n}"));
        let mut copies = thin(&l, "data").await;
        let git_dir = private(&l, "data");
        engine_commits(&l.repo, 0, 5);
        a_keep_at_once_is_due(&mut copies, &l.repo, 1).await;
        let pid = cut_the_form(&mut copies, &dir, hold, running).await;
        let began = Instant::now();
        while alive(pid) && began.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(pid), "{hold}: the git of the form that was cut");
        // What started the next look cut the keep: a tool call, which leaves no mark of its own on a head.
        copies.keep_cut(false);
        assert_the_keeps_folders_are_empty(&git_dir, hold);
        assert!(
            !may_a_keep_at_once_follow_a_new_head(&mut copies, &l, 5, 2).await,
            "TRIGGER PRESENT: {hold}: the form of a keep at once that was cut leaves the mark set: no keep at once may \
             follow the next look that left the timer on a new head"
        );
        copies.keep_at_once(&limit()).await;
        assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue, "{hold}");
        copies.look_is_spaced(true);
        changed_look(&mut copies, &l.repo, 3).await;
        assert!(
            copies.keep_is_due(),
            "{hold}: the keep after the second look of the same head does not ask the mark"
        );
        let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
        assert!(
            matches!(keep, Keep::Kept { .. }) && calls == vec![KeepCall::Index, KeepCall::Filter],
            "{hold}: the rule of the second look keeps, by the form: a cut set no session mark: {keep:?} {calls:?} {note:?}"
        );
        // A pack kept by the form clears the mark, shown with the same step behind it.
        a_new_pack_in_the_engines_store(&l, 2);
        assert!(
            may_a_keep_at_once_follow_a_new_head(&mut copies, &l, 6, 4).await,
            "{hold}: after a keep that the form kept, a keep at once may follow a look again"
        );
        assert_eq!(
            marker_lines(&git_dir),
            Vec::<String>::new(),
            "{hold}: TRIGGER PRESENT is not shown: the look after the engine's packs changed read a kept pack"
        );
    }
}

/// Indexes the pack of the bundle `bundle` with nothing else in reach: git's own `index-pack` on a file that holds the
/// bundle's bytes behind its header, in a repository that holds no object. The error is what git said when it refused.
fn index_alone(l: &b860_thin_support::Lab, name: &str, bundle: &Path) -> Result<(), String> {
    let bytes = std::fs::read(bundle).unwrap();
    let pack_at = bytes.windows(2).position(|w| w == b"\n\n").unwrap() + 2;
    let store = l.dir.join(format!("alone-{name}"));
    git(&l.dir, &["init", "-q", "--bare", store.to_str().unwrap()]);
    std::fs::write(store.join("in.pack"), &bytes[pack_at..]).unwrap();
    let out = std::process::Command::new("git")
        .current_dir(&store)
        .args(["index-pack", "in.pack"])
        .output()
        .unwrap();
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// Test 7 of the build brief: `pack.window = 0`, the indexing half. The run commits a changed version of a long file
/// of the base. With a window git writes that version into the look's bundle as a delta of the base's version, which the
/// bundle does not hold: in a private repository whose setting a test changed to a window of 10, git's indexing of the
/// look's bundle with no other store in reach is refused for an unresolved delta. With the setting the worker writes
/// (`TUNE_BLOCK`), the bundle of the same state holds the version whole, and git indexes it with no other store in reach,
/// which is what the form's `index-pack --stdin` does.
#[tokio::test]
async fn b936_a_looks_bundle_is_indexed_with_no_other_store_in_reach() {
    let l = lab_with_a_long_file_at_the_base("b936-window");
    std::fs::write(l.repo.join("long.txt"), long_text(4_000, Some(7))).unwrap();
    git(&l.repo, &["add", "long.txt"]);
    git(
        &l.repo,
        &["commit", "-q", "-m", "one line of the long file"],
    );

    let mut control = thin(&l, "data-window").await;
    let control_git = private(&l, "data-window");
    git(
        &control_git,
        &[
            "--git-dir",
            control_git.to_str().unwrap(),
            "config",
            "pack.window",
            "10",
        ],
    );
    let made = changed_look(&mut control, &l.repo, 1).await;
    let refused = index_alone(&l, "window-10", &made.bundle);
    assert!(
        matches!(&refused, Err(why) if why.contains("unresolved delta")),
        "TRIGGER PRESENT is not shown: with a window of 10 the look's bundle of this state is indexed with no other store \
         in reach: {refused:?} ({} bytes)",
        made.bundle_bytes
    );

    let mut copies = thin(&l, "data").await;
    let made = changed_look(&mut copies, &l.repo, 2).await;
    let alone = index_alone(&l, "window-0", &made.bundle);
    assert_eq!(
        alone,
        Ok(()),
        "TRIGGER PRESENT: with the worker's setting git indexes the look's bundle with no other store in reach ({} bytes)",
        made.bundle_bytes
    );
}

/// Test 9 of the build brief: over a base that stands in a pack, a keep by the form opens no loose object file of the
/// engine's store: git indexes the look's bundle on the worker's own disk, and the filter reads the indexed pack and the
/// indexes of the engine's packs. Release 5.28's keep, the control on the same state, opens every loose object of the
/// run's 100 commits. Counted as `b860_kept_later_looks_do_not_open_the_engines_objects` counts, on the folders of loose
/// objects alone.
#[tokio::test]
async fn b936_a_keep_by_the_form_opens_no_loose_object_of_the_engines_store() {
    let (mut opened, mut markers) = (Vec::new(), Vec::new());
    for form in [false, true] {
        let l = lab_on_a_packed_base(if form {
            "b936-opens-form"
        } else {
            "b936-opens-older"
        });
        let mut copies = thin(&l, "data").await;
        copies.set_session_restored(!form);
        let head = engine_commits(&l.repo, 0, 100);
        two_looks(&mut copies, &l.repo, 1).await;
        let mut watch = Opens::watch_loose(&l.repo);
        let keep = copies.keep_commits(&limit()).await;
        opened.push(watch.count());
        assert!(matches!(keep, Keep::Kept { .. }), "form {form}: {keep:?}");
        let marker = if form {
            format!("agentd kept bundle {head}")
        } else {
            format!("agentd kept {head}")
        };
        markers.push((marker_lines(&private(&l, "data")), vec![marker]));
    }
    println!(
        "loose object files of the engine's store opened by one keep over 100 commits: {} by release 5.28's keep, {} by \
         the form",
        opened[0], opened[1]
    );
    assert!(
        opened[0] >= 300,
        "TRIGGER PRESENT is not shown: release 5.28's keep opened {} loose object files of the engine's store",
        opened[0]
    );
    assert_eq!(
        opened[1], 0,
        "TRIGGER PRESENT: the form opened loose object files of the engine's store; release 5.28's keep opens {}",
        opened[0]
    );
    for (stands, wanted) in markers {
        assert_eq!(
            stands, wanted,
            "TRIGGER PRESENT is not shown: which keep made each pack"
        );
    }
}

/// Test 15 (c) of the build brief (ruling 49 (a) of the lead): one head. The keep at rest asks its questions for the
/// head the look read and never reads the engine's `HEAD` again. In the first store the look's head is a loose file, and
/// after the look the engine's `HEAD` moves to a commit that stands in a pack only: the form runs and its marker names the
/// look's head. In the second store the look's head stands in a pack only, and after the look `HEAD` moves to a commit
/// that is loose: the form is off. Neither keep starts a `git rev-parse`.
#[tokio::test]
async fn b936_the_keep_at_rest_asks_for_the_head_the_look_read_and_not_for_the_engines_head_now() {
    let Some(dir) = second_process(
        "b936_the_keep_at_rest_asks_for_the_head_the_look_read_and_not_for_the_engines_head_now",
    ) else {
        return;
    };
    // The look's head is loose; the engine's head now stands in a pack only.
    let l = lab_on_a_packed_base("b936-one-head-loose");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = engine_commits(&l.repo, 0, 3);
    two_looks(&mut copies, &l.repo, 1).await;
    git(&l.repo, &["reset", "-q", "--soft", &l.base]);
    assert_eq!(git(&l.repo, &["rev-parse", "HEAD"]), l.base);
    assert!(
        loose_object_path(&l.repo, &head).unwrap().is_file()
            && !loose_object_path(&l.repo, &l.base).unwrap().exists(),
        "TRIGGER PRESENT is not shown: the look's head is a loose file and the engine's head now is not"
    );
    let from = git_calls(&dir).len();
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    let started = git_calls(&dir).split_off(from);
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Filter],
        "TRIGGER PRESENT: the form runs for the head the look read, which is a loose file: {note:?}"
    );
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(
        marker_lines(&git_dir),
        vec![format!("agentd kept bundle {head}")],
        "the marker names the look's head"
    );
    assert!(
        !started.iter().any(|call| command_of(call) == "rev-parse"),
        "the keep at rest reads no head: {started:?}"
    );

    // The look's head stands in a pack only; the engine's head now is loose.
    let l = lab_on_a_packed_base("b936-one-head-packed");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = engine_commits(&l.repo, 0, 3);
    git(&l.repo, &["repack", "-a", "-d", "-q"]);
    two_looks(&mut copies, &l.repo, 1).await;
    let now = engine_commits(&l.repo, 3, 1);
    assert!(
        !loose_object_path(&l.repo, &head).unwrap().exists()
            && loose_object_path(&l.repo, &now).unwrap().is_file(),
        "TRIGGER PRESENT is not shown: the look's head stands in a pack only and the engine's head now is a loose file"
    );
    let from = git_calls(&dir).len();
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "a head that moved behind the look",
        "the head stands in a pack only",
    )
    .await;
    let started = git_calls(&dir).split_off(from);
    assert_eq!(
        keep,
        Keep::Nothing,
        "the engine packed every commit below the look's head"
    );
    assert!(
        !started.iter().any(|call| command_of(call) == "rev-parse"),
        "the keep at rest reads no head: {started:?}"
    );
}

// ----: the rules of the session mark and of the pack names that the first build left untested ---------------------------------

/// Runs the keep that is due with the give-up `give_up`, and calls `at_the_hold` once, when the git call that the stand-in
/// holds through the file `hold` has written its process id into `running`; `at_the_hold` lets the call go (or leaves it
/// held until the give-up). The answer is the keep's.
async fn keep_with_a_hold(
    copies: &mut Snapshotter,
    dir: &Path,
    give_up: &GiveUp,
    (hold, running): (&str, &str),
    mut at_the_hold: impl FnMut(),
) -> Keep {
    let _ = std::fs::remove_file(dir.join(running));
    std::fs::write(dir.join(hold), "").unwrap();
    let mut seen = false;
    let keep = copies.keep_commits(give_up);
    tokio::pin!(keep);
    let began = Instant::now();
    let out = loop {
        tokio::select! {
            out = &mut keep => break out,
            () = tokio::time::sleep(Duration::from_millis(20)) => {
                if !seen && held_pid(&dir.join(running)).is_some() {
                    seen = true;
                    at_the_hold();
                }
                assert!(began.elapsed() < Duration::from_secs(60), "the keep did not end ({hold})");
            }
        }
    };
    assert!(
        seen,
        "TRIGGER PRESENT is not shown: no git call was held through {hold}: {out:?}"
    );
    out
}

/// The review's fourth major and one of its minors (rulings 42 (c)(ii), 44 (a) and 49 (a)(6) of the lead): what else
/// sets no session mark. A filter that is held until the keep's give-up; a filter that git refuses while the names of the
/// engine's packs changed around it; a name in the engine's pack folder that ends in `.pack`, has its index beside it and
/// is not `pack-<40 hexadecimal characters>.pack`; and a `.git` that is no longer the repository's own directory when the
/// form reads the store behind its filter. Each time the form gives way for that keep, release 5.28's keep runs in the
/// same wait, and the form runs again at the next keep moment.
#[tokio::test]
async fn b936_what_else_gives_way_without_the_session_mark() {
    let Some(dir) = second_process("b936_what_else_gives_way_without_the_session_mark") else {
        return;
    };
    let runs_again = |calls: Vec<KeepCall>, note: String, after: &str| {
        assert_eq!(
            calls,
            vec![KeepCall::Index, KeepCall::Filter],
            "TRIGGER PRESENT: {after} set no session mark, so the form runs at the next keep moment: {note:?}"
        );
    };

    // The filter is held until the keep's give-up. Release 5.28's keep has a give-up of its own.
    let l = lab_on_a_packed_base("b936-filter-out-of-time");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    let skip = b860_kept_support::kept_seen(&dir, &git_dir).len();
    let short = GiveUp::at(Some(
        tokio::time::Instant::now() + Duration::from_millis(1_500),
    ));
    let keep = keep_with_a_hold(
        &mut copies,
        &dir,
        &short,
        ("filter-hold", "filter-running"),
        || {},
    )
    .await;
    std::fs::remove_file(dir.join("filter-hold")).unwrap();
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "the keep behind a form whose filter ran out of time has a give-up of its own: {keep:?}"
    );
    assert_eq!(
        b860_kept_support::keep_calls_from(&dir, &git_dir, skip),
        vec![KeepCall::Index, KeepCall::Filter, KeepCall::Older]
    );
    let note = copies.take_form_note().unwrap_or_default();
    assert!(
        note.starts_with("gave way index=")
            && note.contains(": git pack-objects was given up before it had ended")
            && !note.contains("off for this session"),
        "the form's note: {note:?}"
    );
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    assert_the_keeps_folders_are_empty(&git_dir, "a filter that was given up");
    a_new_head_at_rest(&mut copies, &l, 1, 3).await;
    let (_, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    runs_again(calls, note, "a filter that ran out of time");

    // Git refuses the filter, and the names of the engine's packs changed around it: a pack lands while the indexing is
    // held, behind the form's first read of the store.
    let l = lab_on_a_packed_base("b936-refused-and-changed");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    std::fs::write(dir.join("filter-fail"), "").unwrap();
    let skip = b860_kept_support::kept_seen(&dir, &git_dir).len();
    let keep = keep_with_a_hold(
        &mut copies,
        &dir,
        &limit(),
        ("check-hold", "check-running"),
        || {
            a_new_pack_in_the_engines_store(&l, 7);
            std::fs::remove_file(dir.join("check-hold")).unwrap();
        },
    )
    .await;
    std::fs::remove_file(dir.join("filter-fail")).unwrap();
    assert_eq!(
        b860_kept_support::keep_calls_from(&dir, &git_dir, skip),
        vec![KeepCall::Index, KeepCall::Filter, KeepCall::Older],
        "TRIGGER PRESENT is not shown: the filter ran and was refused: {keep:?}"
    );
    let note = copies.take_form_note().unwrap_or_default();
    assert!(
        note.contains("the engine's packs changed while the pack was made")
            && !note.contains("off for this session"),
        "TRIGGER PRESENT: a filter that git refused while the engine's packs changed sets no session mark: {note:?}"
    );
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_the_keeps_folders_are_empty(&git_dir, "a refused filter and packs that changed");
    a_new_head_at_rest(&mut copies, &l, 1, 3).await;
    let (_, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    runs_again(calls, note, "a refused filter while the packs changed");

    // A name of another shape that ends in `.pack` and has its index beside it: a copy of the engine's own pack.
    let l = lab_on_a_packed_base("b936-other-shape-with-index");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    let pack_folder = l.repo.join(".git/objects/pack");
    let own = names(&pack_folder)
        .into_iter()
        .find(|name| name.ends_with(".pack"))
        .unwrap();
    for extension in ["pack", "idx"] {
        std::fs::copy(
            pack_folder.join(&own).with_extension(extension),
            pack_folder.join(format!("tmp_pack_in_flight.{extension}")),
        )
        .unwrap();
    }
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    for extension in ["pack", "idx"] {
        std::fs::remove_file(pack_folder.join(format!("tmp_pack_in_flight.{extension}"))).unwrap();
    }
    assert_eq!(
        calls,
        vec![KeepCall::Older],
        "TRIGGER PRESENT: a name of another shape, with its index beside it, is not named to the filter: neither the \
         indexing nor the filter starts: {note:?}"
    );
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert!(
        note.starts_with(
            "gave way index=0 filter=0: the engine's pack folder holds tmp_pack_in_flight.pack"
        ) && !note.contains("off for this session"),
        "{note:?}"
    );
    assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
    a_new_head_at_rest(&mut copies, &l, 1, 3).await;
    let (_, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    runs_again(calls, note, "a name of another shape with its index");

    // The engine's `.git` is the repository's own at the form's first read and no longer at the read behind the filter.
    let l = lab_on_a_packed_base("b936-git-dir-gone-behind-the-filter");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = asked.clone();
    copies.set_git_dir_reader(std::sync::Arc::new(move |_| {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) != 1
    }));
    let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
    assert!(
        asked.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "TRIGGER PRESENT is not shown: the form asked what `.git` is fewer than two times"
    );
    assert_eq!(
        calls,
        vec![KeepCall::Index, KeepCall::Filter, KeepCall::Older],
        "TRIGGER PRESENT: a `.git` that is no longer the repository's own behind the filter makes the form give way: \
         {note:?}"
    );
    assert!(
        note.contains("the engine's packs changed while the pack was made")
            && !note.contains("off for this session"),
        "{note:?}"
    );
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert_eq!(
        marker_lines(&git_dir),
        vec![format!("agentd kept {head}")],
        "no pack by the form stands"
    );
    assert_the_keeps_folders_are_empty(&git_dir, "a `.git` that changed behind the filter");
}

/// The review's minors on the form's own refusals and its start (ruling 56 (b)(3) of the lead): a filtered pack
/// whose index cannot be read, and an indexing that ends 0 and names no pack, or names a pack that does not stand in the
/// form's folder (ruling 69 (b) of the lead), are the form's own refusal and set the
/// session mark; a head whose loose object path holds something that is no regular file turns the form off with a line
/// of its own, which is not the line of a head that stands in a pack only; the form empties its folders before it starts;
/// and the note of a form that gave way before its keep was cut is not left for a later keep.
#[tokio::test]
async fn b936_the_forms_own_refusals_its_start_and_its_note() {
    let Some(dir) = second_process("b936_the_forms_own_refusals_its_start_and_its_note") else {
        return;
    };
    // The index of the filtered pack cannot be read; the indexing names no pack; the indexing prints a name of 40
    // hexadecimal characters and the pack of that name does not stand in the form's folder.
    for (steer, calls_wanted, words) in [
        (
            "filter-bad-index",
            vec![KeepCall::Index, KeepCall::Filter, KeepCall::Older],
            "the index of the filtered pack could not be read",
        ),
        (
            "index-no-name",
            vec![KeepCall::Index, KeepCall::Older],
            "git index-pack named no pack",
        ),
        (
            "index-name-no-pack",
            vec![KeepCall::Index, KeepCall::Older],
            "git index-pack named no pack",
        ),
    ] {
        let l = lab_on_a_packed_base(&format!("b936-own-refusal-{steer}"));
        let mut copies = thin(&l, "data").await;
        let git_dir = private(&l, "data");
        let head = a_new_head_at_rest(&mut copies, &l, 0, 1).await;
        std::fs::write(dir.join(steer), "").unwrap();
        let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
        std::fs::remove_file(dir.join(steer)).unwrap();
        assert_eq!(
            calls, calls_wanted,
            "TRIGGER PRESENT is not shown: {steer}: {note:?}"
        );
        assert!(
            note.starts_with("gave way index=")
                && note.contains(words)
                && note.ends_with("; off for this session"),
            "{steer}: the form's note: {note:?}"
        );
        assert!(matches!(keep, Keep::Kept { .. }), "{steer}: {keep:?}");
        assert_eq!(marker_lines(&git_dir), vec![format!("agentd kept {head}")]);
        assert_the_keeps_folders_are_empty(&git_dir, steer);
        a_new_head_at_rest(&mut copies, &l, 1, 3).await;
        let (keep, calls, note) = keep_now(&mut copies, &dir, &git_dir).await;
        assert_eq!(
            (calls, note.as_str()),
            (
                vec![KeepCall::Older],
                "off: git refused the form earlier in this session"
            ),
            "TRIGGER PRESENT: {steer}: the session mark: {keep:?}"
        );
    }

    // Something that is no regular file stands where the head's loose object file would: the question has no answer.
    let l = lab_on_a_packed_base("b936-loose-path-no-file");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    let head = engine_commits(&l.repo, 0, 3);
    git(&l.repo, &["repack", "-a", "-d", "-q"]);
    two_looks(&mut copies, &l.repo, 1).await;
    let path = loose_object_path(&l.repo, &head).unwrap();
    assert!(
        !path.exists(),
        "TRIGGER PRESENT is not shown: the head is still a loose file"
    );
    std::fs::create_dir_all(&path).unwrap();
    let keep = assert_the_form_is_off(
        &mut copies,
        &dir,
        &git_dir,
        "a loose path that is no regular file",
        "the head's loose object file could not be read: it is no regular file",
    )
    .await;
    std::fs::remove_dir(&path).unwrap();
    assert_eq!(keep, Keep::Nothing, "the engine packed every commit");

    // The form empties its folders before it starts.
    let l = lab_on_a_packed_base("b936-folders-at-the-start");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    let stray = "left-by-an-earlier-keep";
    let (indexed, incoming) = (indexed_folder(&git_dir), git_dir.join("kept/incoming"));
    for folder in [&indexed, &incoming] {
        std::fs::create_dir_all(folder).unwrap();
        std::fs::write(folder.join(stray), "bytes of a keep that ended badly").unwrap();
    }
    let mut at_the_filter = (Vec::new(), Vec::new());
    let keep = keep_with_a_hold(
        &mut copies,
        &dir,
        &limit(),
        ("filter-hold", "filter-running"),
        || {
            at_the_filter = (names(&indexed), names(&incoming));
            std::fs::remove_file(dir.join("filter-hold")).unwrap();
        },
    )
    .await;
    assert!(matches!(keep, Keep::Kept { .. }), "{keep:?}");
    assert!(
        !at_the_filter.0.iter().any(|name| name == stray)
            && at_the_filter.0.iter().any(|name| name.ends_with(".pack"))
            && at_the_filter.1.is_empty(),
        "TRIGGER PRESENT: when its filter starts, the form's folder holds the indexed pack and nothing an earlier keep \
         left, and the folder a keep writes into is empty: {at_the_filter:?}"
    );

    // A form gave way, and release 5.28's keep behind it was cut: the worker takes no note of a keep that was cut. The
    // note is not a later keep's.
    let l = lab_on_a_packed_base("b936-note-of-a-cut-keep");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    a_new_head_at_rest(&mut copies, &l, 0, 1).await;
    let skip = b860_kept_support::kept_seen(&dir, &git_dir).len();
    let pid = cut_the_keep(&mut copies, &dir).await;
    let began = Instant::now();
    while alive(pid) && began.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        b860_kept_support::keep_calls_from(&dir, &git_dir, skip),
        vec![KeepCall::Index, KeepCall::Filter, KeepCall::Older],
        "TRIGGER PRESENT is not shown: the form had not given way before release 5.28's keep was cut"
    );
    copies.keep_cut(true);
    changed_look(&mut copies, &l.repo, 3).await;
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    assert_eq!(
        copies.take_form_note(),
        None,
        "TRIGGER PRESENT: what the form of a keep that was cut left for the log is not a later keep's"
    );
}
