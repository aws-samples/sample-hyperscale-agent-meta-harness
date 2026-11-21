//! The clean-up of agentd's private repositories waits for a mark.
//!
//! `git prune` ran at the start of every look of the copy task that followed a changed one, and walked what the repository can reach
//! each time: on a history of 20,000 small commits that was 0.32 s of processor time where the rest of the look cost 0.03 s. The
//! clean-up now runs once the repository has grown by 64 MiB of loose objects, by 6,700 loose objects or by 64 MiB of packs since the
//! last clean-up, or when its disk has less than 1 GiB free. These tests run git and no engine.
//!
//! agentd starts `git` by name, and a process cannot change its own search path here. A test that needs a `git` of its own in front
//! of the real one (one that logs each `prune`, and can make a `prune` wait) starts this test binary once more with that search path
//! and its scratch folder named in the environment; that second process makes the readings and holds the bounds.
#![allow(clippy::unwrap_used)]

mod b860_support;
mod common;

use agentd::direct::driver::{GitProbe, TreeProbe};
use agentd::direct::goal::tree::TreeFp;
use agentd::direct::input::TreeSource;
use agentd::direct::snapshot::{
    GitSource, Loose, Snapshotter, TIDY_AFTER_KIB, TIDY_AFTER_OBJECTS, TIDY_FREE_BYTES, Tidy,
    TidyMarks, tidy_needed,
};
use b860_support::{MIB, noise};
use common::{git, python_calc, scratch};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The loose objects of the repository at `git_dir`: the files in its two-letter object folders.
fn loose_objects(git_dir: &Path) -> u64 {
    let mut n = 0;
    for e in std::fs::read_dir(git_dir.join("objects")).unwrap() {
        let e = e.unwrap();
        if e.file_name().len() == 2 && e.file_type().unwrap().is_dir() {
            n += std::fs::read_dir(e.path()).unwrap().count() as u64;
        }
    }
    n
}

/// The bytes of every file under `dir`.
fn bytes_under(dir: &Path) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let t = e.file_type().unwrap();
        if t.is_dir() {
            total += bytes_under(&e.path());
        } else if t.is_file() {
            total += e.metadata().unwrap().len();
        }
    }
    total
}

async fn setup(label: &str) -> (PathBuf, PathBuf, PathBuf, Snapshotter) {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let snap = Snapshotter::new(&data, &repo, &base).await.unwrap();
    (dir, repo, data, snap)
}

/// The rule itself: nothing below the marks; the prune for 64 MiB more loose objects, for 6,700 more loose objects and for a disk
/// that is nearly full; the prune and the repack for 64 MiB more of packs.
#[test]
fn b860_tidy_waits_for_the_watermark_the_rule() {
    let roomy = 8 * TIDY_FREE_BYTES;
    let after_last = Loose {
        count: 40,
        size_kib: 2_000,
        pack_kib: 500,
    };
    assert_eq!(
        (TIDY_AFTER_KIB, TIDY_AFTER_OBJECTS, TIDY_FREE_BYTES),
        (65_536, 6_700, 1 << 30)
    );
    // Nothing since the last clean-up, and just under each mark.
    assert_eq!(tidy_needed(after_last, after_last, roomy), Tidy::No);
    let under = Loose {
        count: after_last.count + TIDY_AFTER_OBJECTS - 1,
        size_kib: after_last.size_kib + TIDY_AFTER_KIB - 1,
        pack_kib: after_last.pack_kib + TIDY_AFTER_KIB - 1,
    };
    assert_eq!(tidy_needed(under, after_last, roomy), Tidy::No);
    assert_eq!(tidy_needed(under, after_last, TIDY_FREE_BYTES), Tidy::No);
    // A repository that was never cleaned counts from nothing.
    assert_eq!(
        tidy_needed(Loose::default(), Loose::default(), roomy),
        Tidy::No
    );
    // Each mark alone.
    let more_bytes = Loose {
        size_kib: after_last.size_kib + TIDY_AFTER_KIB,
        ..after_last
    };
    assert_eq!(tidy_needed(more_bytes, after_last, roomy), Tidy::Prune);
    let more_objects = Loose {
        count: after_last.count + TIDY_AFTER_OBJECTS,
        ..after_last
    };
    assert_eq!(tidy_needed(more_objects, after_last, roomy), Tidy::Prune);
    let more_pack = Loose {
        pack_kib: after_last.pack_kib + TIDY_AFTER_KIB,
        ..after_last
    };
    assert_eq!(tidy_needed(more_pack, after_last, roomy), Tidy::PruneRepack);
    // A disk that is nearly full asks for the prune whatever the repository holds.
    assert_eq!(
        tidy_needed(after_last, after_last, TIDY_FREE_BYTES - 1),
        Tidy::Prune
    );
    // What git says of a repository.
    let said = "count: 12\nsize: 340\nin-pack: 7\npacks: 1\nsize-pack: 9000\nprune-packable: 0\ngarbage: 0\nsize-garbage: 0\n";
    assert_eq!(
        Loose::parse(said),
        Loose {
            count: 12,
            size_kib: 340,
            pack_kib: 9_000
        }
    );
}

