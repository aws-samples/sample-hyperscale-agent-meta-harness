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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// of a reading of [`reading_that_hangs`] waits on a pipe that nothing ever writes to: one that the product did not end
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

/// Watches the processes this test started for the `git add` of a first reading in the folder of a test: the git that reads the
/// tree, and the one that waits on the pipe of [`reading_that_hangs`]. A thread of the test looks at `/proc` every two
/// milliseconds until it sees that git or the watch is ended. What it saw is the test's own evidence of where the limit of a
/// reading found it, beside what the reading says of itself: the reading makes a file call on the work tree before it starts
/// its git, and a limit that comes during that call finds no git to end.
struct ReadingsGit {
    ended: Arc<AtomicBool>,
    seen: Arc<AtomicBool>,
    watch: Option<std::thread::JoinHandle<()>>,
}

impl ReadingsGit {
    fn watch(folder: &Path) -> Self {
        let ended = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(AtomicBool::new(false));
        let folder = folder.to_string_lossy().to_string();
        let watch = {
            let (ended, seen) = (ended.clone(), seen.clone());
            std::thread::spawn(move || {
                while !ended.load(Ordering::SeqCst) {
                    if git_children().iter().any(|pid| is_an_add_in(*pid, &folder)) {
                        seen.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        Self {
            ended,
            seen,
            watch: Some(watch),
        }
    }

    /// Ends the watch: whether the `git add` of the reading was seen running.
    fn seen(mut self) -> bool {
        self.ended.store(true, Ordering::SeqCst);
        if let Some(watch) = self.watch.take() {
            watch.join().unwrap();
        }
        self.seen.load(Ordering::SeqCst)
    }
}

/// Whether the process `pid` was started as a `git add` whose command line names `folder`.
fn is_an_add_in(pid: u32, folder: &str) -> bool {
    let started_with = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let line = String::from_utf8_lossy(&started_with);
    line.contains(folder) && line.split('\0').any(|word| word == "add")
}

/// What the making of the private repository may take beyond the limit of its first reading: 29.8 s. The reading of
/// [`b860_first_reading_that_hangs_is_given_up`] with its limit of 200 ms has the 30 s it always had.
const BEYOND_THE_LIMIT: Duration = Duration::from_millis(29_800);

/// The two things a first reading says when it was given up: a git of it was ended, or a file call it made on the work tree
/// had not returned.
fn is_a_give_up(note: &str) -> bool {
    note.contains("given up") || note.contains("the work tree did not answer")
}

/// What the limit of a first reading that was given up met, by the reading's own note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Met {
    /// A file call of the reading on the work tree that had not returned: the reading started no git after it.
    FileCall,
    /// A git of the reading, which was ended.
    Git,
}

/// What the note of a first reading says its limit met; `None` for a note that names no give-up.
fn met_by(note: &str) -> Option<Met> {
    if note.contains("the work tree did not answer") {
        Some(Met::FileCall)
    } else if note.contains("given up") {
        Some(Met::Git)
    } else {
        None
    }
}

/// The notes a reading leaves name what its limit met, and a note of a reading that went through names nothing.
#[test]
fn b948_the_note_of_a_reading_names_what_its_limit_met() {
    assert_eq!(
        met_by("snapshot: the work tree did not answer; its ignore rules was not read"),
        Some(Met::FileCall)
    );
    assert_eq!(
        met_by("snapshot: git: git add: given up before it had ended"),
        Some(Met::Git)
    );
    assert_eq!(met_by("snapshot: git: git add: exit status 128"), None);
    assert_eq!(met_by(""), None);
    for note in [
        "snapshot: the work tree did not answer; its ignore rules was not read",
        "snapshot: git: git add: given up before it had ended",
        "snapshot: git: git add: exit status 128",
    ] {
        assert_eq!(met_by(note).is_some(), is_a_give_up(note), "{note}");
    }
    assert!(
        the_watcher_saw_no_git("snapshot: git: git add: given up before it had ended")
            .contains("was late")
    );
    assert!(
        the_watcher_saw_no_git(
            "snapshot: the work tree did not answer; its ignore rules was not read"
        )
        .contains("before its git was started")
    );
}

/// What a reading's note says about a `git add` that the test's watcher did not see running ([`ReadingsGit`]): the
/// reading started none, because its limit came during its file call on the work tree; or it started one and ended it, and
/// the watcher, which looks every two milliseconds, was late.
fn the_watcher_saw_no_git(note: &str) -> &'static str {
    match met_by(note) {
        Some(Met::FileCall) => {
            "By that note the limit came during the reading's file call on the work tree, before its git was started"
        }
        Some(Met::Git) => {
            "By that note the git of the reading ran and was given up: the watcher, which looks every two milliseconds, was late"
        }
        None => "That note names no give-up",
    }
}

/// What came of one first reading of a tree whose ignore file is a named pipe ([`reading_that_hangs`]).
struct HungReading {
    dir: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    pipe: PathBuf,
    copies: Snapshotter,
    /// Why the reading was given up, in its own words.
    note: String,
    /// How long the making of the private repository took, the reading included.
    took: Duration,
    /// Whether the `git add` of the reading was seen running before the making of the private repository returned.
    git_seen: bool,
    _no_git_is_left: EndGits,
}

/// One first reading, with the limit `limit`, of a tree that cannot be read to its end: the ignore file of the tree is a named
/// pipe, git opens it to read the rules and waits for a writer that never comes. The private repository is made all the same,
/// and the reading leaves a note that says why it was given up.
async fn reading_that_hangs(label: &str, limit: Duration) -> HungReading {
    let dir = scratch(label);
    let no_git_is_left = EndGits(dir.clone());
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
    let watch = ReadingsGit::watch(&dir);
    let began = Instant::now();
    let mut copies = tokio::time::timeout(
        Duration::from_millis(59_800) + limit,
        Snapshotter::new_primed(&data, &repo, &base, Some(limit)),
    )
    .await
    .expect("the private repository was made within the test's limit")
    .expect("a reading that is given up fails nothing");
    let took = began.elapsed();
    let git_seen = watch.seen();
    let note = copies.take_prime_note().expect("a reading was made");
    let note = note.expect_err(
        "TRIGGER PRESENT is not shown: git read the tree and did not wait for the pipe",
    );
    HungReading {
        dir,
        repo,
        data,
        pipe,
        copies,
        note,
        took,
        git_seen,
        _no_git_is_left: no_git_is_left,
    }
}

impl HungReading {
    /// The reading was given up at its limit, its git is gone, and it left no lock.
    async fn left_nothing_behind(&self, limit: Duration) {
        assert!(
            self.took < limit + BEYOND_THE_LIMIT,
            "the reading was given up at its limit of {limit:?}: {:?}",
            self.took
        );
        let ended = Instant::now();
        while !git_children().is_empty() && ended.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            git_children(),
            Vec::<u32>::new(),
            "a git of the reading still runs"
        );
        assert!(!self.data.join("snapshot.git/index.lock").exists());
    }

    /// The next look works once the tree can be read. `wait_for_the_file_call`: the reading was given up while a file call
    /// of it on the work tree had not returned. The private repository makes one such call at a time and refuses the next
    /// one until the outstanding one has returned, so the look is asked again, for two minutes at the most, while it is
    /// refused for that reason alone.
    async fn the_next_look_works(mut self, wait_for_the_file_call: bool) {
        std::fs::remove_file(&self.pipe).unwrap();
        std::fs::write(self.repo.join("notes.txt"), "after the pipe\n").unwrap();
        let asked = Instant::now();
        let made = loop {
            match self.copies.snapshot().await {
                Err(e)
                    if wait_for_the_file_call
                        && e.contains("has not answered an earlier call")
                        && asked.elapsed() < Duration::from_secs(120) =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                looked => {
                    break looked
                        .unwrap_or_else(|e| {
                            panic!("the tree takes a new reading after a first reading that was given up: {e}")
                        })
                        .expect("the tree changed");
                }
            }
        };
        let clone = self.dir.join("clone");
        git(
            &self.dir,
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
}

/// The limits [`b860_first_reading_that_hangs_is_given_up`] gives its reading in turn. The first is the limit the test always
/// had. A later one is used only when the limit before it came before the git of the reading ran.
const HANG_LIMITS: [Duration; 3] = [
    Duration::from_millis(200),
    Duration::from_secs(2),
    Duration::from_secs(20),
];

/// A first reading that does not return is given up at its limit, with its git ended, and fails nothing: the private repository is
/// made, the note says what happened, no lock file is left, and the next look works. The ignore file of the tree is a named pipe
/// here: git opens it to read the rules and waits for a writer that never comes.
///
/// The trigger is that the git of the reading runs when the limit comes, and the test sees it for itself ([`ReadingsGit`]). The
/// reading makes a file call on the work tree before it starts its git, so on a machine that is busy enough a limit of 200 ms
/// comes before there is a git to end: the reading then says that the work tree did not answer, which is right and is not what
/// this test is about. Such a reading is held to what is right for it (nothing failed, the note names a give-up, no git and no
/// lock are left), and the reading is made again in a new folder with the next limit of [`HANG_LIMITS`]. The reading in
/// which the git was seen running is held to everything: its note names the git that was given up. When no limit shows the
/// trigger, the test fails and says so.
///
/// The watcher can also miss a git that did run: it looks every two milliseconds, and on a crowded machine its thread can
/// come too late (at a load of about a hundred the reading's own note named the git that was given up in every such run).
/// The line the test prints for a reading whose git it did not see therefore says only that, with the reading's own note
/// and what that note means ([`the_watcher_saw_no_git`]); it does not say that the limit came too early. The reading is
/// made again with the next limit all the same.
#[tokio::test]
async fn b860_first_reading_that_hangs_is_given_up() {
    for limit in HANG_LIMITS {
        let reading = reading_that_hangs("b860-prime-hang", limit).await;
        if reading.git_seen {
            assert!(reading.note.contains("given up"), "{}", reading.note);
        } else {
            assert!(is_a_give_up(&reading.note), "{}", reading.note);
        }
        reading.left_nothing_behind(limit).await;
        if !reading.git_seen {
            println!(
                "TRIGGER ABSENT at a limit of {limit:?}: the test's watcher did not see the git of the reading running. The \
                 reading says: {}. {}. The reading is made again with the next limit",
                reading.note,
                the_watcher_saw_no_git(&reading.note)
            );
            continue;
        }
        println!(
            "TRIGGER PRESENT: the git of the reading was running when its limit of {limit:?} came: {}",
            reading.note
        );
        reading.the_next_look_works(false).await;
        return;
    }
    panic!(
        "TRIGGER PRESENT is not shown: with no limit of {HANG_LIMITS:?} was the git of the first reading seen running when the \
         limit came"
    );
}

/// A first reading with no time at all is given up at its start and fails nothing either. The limit has come before the
/// reading does anything. What it meets is one of two things. Its first file call on the work tree (the read of the
/// repository's ignore rules): the reading then says that the work tree did not answer. Or, when that call has returned by
/// the time the reading waits for it, the git the reading starts next, which is ended at once: the note names that git. A
/// timer of the runtime fires in whole milliseconds and the file call takes a fraction of one, so on a quiet machine it is
/// the git in most runs; on a crowded build machine [`b860_first_reading_that_hangs_is_given_up`] met the file call, with
/// its limit of 200 ms.
///
/// A test cannot choose which of the two a run meets. The limit is all a caller of the reading sets, and nothing a test
/// puts into the tree makes the file call slow: it reads one regular file, opened without blocking and capped in size
/// (`read_regular_nofollow`). So the test takes the path its run meets, prints it (`PATH: ...`, by the reading's own note,
/// [`met_by`]) and holds the reading, in every run, to all that is right for that path:
/// - its file call: the note names the ignore rules that were not read; no `git add` of the reading was seen, because none
///   was started; no git and no lock file are left; and the tree takes a new reading afterwards: the look that follows is
///   refused only while the outstanding call has not returned, for two minutes at the most, and then works. A tree that
///   stays refused for good after a file call that outlived its give-up fails here, in every run that meets the file call.
/// - its git: the note names the `git add` that was given up; that git is gone and has left no lock file; and the look
///   that follows works at once, because no file call is outstanding.
///
/// The test's watcher is not asked on the path of the git: a git that is ended at once lives a shorter time than the two
/// milliseconds between the watcher's looks, and in 1,200 runs it was seen in none.
#[tokio::test]
async fn b860_first_reading_with_no_time_at_all_is_given_up_and_fails_nothing() {
    let reading = reading_that_hangs("b860-prime-early", Duration::ZERO).await;
    match met_by(&reading.note) {
        Some(Met::FileCall) => {
            println!(
                "PATH: the reading with no time at all met its file call: {}",
                reading.note
            );
            assert!(
                reading.note.contains("its ignore rules"),
                "the first file call of a reading is the read of the ignore rules: {}",
                reading.note
            );
            assert!(
                !reading.git_seen,
                "a reading that is given up in its file call starts no git, and a `git add` of it was seen running: {}",
                reading.note
            );
            reading.left_nothing_behind(Duration::ZERO).await;
            reading.the_next_look_works(true).await;
        }
        Some(Met::Git) => {
            println!(
                "PATH: the reading with no time at all met its git: {}",
                reading.note
            );
            assert!(
                reading.note.contains("git add"),
                "the git a reading starts first is its `git add`: {}",
                reading.note
            );
            reading.left_nothing_behind(Duration::ZERO).await;
            reading.the_next_look_works(false).await;
        }
        None => panic!(
            "the note of a reading with no time at all names a give-up: {}",
            reading.note
        ),
    }
}
