//! The first reading of the tree in a session is made before the engine starts.
//!
//! The private repository of the copies starts each session with an index that `git read-tree` filled from the base: it names every
//! file and knows the state of none. So the first look of the copy task read and hashed every file of the tree (3.65 s of git on the
//! large repository of the measurement), and with no look made yet the first tool call that ended started that look at once, right in
//! front of the engine's next tool call. The tree is now read once when the private repository is made, before the engine starts,
//! where it competes with nothing; the first look of the copy task then costs what a look at an unchanged tree costs.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::snapshot::{PRIME_LIMIT, Snapshotter};
use common::{BASE_CALC, git, python_calc, scratch};
use direct_support::{Direct, Opts};
use harness_protocol::runtime::TurnOutcome;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A repository of `files` small files in folders of a hundred, committed once.
fn generated(dir: &Path, files: usize) -> PathBuf {
    let repo = dir.join("generated");
    for n in 0..files {
        let folder = repo.join(format!("pkg{:02}", n / 100));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(format!("file{n:04}.txt")),
            format!("line one of file {n}\nline two\n"),
        )
        .unwrap();
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    repo
}

/// The entries of the private index of the copies for the work tree at `repo`, and how many of them carry the time their file was
/// changed (`git ls-files --debug`): an entry without it makes git read and hash the file at the next look.
fn index_entries(data: &Path, repo: &Path) -> (usize, usize) {
    let out = std::process::Command::new("git")
        .env("GIT_DIR", data.join("snapshot.git"))
        .env("GIT_WORK_TREE", repo)
        .current_dir(repo)
        .args(["ls-files", "--debug"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let times: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("mtime: "))
        .collect();
    let known = times.iter().filter(|t| t.trim() != "0:0").count();
    (times.len(), known)
}

/// After the private repository is made, its index knows the state of the files: at least 99 of 100 entries carry the time their file
/// was changed. Before, none did, so the first look read and hashed every file. And in a whole turn the reading is logged before the
/// engine starts.
#[tokio::test(flavor = "multi_thread")]
async fn b860_first_reading_is_made_before_the_engine_starts() {
    let dir = scratch("b860-prime");
    let repo = generated(&dir, 2_000);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let _copies = Snapshotter::new(&data, &repo, &base).await.unwrap();
    let (entries, known) = index_entries(&data, &repo);
    assert_eq!(entries, 2_000, "the index names every file of the base");
    assert!(
        known * 100 >= entries * 99,
        "TRIGGER PRESENT: {known} of {entries} entries of the private index know the state of their file: the first look of the \
         session reads and hashes the others"
    );

    // A whole turn: the reading is made, and logged, before the engine starts.
    let d = Direct::start_process(Opts::default()).await;
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let log = d.process.as_ref().unwrap().log_text();
    let read = log.find("direct turn copies: the first reading of the tree took");
    let engine = log.find("direct turn engine starting");
    assert!(
        matches!((read, engine), (Some(read), Some(engine)) if read < engine),
        "TRIGGER PRESENT: the first reading of the tree is not logged before the engine starts; log:\n{log}"
    );
}

/// The first look of a session on a new copy of the small repository, with the first reading (`prime`) or without it: `change` is
/// what happens to the tree between the making of the private repository and the look (it gets the work tree and agentd's data
/// folder). The answer is the tree of the copy the look made and the paths that tree holds; `None` when the look made no copy.
async fn first_look(
    label: &str,
    prime: bool,
    change: &dyn Fn(&Path, &Path),
) -> Option<(String, String)> {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let mut copies = Snapshotter::new_primed(&data, &repo, &base, prime.then_some(PRIME_LIMIT))
        .await
        .unwrap();
    match copies.take_prime_note() {
        Some(read) => {
            assert!(prime, "a reading was made without being asked for");
            read.expect("the first reading went through");
        }
        None => assert!(!prime, "no reading was made"),
    }
    change(&repo, &data);
    let made = copies.snapshot().await.unwrap()?;
    let git_dir = data.join("snapshot.git");
    let git_dir = git_dir.to_str().unwrap();
    let tree = git(
        &repo,
        &[
            "--git-dir",
            git_dir,
            "rev-parse",
            &format!("{}^{{tree}}", made.commit),
        ],
    );
    let paths = git(
        &repo,
        &["--git-dir", git_dir, "ls-tree", "-r", "--name-only", &tree],
    );
    Some((tree, paths))
}

/// The reading changes no decision of the first look. With it and without it the look makes no copy of an unchanged tree; it makes
/// the same copy when a file changed after the private repository was made, also when the file was rewritten with the same size and
/// carries the time of the index file itself (twelve rounds, the time set by hand, so that no round depends on the speed of the
/// host); and a file that the base tracks and an ignore rule now matches stays in the copy.
#[tokio::test]
async fn b860_first_reading_changes_no_decision() {
    // An unchanged tree.
    for prime in [true, false] {
        assert_eq!(
            first_look("b860-same", prime, &|_, _| {}).await,
            None,
            "an unchanged tree gets no copy (first reading: {prime})"
        );
    }

    // A file that changed.
    let edit = |repo: &Path, _: &Path| {
        std::fs::write(
            repo.join("src/calc.py"),
            "def add(a, b):\n    return b + a\n",
        )
        .unwrap();
    };
    let with = first_look("b860-edit", true, &edit).await;
    let without = first_look("b860-edit", false, &edit).await;
    assert!(with.is_some(), "the changed file is copied");
    assert_eq!(with, without);

    // A file rewritten with the same size that carries the time of the index file: git cannot tell it from its entry by size or
    // time, and reads it.
    for round in 0..12_u8 {
        let rewrite = move |repo: &Path, data: &Path| {
            let mut text = BASE_CALC.as_bytes().to_vec();
            let last = text.len() - 2;
            text[last] = b'c' + round;
            assert_eq!(text.len(), BASE_CALC.len());
            let path = repo.join("src/calc.py");
            std::fs::write(&path, &text).unwrap();
            let index_time = std::fs::metadata(data.join("snapshot.git/index"))
                .unwrap()
                .modified()
                .unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(index_time)
                .unwrap();
        };
        let with = first_look("b860-racy", true, &rewrite).await;
        let without = first_look("b860-racy", false, &rewrite).await;
        assert!(
            with.is_some(),
            "round {round}: a file rewritten with the same size and the time of the index is copied"
        );
        assert_eq!(with, without, "round {round}");
    }

    // A file the base tracks that an ignore rule matches now stays in the copy.
    let ignore = |repo: &Path, _: &Path| {
        std::fs::write(repo.join(".git/info/exclude"), "src/\n").unwrap();
        std::fs::write(repo.join("notes.txt"), "a note\n").unwrap();
    };
    let with = first_look("b860-ignored", true, &ignore).await;
    let without = first_look("b860-ignored", false, &ignore).await;
    let (_, paths) = with.clone().expect("the new file is copied");
    assert!(
        paths.lines().any(|p| p == "src/calc.py") && paths.lines().any(|p| p == "notes.txt"),
        "the copy holds the file the base tracks and the new file:\n{paths}"
    );
    assert_eq!(with, without);
}

/// The processes named `git` that this process started and that still run.
fn git_children() -> Vec<u32> {
    let me = std::process::id();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // `<pid> (<name>) <state> <parent> ...`
            let (head, rest) = stat.rsplit_once(')')?;
            let name = head.split_once('(')?.1;
            let mut fields = rest.split_whitespace();
            let state = fields.next()?;
            let parent: u32 = fields.next()?.parse().ok()?;
            (parent == me && name == "git" && state != "Z").then_some(pid)
        })
        .collect()
}