/// With git: ten looks at a small file that is rewritten before each run no clean-up, so the repository still holds every version.
/// After that, eleven looks at a file of 8 MiB that is rewritten before each: the clean-up ran once the repository had grown by
/// 64 MiB, so it holds well under what the looks wrote, and the last copy still clones to the last state. Before, each of these looks
/// started with a `git prune`.
#[tokio::test]
async fn b860_tidy_waits_for_the_watermark() {
    let (dir, repo, data, mut snap) = setup("b860-tidy").await;
    let git_dir = data.join("snapshot.git");
    for n in 0..10_u64 {
        std::fs::write(repo.join("notes.txt"), format!("round {n}\n")).unwrap();
        snap.snapshot().await.unwrap().expect("the tree changed");
    }
    assert_eq!(snap.take_tidy_error(), None);
    // Each look left a blob, a tree and a commit.
    let small = loose_objects(&git_dir);
    assert!(
        small >= 30,
        "TRIGGER PRESENT: after ten looks at a rewritten small file the private repository holds {small} loose objects: a clean-up \
         ran before the repository had grown by a mark"
    );

    // 88 MiB of content that does not compress, 8 MiB before each look.
    let piece = 8 * MIB as usize;
    let mut last = None;
    for n in 0..11_u64 {
        std::fs::write(repo.join("data.bin"), noise(n, piece)).unwrap();
        last = Some(snap.snapshot().await.unwrap().expect("the tree changed"));
    }
    assert_eq!(snap.take_tidy_error(), None);
    let held = bytes_under(&git_dir.join("objects"));
    assert!(
        held < 48 * MIB,
        "after 88 MiB of rewritten content the private repository holds {held} bytes of objects: no clean-up ran at the mark of \
         64 MiB"
    );
    assert!(
        held >= piece as u64,
        "the repository holds what its last copy needs: {held} bytes"
    );
    assert!(
        loose_objects(&git_dir) < small,
        "the clean-up removed the versions of the small file too"
    );
    // The copy still holds the work.
    let clone = dir.join("clone");
    git(
        &dir,
        &[
            "clone",
            "-q",
            "-b",
            "snapshot",
            last.unwrap().bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read(clone.join("data.bin")).unwrap(),
        noise(10, piece)
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("notes.txt")).unwrap(),
        "round 9\n"
    );
}

/// The three other private repositories of a session read the same tree: the goal loop's fingerprint (`goal.git`), the probe of the
/// nudge and the review round (`probe.git`) and the facts of a stop (`facts.git`). Each reading of a changed tree left the new
/// versions of the changed files there, and nothing ever removed them: a run that rewrites a large file between its rounds filled
/// the worker's disk. With every clean-up run at once, five readings of a file that is rewritten before each leave the one version
/// the index names, and each reading still answers.
#[tokio::test]
async fn b860_other_private_repositories_are_pruned() {
    let dir = scratch("b860-tidy-others");
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let goal = TreeFp::open(&data, &repo, &[])
        .await
        .unwrap()
        .with_tidy_marks(TidyMarks::AT_ONCE);
    let probe = GitProbe::open(&data, &repo)
        .await
        .with_tidy_marks(TidyMarks::AT_ONCE);
    let facts = GitSource::open(&data, &repo, &base)
        .await
        .unwrap()
        .with_tidy_marks(TidyMarks::AT_ONCE);
    let mut goal_seen = Vec::new();
    let mut probe_seen = Vec::new();
    for n in 0..5_u64 {
        std::fs::write(repo.join("data.bin"), noise(n, 64 * 1024)).unwrap();
        goal_seen.push(goal.fingerprint().await.expect("a fingerprint"));
        let fp = probe.fingerprint().await;
        assert!(!fp.starts_with("error-"), "{fp}");
        probe_seen.push(fp);
        let changed = facts.changed_files(10).await.expect("the changed files");
        assert_eq!(
            (changed.paths, changed.total),
            (vec!["data.bin".to_string()], 1)
        );
        let all = facts.facts().await;
        assert_eq!(all.errors, Vec::<String>::new());
        assert_eq!(all.changed_files, vec!["data.bin".to_string()]);
    }
    // Every reading saw its own version of the file.
    for seen in [&mut goal_seen, &mut probe_seen] {
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 5, "five readings, five trees");
    }
    for name in ["goal.git", "probe.git", "facts.git"] {
        let left = loose_objects(&data.join(name));
        assert!(
            left <= 2,
            "TRIGGER PRESENT: after five readings of a rewritten file {name} holds {left} loose objects: the versions of earlier \
             readings stay"
        );
    }
}

/// The probe's readings share one index, and a clean-up must not run beside the `git add` of another reading: a prune can remove an
/// object that add has just written. With a clean-up after each new object, twenty readings from two tasks at once, the file
/// rewritten before each, all answer with a fingerprint. Before, the probe had no lock: two readings at once met on the index's lock
/// file, and one answered with an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn b860_probe_readings_do_not_overlap_a_prune() {
    let dir = scratch("b860-tidy-probe");
    let repo = python_calc(&dir);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let probe = Arc::new(
        GitProbe::open(&data, &repo)
            .await
            .with_tidy_marks(TidyMarks {
                kib: u64::MAX / 4,
                objects: 1,
                free_bytes: 0,
            }),
    );
    let mut tasks = Vec::new();
    for task in 0..2_u64 {
        let probe = probe.clone();
        let file = repo.join(format!("data-{task}.bin"));
        tasks.push(tokio::spawn(async move {
            let mut seen = Vec::new();
            for n in 0..10_u64 {
                std::fs::write(&file, noise(task * 100 + n, 16 * 1024)).unwrap();
                seen.push(probe.fingerprint().await);
            }
            seen
        }));
    }
    let mut errors = Vec::new();
    let mut readings = 0;
    for task in tasks {
        for fp in task.await.unwrap() {
            readings += 1;
            if fp.starts_with("error-") {
                errors.push(fp);
            }
        }
    }
    assert_eq!(readings, 20);
    assert_eq!(
        errors,
        Vec::<String>::new(),
        "TRIGGER PRESENT: readings of the probe that ran at once answered with an error"
    );
    assert!(
        loose_objects(&data.join("probe.git")) <= 4,
        "the clean-up ran: {} loose objects",
        loose_objects(&data.join("probe.git"))
    );
}

// ---- a git that logs each prune, and a clean-up that does not end ------------------------------------------------------------------

/// A `git` for the front of the search path that writes one line for each `prune` it is started with, the repository of the call,
/// into `dir/prunes.log`. While the file `dir/prune-waits` is there, a `prune` in `goal.git`, `probe.git` or `facts.git` also writes
/// its process id into `dir/prunes-held` and then waits ten minutes in place of pruning. Every other call is the real git. The answer
/// is the folder that holds it.
fn git_that_logs_prunes(dir: &Path) -> PathBuf {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "git on the search path: {real:?}");
    let bin = dir.join("prune-git-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = format!(
        r#"#!/bin/sh
prune=0
for a in "$@"; do if [ "$a" = prune ]; then prune=1; fi; done
if [ "$prune" = 1 ]; then
  echo "$GIT_DIR" >> '{log}'
  if [ -e '{waits}' ]; then
    case "$GIT_DIR" in
      */goal.git|*/probe.git|*/facts.git)
        echo "$$" >> '{held}'
        exec sleep 600 ;;
    esac
  fi