/// Ends, when the test ends, the gits this process started in the folder `0` that still run, whatever way the test ends. The git
/// of [`b860_first_reading_that_hangs_is_given_up`] waits on a pipe that nothing ever writes to: one that the product did not end
/// would wait there for good, on every run of a test that fails for that very reason. Only the gits of that folder: under plain
/// `cargo test` other tests of this file run git in the same process.
struct EndGits(PathBuf);

impl Drop for EndGits {
    fn drop(&mut self) {
        let folder = self.0.to_string_lossy().to_string();
        for pid in git_children() {
            let started_with = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if String::from_utf8_lossy(&started_with).contains(&folder) {
                let _ = std::process::Command::new("kill")
                    .args(["-s", "KILL", &pid.to_string()])
                    .status();
            }
        }
    }
}

/// A first reading that does not return is given up at its limit, with its git ended, and fails nothing: the private repository is
/// made, the note says what happened, no lock file is left, and the next look works. The ignore file of the tree is a named pipe
/// here: git opens it to read the rules and waits for a writer that never comes.
#[tokio::test]
async fn b860_first_reading_that_hangs_is_given_up() {
    let dir = scratch("b860-prime-hang");
    let _no_git_is_left = EndGits(dir.clone());
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let pipe = repo.join(".gitignore");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .unwrap()
            .success()
    );
    let began = Instant::now();
    let mut copies = tokio::time::timeout(
        Duration::from_secs(60),
        Snapshotter::new_primed(&data, &repo, &base, Some(Duration::from_millis(200))),
    )
    .await
    .expect("the private repository was made within the test's limit")
    .expect("a reading that is given up fails nothing");
    let took = began.elapsed();
    let note = copies.take_prime_note().expect("a reading was made");
    let e = note.expect_err(
        "TRIGGER PRESENT is not shown: git read the tree and did not wait for the pipe",
    );
    assert!(e.contains("given up"), "{e}");
    assert!(
        took < Duration::from_secs(30),
        "the reading was given up at its limit of 200 ms: {took:?}"
    );
    // git is gone, and it left no lock.
    let ended = Instant::now();
    while !git_children().is_empty() && ended.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        git_children(),
        Vec::<u32>::new(),
        "a git of the reading still runs"
    );
    assert!(!data.join("snapshot.git/index.lock").exists());

    // The next look works once the tree can be read.
    std::fs::remove_file(&pipe).unwrap();
    std::fs::write(repo.join("notes.txt"), "after the pipe\n").unwrap();
    let made = copies.snapshot().await.unwrap().expect("the tree changed");
    let clone = dir.join("clone");
    git(
        &dir,
        &[
            "clone",
            "-q",
            "-b",
            "snapshot",
            made.bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("notes.txt")).unwrap(),
        "after the pipe\n"
    );
}