fi
exec '{real}' "$@"
"#,
        log = dir.join("prunes.log").display(),
        waits = dir.join("prune-waits").display(),
        held = dir.join("prunes-held").display(),
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// How many times `git prune` was started in the private repository `name` ([`git_that_logs_prunes`]).
fn prunes(dir: &Path, name: &str) -> usize {
    std::fs::read_to_string(dir.join("prunes.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.trim_end().ends_with(name))
        .count()
}

/// The process ids of the prunes that were made to wait ([`git_that_logs_prunes`]).
fn prunes_held(dir: &Path) -> Vec<u32> {
    std::fs::read_to_string(dir.join("prunes-held"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Whether the process `pid` runs (a process that has ended and waits to be collected does not).
fn runs(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        // `<pid> (<name>) <state> ...`: the state follows the last `)`.
        stat.rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X")
    })
}

/// Starts this test binary once more for the test `name`, with the git of [`git_that_logs_prunes`] first on the search path and
/// the scratch folder `dir` named in the variable `var`, and returns what that process printed. A prune of the test that still waits
/// afterwards is ended here, whatever the product did, so no run of the test leaves a process behind.
fn in_a_process_with_that_git(dir: &Path, name: &str, var: &str) -> std::process::Output {
    let bin = git_that_logs_prunes(dir);
    let search = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env("PATH", search)
        .env(var, dir)
        .output()
        .unwrap();
    for pid in prunes_held(dir) {
        if runs(pid) {
            let _ = std::process::Command::new("kill")
                .args(["-s", "KILL", &pid.to_string()])
                .status();
        }
    }
    out
}

/// The variable that makes [`b860_a_clean_up_that_runs_long_fails_no_reading`] the process that reads: it names the test's scratch
/// folder.
const SLOW_DIR: &str = "B860_SLOW_TIDY_DIR";
/// The limit a clean-up after a reading has in that test.
const SLOW_LIMIT: Duration = Duration::from_millis(300);

/// The readings of [`b860_a_clean_up_that_runs_long_fails_no_reading`], in the process whose search path leads with the git whose
/// prune waits.
async fn slow_tidy_readings(dir: &Path) {
    let repo = python_calc(dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let goal = TreeFp::open(&data, &repo, &[])
        .await
        .unwrap()
        .with_tidy_marks(TidyMarks::AT_ONCE)
        .with_tidy_limit(SLOW_LIMIT);
    let probe = GitProbe::open(&data, &repo)
        .await
        .with_tidy_marks(TidyMarks::AT_ONCE)
        .with_tidy_limit(SLOW_LIMIT);
    let facts = GitSource::open(&data, &repo, &base)
        .await
        .unwrap()
        .with_tidy_marks(TidyMarks::AT_ONCE)
        .with_tidy_limit(SLOW_LIMIT);
    std::fs::write(dir.join("prune-waits"), "set\n").unwrap();
    std::fs::write(repo.join("data.bin"), noise(1, 64 * 1024)).unwrap();

    // goal.git: the fingerprint is made before its clean-up starts. A clean-up that does not end must not turn it into none, and
    // must not hold the goal loop for the 60 s a fingerprint has.
    let began = Instant::now();
    let fp = tokio::time::timeout(Duration::from_secs(120), goal.fingerprint())
        .await
        .expect("the fingerprint came back within the test's limit");
    let took = began.elapsed();
    assert_eq!(
        prunes(dir, "goal.git"),
        1,
        "TRIGGER PRESENT is not shown: the clean-up of goal.git did not start its prune"
    );
    assert!(
        fp.is_some(),
        "TRIGGER PRESENT: the fingerprint of the goal loop was made, its clean-up did not end, and after {took:?} the fingerprint \
         came back as none"
    );
    assert!(
        took < Duration::from_secs(30),
        "the clean-up of goal.git has a limit of {SLOW_LIMIT:?} here; the fingerprint came back after {took:?}"
    );

    // probe.git and facts.git: their readings come back too, with what they read.
    let fp = tokio::time::timeout(Duration::from_secs(30), probe.fingerprint())
        .await
        .expect("TRIGGER PRESENT: the reading of probe.git waited more than 30 s for a clean-up that does not end");
    assert!(!fp.starts_with("error-"), "{fp}");
    assert_eq!(prunes(dir, "probe.git"), 1);
    let changed = tokio::time::timeout(Duration::from_secs(30), facts.changed_files(10))
        .await
        .expect("TRIGGER PRESENT: the reading of facts.git waited more than 30 s for a clean-up that does not end")
        .expect("the changed files");
    assert_eq!(changed.paths, vec!["data.bin".to_string()]);
    assert_eq!(prunes(dir, "facts.git"), 1);

    // A clean-up that was given up ended its git.
    assert_eq!(prunes_held(dir).len(), 3, "three prunes were made to wait");
    let ended = Instant::now();
    while prunes_held(dir).iter().any(|pid| runs(*pid)) && ended.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let left: Vec<u32> = prunes_held(dir)
        .into_iter()
        .filter(|pid| runs(*pid))
        .collect();
    assert_eq!(
        left,
        Vec::<u32>::new(),
        "TRIGGER PRESENT: a prune whose clean-up was given up still runs"
    );

    // The clean-up after the next reading goes through, and leaves each repository as a clean-up leaves it.
    std::fs::remove_file(dir.join("prune-waits")).unwrap();
    std::fs::write(repo.join("data.bin"), noise(2, 64 * 1024)).unwrap();
    assert!(goal.fingerprint().await.is_some());
    assert!(!probe.fingerprint().await.starts_with("error-"));
    assert!(facts.changed_files(10).await.is_some());
    for name in ["goal.git", "probe.git", "facts.git"] {
        assert_eq!(prunes(dir, name), 2, "{name}: a second clean-up ran");
        let left = loose_objects(&data.join(name));
        assert!(
            left <= 2,
            "after a clean-up that went through {name} holds {left} loose objects"
        );
    }
    std::fs::write(dir.join("readings-done"), "done\n").unwrap();
}

/// The clean-up of `goal.git`, `probe.git` and `facts.git` follows a reading that is made already, so it must fail no reading and
/// hold none for long. Here `git prune` does not end in any of the three. Before, the clean-up of `goal.git` ran inside the 60 s a
/// fingerprint of the goal loop has: the fingerprint that was made came back as none after those 60 s, and the goal loop decided on
/// "unknown". The clean-ups of `probe.git` and `facts.git` had no limit at all. Now the clean-up runs after the timed reading, under
/// the same lock, with a limit of its own (`snapshot::TIDY_LIMIT`, set to 300 ms here): each reading comes back with what it read,
/// the git of a clean-up that was given up is ended, and the clean-up after the next reading goes through.
#[tokio::test]
async fn b860_a_clean_up_that_runs_long_fails_no_reading() {
    if let Some(dir) = std::env::var_os(SLOW_DIR) {
        return slow_tidy_readings(Path::new(&dir)).await;
    }
    let dir = scratch("b860-tidy-slow");
    let out = in_a_process_with_that_git(
        &dir,
        "b860_a_clean_up_that_runs_long_fails_no_reading",
        SLOW_DIR,
    );
    assert!(
        out.status.success(),
        "the readings beside a clean-up that does not end:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dir.join("readings-done").exists(),
        "the process that reads made its readings"
    );
}

// ---- a disk that is nearly full ---------------------------------------------------------------------------------------------------

/// The variable that makes [`b860_a_nearly_full_disk_cleans_up_only_what_grew`] the process that reads: it names the test's scratch
/// folder.
const LOW_DIR: &str = "B860_LOW_DISK_DIR";

/// Marks that no growth of a small repository reaches, with a disk that counts as nearly full while it has fewer than `free_bytes`
/// bytes free: only the disk asks for a clean-up.
fn disk_marks(free_bytes: u64) -> TidyMarks {
    TidyMarks {
        kib: u64::MAX / 4,
        objects: u64::MAX / 4,
        free_bytes,
    }
}

/// One reading of the probe, which must answer with a fingerprint.
async fn reads(probe: &GitProbe) {
    let fp = probe.fingerprint().await;
    assert!(!fp.starts_with("error-"), "{fp}");
}

/// The readings of [`b860_a_nearly_full_disk_cleans_up_only_what_grew`], in the process whose search path leads with the git that
/// logs each prune.
async fn low_disk_readings(dir: &Path) {
    let repo = python_calc(dir);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    // Every disk has fewer free bytes than this mark: the disk of the test counts as nearly full.
    let probe = GitProbe::open(&data, &repo)
        .await
        .with_tidy_marks(disk_marks(u64::MAX));
    std::fs::write(repo.join("data.bin"), noise(1, 64 * 1024)).unwrap();
    reads(&probe).await;
    assert_eq!(
        prunes(dir, "probe.git"),
        1,
        "a nearly full disk asks for the clean-up after a reading that added to the repository"
    );
    // The tree does not change: the repository holds what it held after that clean-up, and another one would free nothing.
    for _ in 0..3 {
        reads(&probe).await;
    }
    assert_eq!(
        prunes(dir, "probe.git"),
        1,
        "TRIGGER PRESENT: on a nearly full disk the clean-up ran again after readings that added nothing to the repository"
    );
    // The next reading that adds to the repository is followed by a clean-up, which removes the version before it.
    std::fs::write(repo.join("data.bin"), noise(2, 64 * 1024)).unwrap();
    reads(&probe).await;
    assert_eq!(prunes(dir, "probe.git"), 2);
    assert!(
        loose_objects(&data.join("probe.git")) <= 2,
        "the clean-up removed the earlier version: {} loose objects",
        loose_objects(&data.join("probe.git"))
    );
    // The disk has room again: no disk has fewer than 0 bytes free, and nothing else asks for a clean-up.
    let probe = probe.with_tidy_marks(disk_marks(0));
    std::fs::write(repo.join("data.bin"), noise(3, 64 * 1024)).unwrap();
    reads(&probe).await;
    reads(&probe).await;
    assert_eq!(
        prunes(dir, "probe.git"),
        2,
        "a disk with room asks for no clean-up"
    );
    std::fs::write(dir.join("readings-done"), "done\n").unwrap();
}

/// A disk with less than 1 GiB free asks for the clean-up whatever the repository holds. Before, that clean-up then ran after every
/// reading of the three other private repositories, and at every look of the copy task after a changed one, also when the repository
/// held exactly what the last clean-up had left: one `git prune`, and its walk over what the repository can reach, for nothing. And
/// nothing in the worker's log said that this was on. Now a clean-up that only the disk asks for runs when the repository holds
/// something else than after its last clean-up, and the log says once when the free-space mark starts to force clean-ups and once
/// when it stops.
#[tokio::test]
async fn b860_a_nearly_full_disk_cleans_up_only_what_grew() {
    if let Some(dir) = std::env::var_os(LOW_DIR) {
        return low_disk_readings(Path::new(&dir)).await;
    }
    let dir = scratch("b860-tidy-disk");
    let out = in_a_process_with_that_git(
        &dir,
        "b860_a_nearly_full_disk_cleans_up_only_what_grew",
        LOW_DIR,
    );
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "the readings on a nearly full disk:\n{}\n{said}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        dir.join("readings-done").exists(),
        "the process that reads made its readings"
    );
    let starts = said
        .lines()
        .filter(|l| l.contains("probe.git: its disk has less than"))
        .count();
    let stops = said
        .lines()
        .filter(|l| l.contains("probe.git: its disk has room again"))
        .count();
    assert_eq!(
        (starts, stops),
        (1, 1),
        "TRIGGER PRESENT: the worker's log says once that a nearly full disk forces clean-ups and once that it stopped:\n{said}"
    );
    assert!(
        said.find("its disk has less than") < said.find("its disk has room again"),
        "the start is said before the stop:\n{said}"
    );
}
