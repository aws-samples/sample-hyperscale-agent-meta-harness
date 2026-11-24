//! When the copy task asks the file system of the work tree whether it answers, and the late limit of the look that follows
//! a look which a request to end overtook.
//!
//! Two points read in the code (`watched_look` and `start_snapshots` in `src/direct/mod.rs`) and two rows of the table in the header
//! of `tests/b823_stop_copy.rs` (the follow-up look's limit and when the question's arm is switched on), each held on the clock the
//! test drives. Each test asserts what the worker's texts say (the comment of `watched_look` and the copy task's bullets in the
//! worker's `AGENTS.md`), with its numbers written out. Those four were red on the worker as it was at b7c90f98edb0. Five more tests
//! hold the cases next to them: a late limit equal to the period of the question, the ticks while a look stores its copy, a tree
//! that does not answer during the look that follows a look which a request to end overtook, the late limit of the look that follows
//! such a look when it failed, and a late limit that comes after the git limit of the look.
//! The review of 13cb2fa3e asked for one more: the tie of a late limit equal to the period, made in twelve looks in a row, which a
//! worker that polled its arms in a random order would pass in one run of 4,096.
//! It asked for three more on that order (the end of the look, its late line, a request to end, the question): a look that ends
//! at the moment of its late limit and a tick, or of a tick alone, is not said late and does not ask its tree, and a request to
//! end that comes with a tick while a question is under way is taken up when the question has been answered.
//!
//! The helpers of the driven clock are copied from `tests/b823_stop_copy.rs` at b7c90f98edb0, unchanged but for two methods of
//! `HeldLook` and one constant of `Next` that these tests do not use, and for the doc of `HeldLook::is_said_late_at`, which says
//! when the worker asks the tree; each copy names its source lines. They are copied and not shared, so that the two
//! files change apart.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::snapshot::{STOP_COPY_BUDGET, TREE_ANSWER_LIMIT};
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::s3 as keys;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 909 to 911.
/// The variable that names the scratch folder of a test that drives the clock, in the process that test started for itself
/// ([`on_a_driven_clock`]).
const DRIVEN_DIR: &str = "B893_DRIVEN_DIR";

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 913 to 916.
/// The timer of the copy task in the tests that drive the clock. The spacing of the looks is capped at five of its periods
/// (`snapshot::COPY_SPACING_CAP`), and it is shorter than every duration these tests impose, so the tick of the look after a stored
/// look is due when that look ends.
const DRIVEN_TIMER: Duration = Duration::from_millis(10);

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 918 to 919.
/// The flusher of the worker posts the events of the run this often in these tests (the `events_every` of the test rig).
const DRIVEN_POSTS_EVERY: Duration = Duration::from_millis(100);

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 921 to 923.
/// How long a test that drives the clock waits, in real time and while the clock stands, for real work it knows to be under way: a
/// git, a request to the store, a post of the flusher. No moment of the late rule is measured against it.
const REAL_WORK_WITHIN: Duration = Duration::from_secs(60);

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 925 to 928.
/// How long past its late limit the tests of a look that is said late keep the look held, on the driven clock: the git work of
/// the look has this much more than the late limit, so the look is given up a minute and a half after it was said late. Driven
/// time costs no real time.
const HELD_PAST_THE_LIMIT: Duration = Duration::from_secs(90);

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 932 to 934.
/// The threads for blocking work in the runtime of a test that holds the worker's question to the tree
/// ([`on_a_driven_clock_with`], [`TreeQuestionHeld`]): few, so that the test can take every one of them.
const HELD_QUESTION_THREADS: usize = 4;

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 935 to 936.
/// How long, in real time, such a test keeps the question held while it looks for a late line that must not come.
const QUESTION_HELD_FOR: Duration = Duration::from_millis(300);

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 112 to 119.
/// Every version of `snapshot.json` in the bucket, oldest first: the fake store has them at once (the run's events reach the plane in batches).
fn records(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .versions_of(&keys::snapshot_json_key(RUN))
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
        .collect()
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 250 to 259.
/// The full path of the program `name` on the search path of the tests.
fn program(name: &str) -> String {
    let found = std::process::Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "{name} on the search path: {real:?}");
    real
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 261 to 265.
/// Writes the script `text` to `path` as a program.
fn program_file(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 282 to 285.
/// The file a stand-in writes when it lets its first `git add` through ([`hanging_git_behind_the_first_add`], [`waiting_git`]).
fn first_add_file(dir: &Path) -> PathBuf {
    dir.join("the-first-git-add-went-through")
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 379 to 392.
/// What one late line of the worker states: `direct turn snapshot late: <what> has taken <taken> s so far; its limit
/// was <limit> s (the set limit is <set> s, the last stored copy took <last> s)`, with `no copy was stored before` in the last place
/// when none was.
#[derive(Clone, Copy, Debug)]
struct SaidLate {
    /// How long the look had taken when the worker said it late.
    taken: Duration,
    /// The limit the worker applied to that look.
    limit: Duration,
    /// The late limit that was set for the worker.
    set: Duration,
    /// What the last look that stored a copy took when the limit was made; `None` when no look had stored one.
    last: Option<Duration>,
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 394 to 406.
/// A time as a late line prints it: seconds with three places.
fn stated_seconds(text: &str) -> Option<Duration> {
    let (whole, thousandths) = text.split_once('.')?;
    if thousandths.len() != 3 {
        return None;
    }
    let ms = whole
        .parse::<u64>()
        .ok()?
        .checked_mul(1_000)?
        .checked_add(thousandths.parse::<u64>().ok()?)?;
    Some(Duration::from_millis(ms))
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 408 to 425.
/// The numbers of one late line; `None` when the line does not state them.
fn said_late(line: &str) -> Option<SaidLate> {
    let (_, rest) = line.split_once(" has taken ")?;
    let (taken, rest) = rest.split_once(" s so far; its limit was ")?;
    let (limit, rest) = rest.split_once(" s (the set limit is ")?;
    let (set, rest) = rest.split_once(" s, ")?;
    let last = match rest.strip_prefix("the last stored copy took ") {
        Some(took) => Some(stated_seconds(took.strip_suffix(" s)")?)?),
        None if rest == "no copy was stored before)" => None,
        None => return None,
    };
    Some(SaidLate {
        taken: stated_seconds(taken)?,
        limit: stated_seconds(limit)?,
        set: stated_seconds(set)?,
        last,
    })
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1147 to 1155.
/// What a test sets up for the look after a look that stores a copy, while the upload of that copy is held
/// ([`HeldLook::stores_a_copy`]). The tree stops answering in every case: the mark of the stand-in is set again.
#[derive(Clone, Copy)]
struct Next {
    /// The tree gets a file that no copy has: the look after the stored look has something to store.
    changed: bool,
    /// The stand-in lets one more `git add` through ([`pass_files`]): the look after the stored look reads the tree.
    one_add_goes_through: bool,
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1157 to 1168 (less the members these tests do not use).
impl Next {
    /// The look after the stored look waits in `git add`, and the tree holds a file that no copy has.
    const HELD: Self = Self {
        changed: true,
        one_add_goes_through: false,
    };
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1170 to 1177.
/// The files of [`held_git`] in the scratch folder `dir`: the mark that makes `git add` wait, and the file the stand-in writes when
/// it waits.
fn held_git_files(dir: &Path) -> (PathBuf, PathBuf) {
    (
        dir.join("the-tree-does-not-answer"),
        dir.join("a-look-waits-in-git-add"),
    )
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1179 to 1186.
/// The files with which a test lets one `git add` of [`held_git`] through while the mark exists: the test writes the first, and
/// the stand-in gives it the second name when a `git add` took it.
fn pass_files(dir: &Path) -> (PathBuf, PathBuf) {
    (
        dir.join("one-git-add-goes-through"),
        dir.join("a-git-add-went-through"),
    )
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1188 to 1221.
/// A stand-in like [`waiting_git`] that also says when it waits: while the mark exists its first `git add` goes through (the
/// worker's reading of the tree before it starts the engine, [`first_add_file`]), and every later `git add` writes the file `held`
/// ([`held_git_files`]) and then waits until the mark is gone, for about a quarter of an hour at the most. A test can let one
/// of those later calls through ([`pass_files`]): the call that finds the first of the two files renames it and is the real git.
/// Returns the folder that must lead the search path.
fn held_git(dir: &Path) -> PathBuf {
    let real = program("git");
    let sleep = program("sleep");
    let rename = program("mv");
    let (pass, passed) = pass_files(dir);
    let bin = dir.join("slow-tree-bin");
    let wait = dir.join("slow-tree-wait");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&wait).unwrap();
    let (mark, held) = held_git_files(dir);
    let waiter = wait.join("sleep");
    let ten = "0 1 2 3 4 5 6 7 8 9";
    let script = format!(
        "#!/bin/sh\nfor a in {ten}; do for b in {ten}; do for c in {ten}; do for d in {ten}; do\nif [ -e '{mark}' ]; then '{sleep}' 0.1; else exec '{real}' \"$@\"; fi\ndone; done; done; done\nexec '{real}' \"$@\"\n",
        mark = mark.display()
    );
    program_file(&waiter, &script);
    let script = format!(
        "#!/bin/sh\nif [ -e '{mark}' ]; then for a in \"$@\"; do if [ \"$a\" = add ]; then if [ -e '{first}' ]; then if [ -e '{pass}' ]; then '{rename}' '{pass}' '{passed}'; exec '{real}' \"$@\"; fi; : > '{held}'; exec '{waiter}' \"$@\"; fi; : > '{first}'; fi; done; fi\nexec '{real}' \"$@\"\n",
        mark = mark.display(),
        first = first_add_file(dir).display(),
        pass = pass.display(),
        passed = passed.display(),
        held = held.display(),
        waiter = waiter.display()
    );
    program_file(&bin.join("git"), &script);
    bin
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1223 to 1232.
/// Runs `test` on a clock that the test drives. The worker's git calls take their search path from the environment of the process
/// they run in, so the test first starts this test binary once more for itself (`name`), with [`held_git`] first on the search
/// path and its scratch folder in [`DRIVEN_DIR`], and requires that this copy passed. The copy runs `test` on a runtime of one
/// thread whose clock is paused from the start.
fn on_a_driven_clock<Fut: std::future::Future<Output = ()>>(
    name: &str,
    test: impl FnOnce(PathBuf) -> Fut,
) {
    on_a_driven_clock_with(name, None, test);
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1234 to 1282.
/// [`on_a_driven_clock`] on a runtime that has `blocking_threads` threads for blocking work when that is given (a test that holds
/// the worker's question to the tree, [`TreeQuestionHeld`]); else as many as any runtime.
fn on_a_driven_clock_with<Fut: std::future::Future<Output = ()>>(
    name: &str,
    blocking_threads: Option<usize>,
    test: impl FnOnce(PathBuf) -> Fut,
) {
    if let Some(dir) = std::env::var_os(DRIVEN_DIR) {
        let mut runtime = tokio::runtime::Builder::new_current_thread();
        runtime.enable_all().start_paused(true);
        if let Some(threads) = blocking_threads {
            runtime.max_blocking_threads(threads);
        }
        runtime.build().unwrap().block_on(test(PathBuf::from(dir)));
        return;
    }
    let dir = common::scratch("driven");
    let bin = held_git(&dir);
    let search = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env("PATH", search)
        .env(DRIVEN_DIR, &dir)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    // The mark is gone whatever the copy did, so a stand-in that still waits goes on as the real git and ends.
    let _ = std::fs::remove_file(held_git_files(&dir).0);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // What the copy printed about its clock; the test harness writes its own words in front of the first of those lines.
    for line in text.lines() {
        if let Some(at) = line.find("DRIVEN: ") {
            println!("{}", &line[at..]);
        }
    }
    assert!(
        out.status.success() && text.contains("test result: ok. 1 passed"),
        "the copy of this test that drives the clock: {:?}\n{text}",
        out.status
    );
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1284 to 1289.
/// Keeps the paused clock of the test's runtime where it is. A paused clock moves by itself to the next timer when the runtime has
/// nothing to do, unless a blocking task is outstanding (tokio's rule for a paused clock): this is one that waits until it is let go.
struct ClockHold {
    go: std::sync::mpsc::Sender<()>,
    parked: tokio::task::JoinHandle<()>,
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1291 to 1304.
impl ClockHold {
    fn new() -> Self {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let parked = tokio::task::spawn_blocking(move || {
            let _ = wait.recv();
        });
        Self { go, parked }
    }

    async fn let_go(self) {
        drop(self.go);
        let _ = self.parked.await;
    }
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1306 to 1309.
/// The clock of a test that drives it. It stands while the test waits for real work, and it moves only when the test says so.
struct DrivenClock {
    hold: Option<ClockHold>,
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1311 to 1373.
impl DrivenClock {
    /// The clock stands from now on.
    fn stands() -> Self {
        Self {
            hold: Some(ClockHold::new()),
        }
    }

    fn now() -> tokio::time::Instant {
        tokio::time::Instant::now()
    }

    /// Real time passes while the clock stands: the worker's git, its requests and the engine go on.
    async fn stands_for(&self, real: Duration) {
        assert!(self.hold.is_some(), "the clock is held");
        tokio::task::spawn_blocking(move || std::thread::sleep(real))
            .await
            .unwrap();
    }

    /// Waits while the clock stands until `seen` holds, at most `limit` of real time; `false` when it never did.
    async fn stands_until(&self, limit: Duration, mut seen: impl FnMut() -> bool) -> bool {
        let began = Instant::now();
        while !seen() {
            if began.elapsed() > limit {
                return false;
            }
            self.stands_for(Duration::from_millis(2)).await;
        }
        true
    }

    /// One jump of the clock by `by`, while it is held.
    async fn jumps(&self, by: Duration) {
        assert!(self.hold.is_some(), "the clock is held");
        tokio::time::advance(by).await;
    }

    /// One jump of the clock by `by` while it is held, after which the tasks that the timers of that jump woke are polled before
    /// the test looks.
    async fn jumps_and_settles(&self, by: Duration) {
        self.jumps(by).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    /// Runs the clock to `at` and holds it there. On the way every timer that is due fires in its order, and the clock does not pass
    /// one while a task of the runtime is ready or a blocking call is outstanding (the worker's question to the file system of the
    /// tree is one): when this returns, everything that was ready before `at` has run. The tasks that a timer at `at` itself woke
    /// are polled before the test looks; a blocking call that one of them starts is not waited for here.
    async fn runs_to(&mut self, at: tokio::time::Instant) {
        if let Some(hold) = self.hold.take() {
            hold.let_go().await;
        }
        tokio::time::sleep_until(at).await;
        self.hold = Some(ClockHold::new());
        assert_eq!(Self::now(), at, "the clock stands where the test ran it to");
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1375 to 1387.
/// The worker's question to the file system of the work tree, held by the test. The question is one `statvfs` and one read on a
/// thread for blocking work (`snapshot::TreeAlive`, `ask_tree`). It is no git call, so the stand-in for git cannot hold it, and
/// the worker has no setting that replaces it. What the test owns is its runtime: that runtime has
/// [`HELD_QUESTION_THREADS`] threads for blocking work, one holds the clock ([`ClockHold`]) and this takes every other one. A
/// question the worker asks from then on waits for a thread until the test gives the threads back
/// ([`Self::released`]), and so does any other blocking work of the worker.
struct TreeQuestionHeld {
    go: Vec<std::sync::mpsc::Sender<()>>,
    parked: Vec<tokio::task::JoinHandle<()>>,
    /// Set by a blocking task of the test's own that was queued when every thread was taken: it shows whether any thread was
    /// free since.
    probe: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1389 to 1459.
impl TreeQuestionHeld {
    /// Takes every thread for blocking work but the one that holds the clock; returns when each of them runs a task of the test
    /// that waits. Nothing of the worker was waiting for a thread before (the caller ran the clock to this moment, and the clock
    /// does not pass a timer while a blocking call is outstanding).
    async fn from_now() -> Self {
        let (runs, ran) = std::sync::mpsc::channel::<()>();
        let mut go = Vec::new();
        let mut parked = Vec::new();
        for _ in 1..HELD_QUESTION_THREADS {
            let (send, wait) = std::sync::mpsc::channel::<()>();
            let runs = runs.clone();
            parked.push(tokio::task::spawn_blocking(move || {
                let _ = runs.send(());
                let _ = wait.recv();
            }));
            go.push(send);
        }
        let began = Instant::now();
        let mut running = 0;
        while running < go.len() {
            while ran.try_recv().is_ok() {
                running += 1;
            }
            assert!(
                began.elapsed() < REAL_WORK_WITHIN,
                "TRIGGER PRESENT: the test could not take every thread for blocking work: {running} of {} of its tasks run",
                go.len()
            );
            std::thread::sleep(Duration::from_millis(1));
            tokio::task::yield_now().await;
        }
        let probe = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_free = probe.clone();
        drop(tokio::task::spawn_blocking(move || {
            ran_free.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        Self { go, parked, probe }
    }

    /// Whether no thread for blocking work has been free since the test took them: a task of the test that was queued then has
    /// not run.
    fn holds(&self) -> bool {
        !self.probe.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Real time passes while the question is held and the clock stands. No thread is free for a sleep, so the thread of the test
    /// sleeps itself, in steps, and the worker's tasks are polled between the steps. `true` when `seen` held at one of them.
    async fn held_for(&self, real: Duration, mut seen: impl FnMut() -> bool) -> bool {
        let began = Instant::now();
        loop {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            if seen() {
                return true;
            }
            if began.elapsed() >= real {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The test gives the threads back: what waited for one runs, and the tree is asked and answers.
    async fn released(self) {
        drop(self.go);
        for task in self.parked {
            let _ = task.await;
        }
    }
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1461 to 1468.
/// The late lines of the worker's log, in order.
fn late_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct turn snapshot late: "))
        .collect()
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1470 to 1477.
/// The lines of the worker's log about a look that failed, in order.
fn failed_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct turn snapshot failed: "))
        .collect()
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1479 to 1496.
/// A look that waits in `git add`, on a clock the test drives ([`the_first_look_held`], [`a_look_held_on_the_timer`] and the
/// scenes that go on from them).
struct HeldLook {
    d: Direct,
    clock: DrivenClock,
    /// The moment the look began, on the driven clock. While a scene is being built it is the moment the test knows the clock to
    /// stand at: the start of the look that is held, or the end of the look that has just stored its copy.
    began: tokio::time::Instant,
    /// The mark of the stand-in: the look waits for as long as it exists.
    mark: PathBuf,
    /// The file the stand-in writes when a `git add` waits.
    held: PathBuf,
    /// How long, in real time, the spacing of the looks can hold a look back after the git work of the look before it ended:
    /// five periods of the timer of this worker (`snapshot::COPY_SPACING_CAP`), and a little more.
    spacing: Duration,
    /// How many files that no copy has the test has written into the tree.
    written: u32,
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1498 to 1665.
impl HeldLook {
    fn log(&self) -> String {
        self.d.log.lines().join("\n")
    }

    /// Runs the clock to the late limit of the look, `late_at`, and waits there, while the clock stands, for the late line. The
    /// late timer fires at that moment and the copy task is polled before this looks. The wait for the line is the test's
    /// patience ([`REAL_WORK_WITHIN`]) and no allowance of the rule: real time that a worker spends in blocking work between
    /// its late timer and its late line is not held by these tests, because no observer outside the worker can tell it from a
    /// slow processor; lateness on the driven clock is held. The worker has one such step, the question to the file system of
    /// the tree, which it asks from the first period of a look on, whatever the late limit. A question asked at a tick
    /// before the limit has been answered when the clock reaches the limit, because the clock does not pass a timer while a
    /// blocking call is outstanding ([`DrivenClock::runs_to`]), and at a tick that comes with the limit the late line comes first
    /// in the order of the arms of `watched_look`; so the line is there when this first looks, unless the test holds the question
    /// ([`TreeQuestionHeld`]). `true` when the log has a late line; the clock still stands at `late_at` then, so the line was
    /// written at the limit on that clock.
    async fn is_said_late_at(&mut self, late_at: tokio::time::Instant) -> bool {
        self.has_late_lines_at(late_at, 1).await
    }

    /// [`Self::is_said_late_at`] for a worker whose log already has late lines of looks before this one: `true` when the log
    /// has `lines` late lines or more with the clock standing at `late_at`.
    async fn has_late_lines_at(&mut self, late_at: tokio::time::Instant, lines: usize) -> bool {
        self.clock.runs_to(late_at).await;
        let said = self
            .clock
            .stands_until(REAL_WORK_WITHIN, || late_lines(&self.d).len() >= lines)
            .await;
        assert_eq!(
            DrivenClock::now(),
            late_at,
            "the clock stood at the limit while the test waited"
        );
        said
    }

    /// Waits, while the clock stands, until a look waits in `git add`. The clock has not moved since the moment the test
    /// names as the start of that look ([`Self::began`]): a look that waits began at that moment.
    async fn a_look_waits_in_git_add(&self, which: &str) {
        assert!(
            self.clock
                .stands_until(REAL_WORK_WITHIN, || self.held.exists())
                .await,
            "TRIGGER PRESENT: {which} did not wait in `git add` while the clock stood; copies {:?}; log:\n{}",
            records(&self.d),
            self.log()
        );
        assert_eq!(
            DrivenClock::now(),
            self.began,
            "the clock stood until {which} waited in `git add`"
        );
    }

    /// The scene so far: `copies` looks stored a copy, no look was said late and none failed.
    fn has_stored(&self, copies: usize) {
        assert_eq!(
            records(&self.d).len(),
            copies,
            "TRIGGER PRESENT: {copies} looks stored a copy before the look that waits; log:\n{}",
            self.log()
        );
        assert!(
            late_lines(&self.d).is_empty() && failed_lines(&self.d).is_empty(),
            "no look was late and none failed before the look that waits; log:\n{}",
            self.log()
        );
    }

    /// The look that waits in `git add` stores a copy, and has taken exactly `took` on the driven clock when it ends:
    /// [`Self::began`] is then the moment it ended. `next` is what the test sets up for the look after it.
    ///
    /// The clock jumps by two fifths of `took` while the look still waits. Then the tree answers, the look's git goes on, and the
    /// store holds the upload of its bundle. While it does, the test lets more real time pass than the spacing can hold a look
    /// back ([`Self::spacing`], counted from the end of the look's git work, which lies before the upload), sets the mark again
    /// and what `next` names, lets the upload go and jumps the clock by the rest of `took`. The look ends at that moment: it
    /// took `took`, part of it in its git work and part in its store, so a worker that measured one part alone would state
    /// another number.
    ///
    /// The look has ended when this returns, so a scene may move the clock or ask the worker to end next. A look ends when the
    /// worker has read the store's answer to the record of its copy, and that answer can come late in real time; the record
    /// alone, which the store has before it answers, does not show it. The store does (`FakeAws::put_answers`): the test waits,
    /// while the clock stands, until the store has given one more answer to a PUT of the record and the worker has closed the
    /// connection of every such answer, which it does when its store has returned. It is the wait of
    /// `HeldLook::stores_a_copy_at` in tests/b823_stop_copy.rs; each of the two files has its own scene types and its own helper.
    async fn stores_a_copy(&mut self, took: Duration, next: Next) {
        let in_git = took * 2 / 5;
        let in_store = took - in_git;
        let stored = records(&self.d).len();
        let failed = failed_lines(&self.d).len();
        let record = keys::snapshot_json_key(RUN);
        let (answered, _) = self.d.aws.put_answers(&record);
        self.d.aws.hold_put_body("snapshot.bundle", 0);
        // The first part of what the stored look takes passes while its git works.
        self.clock.jumps(in_git).await;
        // The tree answers: the look's git goes on, and the store holds the upload of its bundle.
        std::fs::remove_file(&self.held).unwrap();
        std::fs::remove_file(&self.mark).unwrap();
        assert!(
            self.clock
                .stands_until(REAL_WORK_WITHIN, || self.d.aws.put_body_held())
                .await,
            "TRIGGER PRESENT: the look did not reach the store; log:\n{}",
            self.log()
        );
        assert_eq!(
            DrivenClock::now(),
            self.began + in_git,
            "the clock stood while the look made its copy"
        );
        // The git work of the look ended before its upload began: from here the spacing holds the next look back for five
        // periods of the timer at the most.
        self.clock.stands_for(self.spacing).await;
        // From now on the tree does not answer.
        std::fs::write(&self.mark, "set\n").unwrap();
        if next.changed {
            // And it holds a file that no copy has.
            self.written += 1;
            let name = match self.written {
                1 => "late.txt".to_string(),
                n => format!("late-{n}.txt"),
            };
            std::fs::write(
                self.d.worktree().join(name),
                "written after the last copy\n",
            )
            .unwrap();
        }
        if next.one_add_goes_through {
            std::fs::write(pass_files(self.mark.parent().unwrap()).0, "one\n").unwrap();
        }
        // The rest of what the stored look takes passes while it stores.
        self.d.aws.release_put_body();
        self.clock.jumps(in_store).await;
        self.began += took;
        assert!(
            self.clock
                .stands_until(REAL_WORK_WITHIN, || records(&self.d).len() > stored)
                .await,
            "TRIGGER PRESENT: the look did not store its copy; copies {:?}; log:\n{}",
            records(&self.d),
            self.log()
        );
        // The store has the record, and its answer may still be on its way: the look ends when the worker has read it. The
        // clock stands until the store tells that it has given a new answer for the record and that every such answer was
        // read, so the look took what the test imposed however late the answer came.
        assert!(
            self.clock
                .stands_until(REAL_WORK_WITHIN, || {
                    let (given, read) = self.d.aws.put_answers(&record);
                    given > answered && read == given
                })
                .await,
            "TRIGGER PRESENT: the worker did not read the store's answer to the record of its copy; copies {:?}; log:\n{}",
            records(&self.d),
            self.log()
        );
        assert_eq!(
            failed_lines(&self.d).len(),
            failed,
            "TRIGGER PRESENT: the look that stored the copy did not fail; log:\n{}",
            self.log()
        );
        assert_eq!(
            DrivenClock::now(),
            self.began,
            "the clock stood while the look stored its copy"
        );
    }

    /// The tree answers again, so a stand-in that still waits ends by itself.
    fn ends(self) {
        let _ = std::fs::remove_file(&self.mark);
    }
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1667 to 1718.
/// Starts a worker inside the test's runtime whose late limit is set to `set`, and returns when its first look waits in
/// `git add`: no look ran before it and no copy was stored. The git work of a look has `git_limit`.
///
/// How the moment is known. The clock stands from the start, so the first look, which the engine's first tool call asks for
/// and which waits for no timer, begins at the moment the test began. The mark of the stand-in is set before the engine works;
/// the stand-in lets the first `git add` through, which is the reading of the tree the worker makes before it starts the
/// engine, and holds the second: the first look of the copy task.
async fn the_first_look_held(set: Duration, git_limit: Duration, dir: &Path) -> HeldLook {
    let clock = DrivenClock::stands();
    let first = DrivenClock::now();
    let (mark, held) = held_git_files(dir);
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "600".into())],
        edit_cfg: Some(Box::new(move |c| {
            c.direct.snapshot_interval = Some(DRIVEN_TIMER);
            c.direct.snapshot_floor = Some(Duration::from_millis(50));
            c.direct.copy_late = Some(set);
            c.direct.copy_git_limit = Some(git_limit);
        })),
        ..Opts::default()
    })
    .await;
    assert_eq!(
        d.app.cfg.direct.events_every, DRIVEN_POSTS_EVERY,
        "the flusher of the test rig"
    );
    // From here the tree does not answer a look: the test rig has made its repository, and the stand-in lets the worker's first
    // reading of the tree through.
    std::fs::write(&mark, "set\n").unwrap();
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    let look = HeldLook {
        d,
        clock,
        began: first,
        mark,
        held,
        spacing: DRIVEN_TIMER * 5 + Duration::from_millis(10),
        written: 0,
    };
    look.a_look_waits_in_git_add("the first look").await;
    assert!(
        look.log()
            .contains("direct turn copies: the first reading of the tree took "),
        "TRIGGER PRESENT: the `git add` the stand-in let through was the worker's reading of the tree before the engine started; \
         log:\n{}",
        look.log()
    );
    look.has_stored(0);
    look
}

// Copied from tests/b823_stop_copy.rs at b7c90f98edb0, lines 1720 to 1739.
/// Starts a worker inside the test's runtime whose late limit is set to `set`, lets its first look store a copy that took exactly
/// `last` on the driven clock, and returns when the look after it waits in `git add`. The git work of a look has `git_limit`.
///
/// How the two moments are known. The first look begins at the moment the test began ([`the_first_look_held`]) and takes `last`
/// ([`HeldLook::stores_a_copy`]). The tick of the next look is due ([`DRIVEN_TIMER`] is shorter) and nothing holds that look
/// back, so the worker starts it at the moment the clock stands at, and the test does not move the clock again before the
/// stand-in has said that it waits.
async fn a_look_held_after_a_stored_look(
    last: Duration,
    set: Duration,
    git_limit: Duration,
    dir: &Path,
) -> HeldLook {
    let mut look = the_first_look_held(set, git_limit, dir).await;
    look.stores_a_copy(last, Next::HELD).await;
    look.a_look_waits_in_git_add("the look after the stored look")
        .await;
    look.has_stored(1);
    look
}

// ---------------------------------------------------------------------------------------------------------------------------------
// The tests. Everything above this line is copied from `tests/b823_stop_copy.rs` (see the note at the top of this file);
// everything below is new.
// ---------------------------------------------------------------------------------------------------------------------------------

/// How long the worker waits for the answer of its question to the file system of the tree (`snapshot::TREE_ANSWER_LIMIT`), written
/// out: the moments these tests look at follow from it, and they are not asked of the worker's own constant.
const ANSWER_WITHIN: Duration = Duration::from_secs(5);

/// The part of the worker's log line that gives a look up because the file system of the tree did not answer its question
/// (`Copies::tree_check` in `src/direct/mod.rs`).
const GIVEN_UP: &str = "; the look in progress is given up";

/// What the copy task logs when it takes a request to end up (`Copies::stop_asked`).
const TAKEN_UP: &str = "direct turn session ending: a copy of the work is saved at once";

/// The lines of the worker's log that give a look up because the tree did not answer, in order.
fn given_up_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains(GIVEN_UP))
        .collect()
}

/// The numbers of the first late line of the log, `None` when there is none; a line that does not state its numbers fails.
fn first_late_line(d: &Direct) -> Option<SaidLate> {
    let said = late_lines(d);
    let first = said.first()?;
    let Some(stated) = said_late(first.trim_end()) else {
        panic!("the worker's late line does not state its numbers: {said:?}");
    };
    Some(stated)
}

/// The worker is asked to end while the look the test holds waits in `git add`, and the copy task takes the request up with the
/// clock standing. The steps are those of `said_late_when_the_tree_has_answered` in `tests/b823_stop_copy.rs`.
async fn asked_to_end_while_it_waits(look: &HeldLook) {
    assert!(
        !look.log().contains("direct turn session ending: "),
        "TRIGGER PRESENT: the worker was not asked to end before the test asks; log:\n{}",
        look.log()
    );
    let _ = look.d.app.ending.ask();
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || look.log().contains(TAKEN_UP))
            .await,
        "TRIGGER PRESENT: the copy task did not take the request to end up while the look was held; log:\n{}",
        look.log()
    );
    assert_eq!(
        DrivenClock::now(),
        look.began,
        "the clock stood while the copy task took the request up"
    );
}

/// The comment of `watched_look` says: "While its git works, the file system of the work tree is asked every
/// `snapshot::TREE_ASK_EVERY` whether it still answers, and the look is given up when it does not". The copy task's bullet in the
/// worker's AGENTS.md says the tree is asked "before a look and every `TREE_ASK_EVERY`, 20 s, while the look's git works". In these
/// tests the period of the question is the set late limit, 400 ms. The last stored look took 200 ms, so the held look is late at
/// 600 ms. At 399 ms the test takes every thread the worker could ask on, so a question asked from then on gets no answer and fails
/// after the 5 s the worker waits for one. By the two texts the tree is asked 400 ms after the look began, while its git works, and
/// the look is given up 5,400 ms after it began, not a millisecond before.
#[test]
fn b923_a_look_late_at_600_ms_asks_its_tree_at_400_ms_and_is_given_up_at_5400_ms() {
    on_a_driven_clock_with(
        "b923_a_look_late_at_600_ms_asks_its_tree_at_400_ms_and_is_given_up_at_5400_ms",
        Some(HELD_QUESTION_THREADS),
        |dir| async move {
            assert_eq!(
                TREE_ANSWER_LIMIT, ANSWER_WITHIN,
                "TRIGGER PRESENT: the worker waits 5 s for the answer of the tree"
            );
            let ms = Duration::from_millis(1);
            let set = Duration::from_millis(400);
            let limit = Duration::from_millis(600);
            let mut look = a_look_held_after_a_stored_look(
                Duration::from_millis(200),
                set,
                limit + HELD_PAST_THE_LIMIT,
                &dir,
            )
            .await;
            let began = look.began;
            look.clock.runs_to(began + Duration::from_millis(399)).await;
            assert!(
                late_lines(&look.d).is_empty() && given_up_lines(&look.d).is_empty(),
                "the look was said late or given up before 400 ms; log:\n{}",
                look.log()
            );
            let held = TreeQuestionHeld::from_now().await;
            // 400 ms: the moment of the question by the texts. It waits for a thread from now on.
            look.clock.jumps_and_settles(ms).await;
            // 600 ms: the late limit of the look.
            look.clock
                .jumps_and_settles(Duration::from_millis(200))
                .await;
            let late_at_600 = late_lines(&look.d).len();
            // 5,399 ms: one millisecond before the answer of a question asked at 400 ms is given up.
            look.clock
                .jumps_and_settles(Duration::from_millis(4_799))
                .await;
            let before = held
                .held_for(QUESTION_HELD_FOR, || !given_up_lines(&look.d).is_empty())
                .await;
            // 5,400 ms: a question asked at 400 ms has had its 5 s.
            look.clock.jumps_and_settles(ms).await;
            assert_eq!(
                DrivenClock::now(),
                began + Duration::from_millis(5_400),
                "the clock stands 5,400 ms after the look began"
            );
            let at_5400 = held
                .held_for(REAL_WORK_WITHIN, || !given_up_lines(&look.d).is_empty())
                .await;
            // 5,600 ms: a question asked at the late limit of 600 ms has had its 5 s. Looked at for the record only.
            look.clock
                .jumps_and_settles(Duration::from_millis(200))
                .await;
            let at_5600 = at_5400
                || held
                    .held_for(REAL_WORK_WITHIN, || !given_up_lines(&look.d).is_empty())
                    .await;
            assert!(
                held.holds(),
                "TRIGGER PRESENT: a thread for blocking work was free while the test held them, so the question was not held; log:\n{}",
                look.log()
            );
            println!(
                "DRIVEN: the last stored look took 200 ms and the next look was held, late at 600 ms; the threads for the question were \
                 taken at 399 ms; late lines at 600 ms: {late_at_600}; given up by 5,399 ms: {before}; by 5,400 ms: {at_5400}; by 5,600 \
                 ms: {at_5600}"
            );
            assert!(
                !before,
                "the look was given up before 5,400 ms, earlier than 5 s after a question asked at 400 ms; log:\n{}",
                look.log()
            );
            assert!(
                at_5400,
                "the texts say the tree is asked every 400 ms while the look's git works, so a question asked at 400 ms gets no \
                 answer and the look is given up at 5,400 ms; it was not (late lines at 600 ms: {late_at_600}; given up by 5,600 \
                 ms: {at_5600}); log:\n{}",
                look.log()
            );
            held.released().await;
            look.ends();
        },
    );
}

/// Row R15 of the table: when the arm of `watched_look` that asks the tree is switched on. The first sentence of the
/// copy task's bullet in the worker's AGENTS.md says the tree is asked every `TREE_ASK_EVERY` while the look's git works. A later
/// sentence of the same bullet says the arm is switched on from the second pass of the loop, so that the question can be under way
/// when the late timer fires only in a look that was asked to end before. This look was not asked to end. The set limit, and with it
/// the period of the question, is 1,500 ms; the last stored look took 1,000 ms, so the held look is late at 3,000 ms. At 1,499 ms
/// the test takes every thread the worker could ask on. By the first sentence the tree is asked at 1,500 ms, the question waits for
/// a thread, and at 3,000 ms the copy task waits for its answer: no late line is written while the question is held. When the test
/// gives the threads back, the line states 3,000 ms taken, the limit of 3,000 ms, the set limit of 1,500 ms and the stored look of
/// 1,000 ms.
#[test]
fn b923_a_look_that_was_not_asked_to_end_asks_its_tree_at_1500_ms_before_its_late_limit_of_3000_ms()
{
    on_a_driven_clock_with(
        "b923_a_look_that_was_not_asked_to_end_asks_its_tree_at_1500_ms_before_its_late_limit_of_3000_ms",
        Some(HELD_QUESTION_THREADS),
        |dir| async move {
            let ms = Duration::from_millis(1);
            let set = Duration::from_millis(1_500);
            let last = Duration::from_millis(1_000);
            let limit = Duration::from_millis(3_000);
            let mut look =
                a_look_held_after_a_stored_look(last, set, limit + HELD_PAST_THE_LIMIT, &dir).await;
            let began = look.began;
            look.clock
                .runs_to(began + Duration::from_millis(1_499))
                .await;
            assert!(
                late_lines(&look.d).is_empty() && failed_lines(&look.d).is_empty(),
                "the look was said late or failed before 1,500 ms; log:\n{}",
                look.log()
            );
            assert!(
                !look.log().contains("direct turn session ending: "),
                "TRIGGER PRESENT: this look was not asked to end; log:\n{}",
                look.log()
            );
            let held = TreeQuestionHeld::from_now().await;
            // 1,500 ms: the moment of the question by the first sentence. It waits for a thread from now on.
            look.clock.jumps_and_settles(ms).await;
            // 3,000 ms: the late limit of the look; its late timer fires.
            look.clock
                .jumps_and_settles(Duration::from_millis(1_500))
                .await;
            assert_eq!(
                DrivenClock::now(),
                began + limit,
                "the clock stands at the late limit"
            );
            let at_once = late_lines(&look.d).len();
            let came = at_once > 0
                || held
                    .held_for(QUESTION_HELD_FOR, || !late_lines(&look.d).is_empty())
                    .await;
            assert!(
                held.holds(),
                "TRIGGER PRESENT: a thread for blocking work was free while the test held them, so the question was not held; log:\n{}",
                look.log()
            );
            println!(
                "DRIVEN: the last stored look took 1,000 ms and the next look was held, late at 3,000 ms, not asked to end; the threads \
                 for the question were taken at 1,499 ms; at 3,000 ms the worker had written {at_once} late lines, and while the \
                 question was held a line came: {came}"
            );
            assert!(
                !came,
                "the first sentence says the tree is asked every 1,500 ms while the look's git works, so the question asked at 1,500 \
                 ms is held at the late limit of 3,000 ms and the copy task waits for its answer; a late line was written while it \
                 was held: {:?}; log:\n{}",
                first_late_line(&look.d),
                look.log()
            );
            held.released().await;
            assert!(
                look.clock
                    .stands_until(REAL_WORK_WITHIN, || !late_lines(&look.d).is_empty())
                    .await,
                "the look was not said late when the tree had answered; log:\n{}",
                look.log()
            );
            let stated = first_late_line(&look.d).unwrap();
            assert_eq!(
                (stated.taken, stated.limit, stated.set, stated.last),
                (limit, limit, set, Some(last)),
                "the late line states the time taken, the limit, the set limit and the last stored look"
            );
            look.ends();
        },
    );
}

/// A request to end comes while the first look of a worker runs; that look stores a copy in 200 ms, and one more look follows it at
/// once. The copy task's bullet in the worker's AGENTS.md says a look is late at the set limit, or at three times what the last look
/// that stored a copy took when that is longer, and the next bullet says that "the look that follows it has the limits of any look".
/// The last look that stored a copy took 200 ms, so by the text the look that follows is late 600 ms after it began: no late line at
/// 599 ms, one at 600 ms, which states 600 ms taken, the limit of 600 ms, the set limit of 400 ms and the stored look of 200 ms.
#[test]
fn b923_the_look_after_a_first_look_of_200_ms_that_a_request_to_end_overtook_is_late_at_600_ms() {
    on_a_driven_clock(
        "b923_the_look_after_a_first_look_of_200_ms_that_a_request_to_end_overtook_is_late_at_600_ms",
        |dir| async move {
            let set = Duration::from_millis(400);
            let stored = Duration::from_millis(200);
            let limit = Duration::from_millis(600);
            let mut look = the_first_look_held(set, limit + HELD_PAST_THE_LIMIT, &dir).await;
            asked_to_end_while_it_waits(&look).await;
            look.stores_a_copy(stored, Next::HELD).await;
            look.a_look_waits_in_git_add(
                "the look that follows the look a request to end overtook",
            )
            .await;
            look.has_stored(1);
            let began = look.began;
            look.clock.runs_to(began + Duration::from_millis(599)).await;
            let early = first_late_line(&look.d);
            println!(
                "DRIVEN: the first look was asked to end and stored a copy in 200 ms; the look that follows it was held; at 599 ms \
                 its late line: {early:?}"
            );
            assert!(
                early.is_none(),
                "the text says the look that follows is late at three times the last stored look, 600 ms; it was said late before: \
                 {early:?}; log:\n{}",
                look.log()
            );
            assert!(
                look.is_said_late_at(began + limit).await,
                "the look that follows was not said late at 600 ms; log:\n{}",
                look.log()
            );
            let stated = first_late_line(&look.d).unwrap();
            assert_eq!(
                (stated.taken, stated.limit, stated.set, stated.last),
                (limit, limit, set, Some(stored)),
                "the late line states the time taken, the limit, the set limit and the last stored look"
            );
            look.ends();
        },
    );
}

/// Row R13 of the table, point 2 the other way round. The last stored look took 200 ms, so the next look is late at
/// 600 ms. A request to end comes while that look runs; it stores a copy in 100 ms, and one more look follows it at once. By the
/// worker's AGENTS.md the last look that stored a copy is now the one of 100 ms, three times that is 300 ms, below the set limit of
/// 400 ms, and the look that follows "has the limits of any look": it is late 400 ms after it began, not at 399 ms, and its line
/// states 400 ms taken, the limit of 400 ms, the set limit of 400 ms and the stored look of 100 ms.
#[test]
fn b923_the_look_after_a_look_of_100_ms_that_a_request_to_end_overtook_is_late_at_400_ms() {
    on_a_driven_clock(
        "b923_the_look_after_a_look_of_100_ms_that_a_request_to_end_overtook_is_late_at_400_ms",
        |dir| async move {
            let set = Duration::from_millis(400);
            let stored = Duration::from_millis(100);
            let limit = Duration::from_millis(400);
            let mut look = a_look_held_after_a_stored_look(
                Duration::from_millis(200),
                set,
                Duration::from_millis(600) + HELD_PAST_THE_LIMIT,
                &dir,
            )
            .await;
            asked_to_end_while_it_waits(&look).await;
            look.stores_a_copy(stored, Next::HELD).await;
            look.a_look_waits_in_git_add(
                "the look that follows the look a request to end overtook",
            )
            .await;
            look.has_stored(2);
            let began = look.began;
            look.clock.runs_to(began + Duration::from_millis(399)).await;
            assert!(
                late_lines(&look.d).is_empty(),
                "the look that follows was said late before 400 ms; log:\n{}",
                look.log()
            );
            let at_400 = look.is_said_late_at(began + limit).await;
            // For the record: where the late line comes when it does not come at 400 ms.
            let at_600 = at_400
                || look
                    .is_said_late_at(began + Duration::from_millis(600))
                    .await;
            let stated = first_late_line(&look.d);
            println!(
                "DRIVEN: the last stored look took 200 ms; the next look was asked to end and stored a copy in 100 ms; the look that \
                 follows it was held; said late by 400 ms: {at_400}; by 600 ms: {at_600}; its line: {stated:?}"
            );
            assert!(
                at_400,
                "the text says the look that follows has the limits of any look: three times the last stored look of 100 ms is \
                 below the set limit, so it is late at 400 ms; it was not (said late by 600 ms: {at_600}, line {stated:?}); log:\n{}",
                look.log()
            );
            let stated = stated.unwrap();
            assert_eq!(
                (stated.taken, stated.limit, stated.set, stated.last),
                (limit, limit, set, Some(stored)),
                "the late line states the time taken, the limit, the set limit and the last stored look"
            );
            look.ends();
        },
    );
}

/// The numbers of the late line at `index` (from 0) of the log, `None` when there is none; a line that does not state its numbers
/// fails.
fn late_line_at(d: &Direct, index: usize) -> Option<SaidLate> {
    let said = late_lines(d);
    let line = said.get(index)?;
    let Some(stated) = said_late(line.trim_end()) else {
        panic!("the worker's late line does not state its numbers: {said:?}");
    };
    Some(stated)
}

/// The numbers a late line states, as one tuple: taken, limit, set limit, last stored look.
fn numbers(stated: Option<SaidLate>) -> Option<(Duration, Duration, Duration, Option<Duration>)> {
    stated.map(|s| (s.taken, s.limit, s.set, s.last))
}

/// Point 3: a late limit equal to the period of the question. The set limit, and with it the period of the question, is
/// 400 ms. The last stored look took 100 ms; three times that is 300 ms, below the set limit, so the held look is late at 400 ms,
/// the moment of its first question. At 399 ms the test takes every thread the worker could ask on. The arms of `watched_look` are
/// polled in a fixed order, the late line before the question, so the late line is written at 400 ms although the question waits
/// for a thread, and states 400 ms taken, the limit of 400 ms, the set limit of 400 ms and the stored look of 100 ms. The question
/// asked at 400 ms gets no answer: the look is given up at 5,400 ms, not a millisecond before.
#[test]
fn b923_a_look_late_at_400_ms_with_a_question_every_400_ms_is_said_late_at_400_ms_and_given_up_at_5400_ms()
 {
    on_a_driven_clock_with(
        "b923_a_look_late_at_400_ms_with_a_question_every_400_ms_is_said_late_at_400_ms_and_given_up_at_5400_ms",
        Some(HELD_QUESTION_THREADS),
        |dir| async move {
            let ms = Duration::from_millis(1);
            let set = Duration::from_millis(400);
            let last = Duration::from_millis(100);
            let limit = Duration::from_millis(400);
            let mut look =
                a_look_held_after_a_stored_look(last, set, limit + HELD_PAST_THE_LIMIT, &dir).await;
            let began = look.began;
            look.clock.runs_to(began + Duration::from_millis(399)).await;
            assert!(
                late_lines(&look.d).is_empty() && given_up_lines(&look.d).is_empty(),
                "the look was said late or given up before 400 ms; log:\n{}",
                look.log()
            );
            let held = TreeQuestionHeld::from_now().await;
            // 400 ms: the late limit of the look and the moment of its first question.
            look.clock.jumps_and_settles(ms).await;
            let late_at_400 = held
                .held_for(QUESTION_HELD_FOR, || !late_lines(&look.d).is_empty())
                .await;
            let stated = numbers(first_late_line(&look.d));
            // 5,399 ms: one millisecond before the answer of a question asked at 400 ms is given up.
            look.clock
                .jumps_and_settles(Duration::from_millis(4_999))
                .await;
            let before = held
                .held_for(QUESTION_HELD_FOR, || !given_up_lines(&look.d).is_empty())
                .await;
            // 5,400 ms: a question asked at 400 ms has had its 5 s.
            look.clock.jumps_and_settles(ms).await;
            assert_eq!(
                DrivenClock::now(),
                began + Duration::from_millis(5_400),
                "the clock stands 5,400 ms after the look began"
            );
            let at_5400 = held
                .held_for(REAL_WORK_WITHIN, || !given_up_lines(&look.d).is_empty())
                .await;
            assert!(
                held.holds(),
                "TRIGGER PRESENT: a thread for blocking work was free while the test held them, so the question was not held; log:\n{}",
                look.log()
            );
            println!(
                "DRIVEN: the last stored look took 100 ms and the next look was held, late at 400 ms, the period of the question; the \
                 threads for the question were taken at 399 ms; a late line at 400 ms: {late_at_400}, stating {stated:?}; given up \
                 by 5,399 ms: {before}; by 5,400 ms: {at_5400}"
            );
            assert!(
                late_at_400,
                "the late line comes before the question at a late limit equal to the period, so it is written at 400 ms while \
                 the question waits; it was not; log:\n{}",
                look.log()
            );
            assert_eq!(
                stated,
                Some((limit, limit, set, Some(last))),
                "the late line states the time taken, the limit, the set limit and the last stored look"
            );
            assert!(
                !before,
                "the look was given up before 5,400 ms, earlier than 5 s after a question asked at 400 ms; log:\n{}",
                look.log()
            );
            assert!(
                at_5400,
                "a question asked at 400 ms gets no answer, so the look is given up at 5,400 ms; it was not; log:\n{}",
                look.log()
            );
            held.released().await;
            look.ends();
        },
    );
}

/// Point 3: the ticks of the question while a look stores its copy. The arm of `watched_look` that asks the tree is polled
/// on every pass, so its ticks also come while the look stores its copy, and the tree is asked only while the look's git works.
/// The set limit, and with it the period of the question, is 400 ms. The first look of a worker waits in `git add`; at 100 ms the
/// tree answers and the look's git goes on, while the test holds the upload of its copy. With the look in its store step the test
/// takes every thread the worker could ask on and runs the clock to 6,000 ms, one period at a time: ticks at 400 ms and every
/// 400 ms after. A question asked at one of them would get no answer and give the look up, 5,400 ms after it began at the latest.
/// No look is given up, the late line at 400 ms says the look is storing its copy, and when the test gives the threads and the
/// upload back, the copy is stored and no look failed.
#[test]
fn b923_ticks_while_a_look_stores_its_copy_ask_the_tree_nothing_and_the_copy_is_stored() {
    on_a_driven_clock_with(
        "b923_ticks_while_a_look_stores_its_copy_ask_the_tree_nothing_and_the_copy_is_stored",
        Some(HELD_QUESTION_THREADS),
        |dir| async move {
            let set = Duration::from_millis(400);
            let look = the_first_look_held(set, HELD_PAST_THE_LIMIT, &dir).await;
            let began = look.began;
            look.d.aws.hold_put_body("snapshot.bundle", 0);
            // 100 ms: the look's git still waits.
            look.clock.jumps(Duration::from_millis(100)).await;
            // The tree answers: the look's git goes on, and the store holds the upload of its copy.
            std::fs::remove_file(&look.held).unwrap();
            std::fs::remove_file(&look.mark).unwrap();
            assert!(
                look.clock
                    .stands_until(REAL_WORK_WITHIN, || look.d.aws.put_body_held())
                    .await,
                "TRIGGER PRESENT: the look did not reach the store; log:\n{}",
                look.log()
            );
            assert_eq!(
                DrivenClock::now(),
                began + Duration::from_millis(100),
                "the clock stood while the look made its copy"
            );
            let held = TreeQuestionHeld::from_now().await;
            // 400 ms: the first tick of the question, and the late limit of the first look of a worker.
            look.clock
                .jumps_and_settles(Duration::from_millis(300))
                .await;
            // 800 ms to 6,000 ms: a tick at each.
            for _ in 0..14 {
                look.clock.jumps_and_settles(set).await;
            }
            assert_eq!(
                DrivenClock::now(),
                began + Duration::from_millis(6_000),
                "the clock stands 6,000 ms after the look began"
            );
            let given_up = held
                .held_for(QUESTION_HELD_FOR, || !given_up_lines(&look.d).is_empty())
                .await;
            assert!(
                held.holds(),
                "TRIGGER PRESENT: a thread for blocking work was free while the test held them, so a question would have been held; \
                 log:\n{}",
                look.log()
            );
            let late = late_lines(&look.d);
            println!(
                "DRIVEN: the first look was late at 400 ms and stored its copy from 100 ms; the threads for the question were taken \
                 at 100 ms and the clock ran to 6,000 ms, a tick every 400 ms; given up by 6,000 ms: {given_up}; late lines: \
                 {late:?}"
            );
            assert!(
                late.len() == 1 && late[0].contains("storing the copy"),
                "TRIGGER PRESENT: the look was in its store step when the ticks came, and its late line says so: {late:?}; log:\n{}",
                look.log()
            );
            assert!(
                !given_up,
                "the tree is asked only while the look's git works, so no tick of the store step asks it, and no question gives \
                 the look up; log:\n{}",
                look.log()
            );
            held.released().await;
            look.d.aws.release_put_body();
            // The fake store looks for the release every 10 ms of the driven clock.
            look.clock.jumps(Duration::from_millis(10)).await;
            assert!(
                look.clock
                    .stands_until(REAL_WORK_WITHIN, || records(&look.d).len() == 1)
                    .await,
                "the look did not store its copy when the test gave the threads and the upload back; log:\n{}",
                look.log()
            );
            assert!(
                failed_lines(&look.d).is_empty() && given_up_lines(&look.d).is_empty(),
                "no look failed; log:\n{}",
                look.log()
            );
        },
    );
}

/// Point 3: a tree that does not answer during the look that follows a look which a request to end overtook. The set
/// limit, and with it the period of the question, is 400 ms. The last stored look took 200 ms, so the next look is late at 600 ms.
/// A request to end comes while that look runs; it stores a copy in 100 ms, and one more look follows it at once. That look has
/// the limits of any look: three times 100 ms is below the set limit, so it is late at 400 ms, and the tree is asked every 400 ms
/// while its git works. At 399 ms the test takes every thread the worker could ask on. The question asked at 400 ms gets no
/// answer, and the look that follows is given up at 5,400 ms, not a millisecond before.
#[test]
fn b923_the_look_after_a_look_that_a_request_to_end_overtook_asks_its_tree_at_400_ms_and_is_given_up_at_5400_ms()
 {
    on_a_driven_clock_with(
        "b923_the_look_after_a_look_that_a_request_to_end_overtook_asks_its_tree_at_400_ms_and_is_given_up_at_5400_ms",
        Some(HELD_QUESTION_THREADS),
        |dir| async move {
            let ms = Duration::from_millis(1);
            let set = Duration::from_millis(400);
            let mut look = a_look_held_after_a_stored_look(
                Duration::from_millis(200),
                set,
                Duration::from_millis(600) + HELD_PAST_THE_LIMIT,
                &dir,
            )
            .await;
            asked_to_end_while_it_waits(&look).await;
            look.stores_a_copy(Duration::from_millis(100), Next::HELD)
                .await;
            look.a_look_waits_in_git_add(
                "the look that follows the look a request to end overtook",
            )
            .await;
            look.has_stored(2);
            let began = look.began;
            look.clock.runs_to(began + Duration::from_millis(399)).await;
            assert!(
                late_lines(&look.d).is_empty() && given_up_lines(&look.d).is_empty(),
                "the look that follows was said late or given up before 400 ms; log:\n{}",
                look.log()
            );
            let held = TreeQuestionHeld::from_now().await;
            // 400 ms: the late limit of the look that follows, and the moment of its first question.
            look.clock.jumps_and_settles(ms).await;
            // 600 ms: the late limit of the look before; a worker that kept that limit asks its tree here.
            look.clock
                .jumps_and_settles(Duration::from_millis(200))
                .await;
            // 5,399 ms: one millisecond before the answer of a question asked at 400 ms is given up.
            look.clock
                .jumps_and_settles(Duration::from_millis(4_799))
                .await;
            let before = held
                .held_for(QUESTION_HELD_FOR, || !given_up_lines(&look.d).is_empty())
                .await;
            // 5,400 ms: a question asked at 400 ms has had its 5 s.
            look.clock.jumps_and_settles(ms).await;
            assert_eq!(
                DrivenClock::now(),
                began + Duration::from_millis(5_400),
                "the clock stands 5,400 ms after the look that follows began"
            );
            let at_5400 = held
                .held_for(REAL_WORK_WITHIN, || !given_up_lines(&look.d).is_empty())
                .await;
            // 5,600 ms: a question asked at 600 ms, the late limit of the look before, has had its 5 s. For the record only.
            look.clock
                .jumps_and_settles(Duration::from_millis(200))
                .await;
            let at_5600 = at_5400
                || held
                    .held_for(REAL_WORK_WITHIN, || !given_up_lines(&look.d).is_empty())
                    .await;
            assert!(
                held.holds(),
                "TRIGGER PRESENT: a thread for blocking work was free while the test held them, so the question was not held; log:\n{}",
                look.log()
            );
            println!(
                "DRIVEN: the last stored look took 200 ms; the next look was asked to end and stored a copy in 100 ms; the look that \
                 follows it was held and the threads for the question were taken at 399 ms; given up by 5,399 ms: {before}; by \
                 5,400 ms: {at_5400}; by 5,600 ms: {at_5600}"
            );
            assert!(
                !before,
                "the look that follows was given up before 5,400 ms, earlier than 5 s after a question asked at 400 ms; log:\n{}",
                look.log()
            );
            assert!(
                at_5400,
                "the look that follows asks its tree every 400 ms while its git works, so a question asked at 400 ms gets no answer \
                 and the look is given up at 5,400 ms; it was not (given up by 5,600 ms: {at_5600}); log:\n{}",
                look.log()
            );
            held.released().await;
            look.ends();
        },
    );
}

/// Point 3: the late limit of the look that follows a look which a request to end overtook and which then failed. The set
/// limit is 400 ms and the git work of every look has 1,000 ms. The last stored look took 200 ms, so the next look is late at
/// 600 ms. A request to end comes while that look waits in `git add`; it is said late at 600 ms and its own git limit gives it up
/// at 1,000 ms, and one more look follows it at once. A look that failed stores no copy, so the last look that stored one is still
/// the look of 200 ms: the look that follows is late 600 ms after it began, not at 599 ms, and its line states 600 ms taken, the
/// limit of 600 ms, the set limit of 400 ms and the stored look of 200 ms.
#[test]
fn b923_the_look_after_a_look_that_a_request_to_end_overtook_and_its_git_limit_gave_up_is_late_at_600_ms()
 {
    on_a_driven_clock(
        "b923_the_look_after_a_look_that_a_request_to_end_overtook_and_its_git_limit_gave_up_is_late_at_600_ms",
        |dir| async move {
            let set = Duration::from_millis(400);
            let stored = Duration::from_millis(200);
            let limit = Duration::from_millis(600);
            let git_limit = Duration::from_millis(1_000);
            let mut look = a_look_held_after_a_stored_look(stored, set, git_limit, &dir).await;
            asked_to_end_while_it_waits(&look).await;
            // The look keeps waiting while the mark exists; the file it wrote is taken away, so the look that follows writes it anew.
            std::fs::remove_file(&look.held).unwrap();
            let first = look.began;
            look.clock.runs_to(first + git_limit).await;
            look.began = first + git_limit;
            look.a_look_waits_in_git_add(
                "the look that follows the look whose git limit gave it up",
            )
            .await;
            assert!(
                late_lines(&look.d).len() == 1 && !failed_lines(&look.d).is_empty(),
                "TRIGGER PRESENT: the look that the request to end overtook was said late once and failed; log:\n{}",
                look.log()
            );
            assert_eq!(
                numbers(late_line_at(&look.d, 0)),
                Some((limit, limit, set, Some(stored))),
                "the look that the request to end overtook was late at 600 ms"
            );
            assert_eq!(
                records(&look.d).len(),
                1,
                "the look that failed stored no copy"
            );
            let began = look.began;
            look.clock.runs_to(began + Duration::from_millis(599)).await;
            let early = late_line_at(&look.d, 1);
            let at_600 = look.has_late_lines_at(began + limit, 2).await;
            let stated = numbers(late_line_at(&look.d, 1));
            println!(
                "DRIVEN: the last stored look took 200 ms; the next look was asked to end, said late at 600 ms and given up by its \
                 git limit at 1,000 ms; the look that follows it was held; at 599 ms its late line: {early:?}; at 600 ms: {at_600}, \
                 stating {stated:?}"
            );
            assert!(
                early.is_none(),
                "the look that follows was said late before 600 ms: {early:?}; log:\n{}",
                look.log()
            );
            assert!(
                at_600,
                "the look that follows was not said late at 600 ms; log:\n{}",
                look.log()
            );
            assert_eq!(
                stated,
                Some((limit, limit, set, Some(stored))),
                "the late line of the look that follows states the time taken, the limit, the set limit and the last stored look, \
                 the look of 200 ms"
            );
            look.ends();
        },
    );
}

/// Point 3: a look whose late limit comes after its git limit. The set limit, and with it the period of the question, is
/// 3,000 ms, and the git work of every look has 8,500 ms. The last stored look took 2,900 ms, so the held look is late at 8,700 ms,
/// after its git limit. The tree is asked every 3,000 ms while the look's git works all the same. At 2,999 ms the test takes every
/// thread the worker could ask on. The question asked at 3,000 ms gets no answer, and the look is given up at 8,000 ms, not a
/// millisecond before, and before its git limit.
#[test]
fn b923_a_look_late_at_8700_ms_with_a_git_limit_of_8500_ms_asks_its_tree_at_3000_ms_and_is_given_up_at_8000_ms()
 {
    on_a_driven_clock_with(
        "b923_a_look_late_at_8700_ms_with_a_git_limit_of_8500_ms_asks_its_tree_at_3000_ms_and_is_given_up_at_8000_ms",
        Some(HELD_QUESTION_THREADS),
        |dir| async move {
            let ms = Duration::from_millis(1);
            let set = Duration::from_millis(3_000);
            let last = Duration::from_millis(2_900);
            let git_limit = Duration::from_millis(8_500);
            let mut look = a_look_held_after_a_stored_look(last, set, git_limit, &dir).await;
            let began = look.began;
            look.clock
                .runs_to(began + Duration::from_millis(2_999))
                .await;
            assert!(
                late_lines(&look.d).is_empty()
                    && given_up_lines(&look.d).is_empty()
                    && failed_lines(&look.d).is_empty(),
                "the look was said late, given up or failed before 3,000 ms; log:\n{}",
                look.log()
            );
            let held = TreeQuestionHeld::from_now().await;
            // 3,000 ms: the moment of the first question.
            look.clock.jumps_and_settles(ms).await;
            // 7,999 ms: one millisecond before the answer of a question asked at 3,000 ms is given up.
            look.clock
                .jumps_and_settles(Duration::from_millis(4_999))
                .await;
            let before = held
                .held_for(QUESTION_HELD_FOR, || !given_up_lines(&look.d).is_empty())
                .await;
            // 8,000 ms: a question asked at 3,000 ms has had its 5 s.
            look.clock.jumps_and_settles(ms).await;
            assert_eq!(
                DrivenClock::now(),
                began + Duration::from_millis(8_000),
                "the clock stands 8,000 ms after the look began"
            );
            let at_8000 = held
                .held_for(REAL_WORK_WITHIN, || !given_up_lines(&look.d).is_empty())
                .await;
            // 8,500 ms: the git limit of the look. For the record only: whether the look failed by then.
            look.clock
                .jumps_and_settles(Duration::from_millis(500))
                .await;
            let failed_by_8500 = held
                .held_for(QUESTION_HELD_FOR, || !failed_lines(&look.d).is_empty())
                .await;
            assert!(
                held.holds(),
                "TRIGGER PRESENT: a thread for blocking work was free while the test held them, so the question was not held; log:\n{}",
                look.log()
            );
            println!(
                "DRIVEN: the last stored look took 2,900 ms and the next look was held, late at 8,700 ms, its git limit 8,500 ms; the \
                 threads for the question were taken at 2,999 ms; given up by 7,999 ms: {before}; by 8,000 ms: {at_8000}; failed by \
                 8,500 ms: {failed_by_8500}"
            );
            assert!(
                !before,
                "the look was given up before 8,000 ms, earlier than 5 s after a question asked at 3,000 ms; log:\n{}",
                look.log()
            );
            assert!(
                at_8000,
                "the tree is asked every 3,000 ms while the look's git works, whatever its late limit, so a question asked at 3,000 \
                 ms gets no answer and the look is given up at 8,000 ms; it was not (failed by 8,500 ms: {failed_by_8500}); log:\n{}",
                look.log()
            );
            held.released().await;
            look.ends();
        },
    );
}

/// How many looks in a row [`b923_twelve_looks_in_a_row_late_at_400_ms_with_a_question_every_400_ms_are_each_said_late_before_their_question`]
/// makes the tie of a late limit equal to the period of the question in. A worker whose arms were polled in a random order would
/// let the question come first in each of them with a chance of one in two, so it passes the test once in 2^12 = 4,096 runs.
const TIES: usize = 12;

/// How far that test runs the clock past the moment a look was given up before it looks for the next one: the next look starts
/// within the spacing of the looks, five periods of the timer ([`DRIVEN_TIMER`], `snapshot::COPY_SPACING_CAP`), 50 ms, and one
/// period more when a look in between failed at once; this is more than both and less than the late limit of 400 ms.
const NEXT_LOOK_BY: Duration = Duration::from_millis(100);

/// Review of 13cb2fa3e: the tie of a late limit equal to the period of the question, in twelve looks in a row. The arms of
/// `watched_look` are polled in a fixed order (`biased;`), the late line before the question. Without that order tokio polls the four
/// arms from a random start, one of four with the same chance: from two of them (the request to end, which has not come, and the
/// question) the question comes first, so one look of two would wait for its question before it is said late. One look, as in
/// `b923_a_look_late_at_400_ms_with_a_question_every_400_ms_is_said_late_at_400_ms_and_given_up_at_5400_ms`, finds that in half the
/// runs; twelve find it in all but one of 4,096 runs ([`TIES`]).
///
/// The set limit, and with it the period of the question, is 400 ms; the last stored look took 100 ms, three times that is below the
/// set limit, so every look is late 400 ms after it began, the moment of its first question. A look that failed stores no copy, so
/// the limit stays 400 ms from look to look. In each round a look waits in `git add`; the test takes every thread the worker could ask
/// on and runs the clock by 400 ms in one jump, past the late limit and the first question of that look and before its second one
/// (the look began at most [`NEXT_LOOK_BY`] before). The late line must be written while the question waits for a thread, and state
/// the limit of 400 ms, the set limit of 400 ms and the stored look of 100 ms. The question gets no answer: 5 s later the look is
/// given up. The test gives the threads back, and the next look begins within [`NEXT_LOOK_BY`].
#[test]
fn b923_twelve_looks_in_a_row_late_at_400_ms_with_a_question_every_400_ms_are_each_said_late_before_their_question()
 {
    on_a_driven_clock_with(
        "b923_twelve_looks_in_a_row_late_at_400_ms_with_a_question_every_400_ms_are_each_said_late_before_their_question",
        Some(HELD_QUESTION_THREADS),
        |dir| async move {
            let set = Duration::from_millis(400);
            let last = Duration::from_millis(100);
            let mut look =
                a_look_held_after_a_stored_look(last, set, set + HELD_PAST_THE_LIMIT, &dir).await;
            let mut taken = Vec::new();
            for round in 1..=TIES {
                // The look of this round waits in `git add`; the file the stand-in wrote for it is taken away, so the next look
                // writes it anew.
                std::fs::remove_file(&look.held).unwrap();
                let failed_before = failed_lines(&look.d).len();
                let held = TreeQuestionHeld::from_now().await;
                // The late limit of the look and its first question.
                look.clock.jumps_and_settles(set).await;
                let tie = DrivenClock::now();
                let said = held
                    .held_for(QUESTION_HELD_FOR, || late_lines(&look.d).len() >= round)
                    .await;
                assert!(
                    said,
                    "round {round} of {TIES}: the late line comes before the question at a late limit equal to the period, so it is \
                     written while the question waits; it was not; log:\n{}",
                    look.log()
                );
                let Some(stated) = late_line_at(&look.d, round - 1) else {
                    panic!(
                        "round {round}: no late line of this look; log:\n{}",
                        look.log()
                    );
                };
                assert_eq!(
                    (stated.limit, stated.set, stated.last),
                    (set, set, Some(last)),
                    "round {round}: the late line states the limit, the set limit and the last stored look"
                );
                // The first look of the rounds began when the clock stood where the test found it; a later one began after the
                // look before it was given up and at most `NEXT_LOOK_BY` before the test found it, so the jump passed its limit by that
                // much at the most.
                assert!(
                    if round == 1 {
                        stated.taken == set
                    } else {
                        stated.taken >= set && stated.taken <= set + NEXT_LOOK_BY
                    },
                    "round {round}: the look had taken {:?} when it was said late: the limit of 400 ms, or in a later round the little \
                     more by which the jump passed it",
                    stated.taken
                );
                taken.push(stated.taken);
                // 5 s after the question: it had no answer, and the look is given up.
                look.clock.jumps_and_settles(ANSWER_WITHIN).await;
                let given_up = held
                    .held_for(REAL_WORK_WITHIN, || given_up_lines(&look.d).len() >= round)
                    .await;
                assert!(
                    held.holds(),
                    "TRIGGER PRESENT: round {round}: a thread for blocking work was free while the test held them, so the question \
                     was not held; log:\n{}",
                    look.log()
                );
                held.released().await;
                assert!(
                    given_up,
                    "round {round}: a question asked at the late limit gets no answer, so the look is given up 5 s later; it was not; \
                     log:\n{}",
                    look.log()
                );
                assert!(
                    look.clock
                        .stands_until(REAL_WORK_WITHIN, || failed_lines(&look.d).len()
                            > failed_before)
                        .await,
                    "TRIGGER PRESENT: round {round}: the look that was given up did not fail while the clock stood; log:\n{}",
                    look.log()
                );
                assert_eq!(
                    DrivenClock::now(),
                    tie + ANSWER_WITHIN,
                    "round {round}: the clock stood while the look that was given up failed"
                );
                if round < TIES {
                    look.clock.runs_to(tie + ANSWER_WITHIN + NEXT_LOOK_BY).await;
                    assert!(
                        look.clock
                            .stands_until(REAL_WORK_WITHIN, || look.held.exists())
                            .await,
                        "TRIGGER PRESENT: the look after the look of round {round} did not wait in `git add` within {NEXT_LOOK_BY:?}; \
                         log:\n{}",
                        look.log()
                    );
                }
            }
            let late = late_lines(&look.d).len();
            let given_up = given_up_lines(&look.d).len();
            println!(
                "DRIVEN: the last stored look took 100 ms; {TIES} looks in a row were held, each late at 400 ms, the period of the \
                 question, with the threads for the question taken before it; late lines: {late}, each written while the question \
                 waited, the looks having taken {taken:?}; given up: {given_up}"
            );
            // A tick that came while the question waited finds it outstanding, and that gives the look up once more before it
            // ends; so the log has one line or two of a look given up in each round.
            assert!(
                late == TIES && given_up >= TIES,
                "one late line and the look given up in each round; log:\n{}",
                look.log()
            );
            look.ends();
        },
    );
}

/// The timer of the copy task in the tests whose look a request to end starts, and the floor of a look that a tool call asks for
/// (`snapshot::COPY_FLOOR`): a minute, longer than anything those tests impose, so that after the first look of the worker has
/// stored its copy the copy task waits and starts no look of its own. The engine of the test rig ends two tool calls at the start, so
/// a tool call has asked for a look by then; that look waits for the floor, and a request to end cuts the wait short.
const QUIET_TIMER: Duration = Duration::from_secs(60);

/// The time the copy of a stop has (`snapshot::STOP_COPY_BUDGET`), written out: the moments these tests look at follow from it.
const STOP_BUDGET: Duration = Duration::from_secs(30);

/// [`the_first_look_held`] for a worker whose timer and floor are [`QUIET_TIMER`] and whose looks have [`HELD_PAST_THE_LIMIT`] for
/// their git work: returns when the first look, which the engine's first tool call asks for at the moment the test began (no look
/// came before it, so it waits for no floor), waits in `git add`.
async fn the_first_look_held_on_a_quiet_timer(set: Duration, dir: &Path) -> HeldLook {
    let clock = DrivenClock::stands();
    let first = DrivenClock::now();
    let (mark, held) = held_git_files(dir);
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "600".into())],
        edit_cfg: Some(Box::new(move |c| {
            c.direct.snapshot_interval = Some(QUIET_TIMER);
            c.direct.snapshot_floor = Some(QUIET_TIMER);
            c.direct.copy_late = Some(set);
            c.direct.copy_git_limit = Some(HELD_PAST_THE_LIMIT);
        })),
        ..Opts::default()
    })
    .await;
    // From here the tree does not answer a look; the stand-in lets the worker's first reading of the tree through.
    std::fs::write(&mark, "set\n").unwrap();
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    let look = HeldLook {
        d,
        clock,
        began: first,
        mark,
        held,
        spacing: DRIVEN_TIMER * 5 + Duration::from_millis(10),
        written: 0,
    };
    look.a_look_waits_in_git_add("the first look").await;
    assert!(
        look.log()
            .contains("direct turn copies: the first reading of the tree took "),
        "TRIGGER PRESENT: the `git add` the stand-in let through was the worker's reading of the tree before the engine started; \
         log:\n{}",
        look.log()
    );
    look.has_stored(0);
    look
}

/// A worker on the timer [`QUIET_TIMER`] with the set limit of 5,000 ms, whose first look stores a copy that took exactly `last`;
/// then the copy task waits for its timer. The test takes every thread the worker could ask on and asks the worker to end: the copy
/// task starts a look at once, at the moment the clock stands at ([`HeldLook::began`] when this returns), and that look asks the
/// file system of the tree before it starts git. The question waits for a thread, and the worker waits for its answer
/// `snapshot::TREE_ANSWER_LIMIT`, 5,000 ms, at the most: then the look fails, 5,000 ms after it began. The period of the question is
/// the set limit, so the look's first tick comes at that moment too.
async fn a_look_asked_for_by_a_request_to_end_and_held_from_its_start(
    last: Duration,
    dir: &Path,
) -> (HeldLook, TreeQuestionHeld) {
    let set = Duration::from_millis(5_000);
    let mut look = the_first_look_held_on_a_quiet_timer(set, dir).await;
    look.stores_a_copy(last, Next::HELD).await;
    assert!(
        !look
            .clock
            .stands_until(QUESTION_HELD_FOR, || look.held.exists())
            .await,
        "TRIGGER PRESENT: a look started after the stored look although the timer is a minute away; log:\n{}",
        look.log()
    );
    look.has_stored(1);
    let held = TreeQuestionHeld::from_now().await;
    let _ = look.d.app.ending.ask();
    assert!(
        held.held_for(REAL_WORK_WITHIN, || look.log().contains(TAKEN_UP))
            .await,
        "TRIGGER PRESENT: the copy task did not take the request to end up while the clock stood; log:\n{}",
        look.log()
    );
    assert_eq!(
        DrivenClock::now(),
        look.began,
        "the clock stood while the copy task took the request up and started its look"
    );
    (look, held)
}

/// What a look that a request to end asked for, held from its start ([`a_look_asked_for_by_a_request_to_end_and_held_from_its_start`]),
/// left in the worker's log 4,999 ms and 5,000 ms after it began, the stored look before it having taken `last`; `late_limit` is the
/// late limit that follows from `last`, for the record. The order of the arms of `watched_look` puts the end of the look first, so the
/// look that fails at 5,000 ms is neither said late nor asks its tree then, whichever other arm is due at that moment.
fn the_asked_look_fails_at_5000_ms_unasked(name: &str, last: Duration, late_limit: Duration) {
    on_a_driven_clock_with(name, Some(HELD_QUESTION_THREADS), move |dir| async move {
        assert_eq!(
            TREE_ANSWER_LIMIT, ANSWER_WITHIN,
            "TRIGGER PRESENT: the worker waits 5 s for the answer of the tree"
        );
        let ms = Duration::from_millis(1);
        let (look, held) =
            a_look_asked_for_by_a_request_to_end_and_held_from_its_start(last, &dir).await;
        let began = look.began;
        // 4,999 ms: the question before git has waited for a thread, and nothing has come of it.
        look.clock.jumps_and_settles(ANSWER_WITHIN - ms).await;
        let early = held
            .held_for(QUESTION_HELD_FOR, || {
                !late_lines(&look.d).is_empty()
                    || !given_up_lines(&look.d).is_empty()
                    || !failed_lines(&look.d).is_empty()
            })
            .await;
        // 5,000 ms: the question has had its 5 s and the look fails; its first tick comes now too, and its late limit when it is
        // 5,000 ms.
        look.clock.jumps_and_settles(ms).await;
        assert_eq!(
            DrivenClock::now(),
            began + ANSWER_WITHIN,
            "the clock stands 5,000 ms after the look began"
        );
        let failed = held
            .held_for(REAL_WORK_WITHIN, || {
                look.log()
                    .contains("direct turn session ending: no copy of the work could be saved")
            })
            .await;
        assert!(
            held.holds(),
            "TRIGGER PRESENT: a thread for blocking work was free while the test held them, so the question was not held; log:\n{}",
            look.log()
        );
        let late = late_lines(&look.d);
        let asked = given_up_lines(&look.d);
        let fails = failed_lines(&look.d);
        println!(
            "DRIVEN: the stored look took {last:?}; a request to end started a look whose question before git was held from its start, \
             late at {late_limit:?}, a question every 5,000 ms; anything by 4,999 ms: {early}; failed at 5,000 ms: {failed}; late \
             lines: {}; questions that gave it up: {}; failed lines: {fails:?}",
            late.len(),
            asked.len()
        );
        assert!(
            !early,
            "the look was said late, asked its tree or failed before its question had waited 5 s; log:\n{}",
            look.log()
        );
        assert!(
            failed && fails.len() == 1 && fails[0].contains("did not answer within 5"),
            "TRIGGER PRESENT: the look failed at 5,000 ms because its question before git had no answer; log:\n{}",
            look.log()
        );
        assert!(
            late.is_empty(),
            "the end of the look comes before its late line in the order of the arms, so a look that ends at its late limit is not \
             said late; it was: {late:?}; log:\n{}",
            look.log()
        );
        assert!(
            asked.is_empty(),
            "the end of the look comes before the question in the order of the arms, so a look that ends at a tick does not ask its \
             tree; it did: {asked:?}; log:\n{}",
            look.log()
        );
        held.released().await;
        look.ends();
    });
}

/// Review of 13cb2fa3e: the end of a look comes first in the order of the arms of `watched_look`. The set limit, and with it
/// the period of the question, is 5,000 ms; the stored look took 100 ms, three times that is below the set limit, so the look that
/// a request to end then asks for is late at 5,000 ms. Its question before git is held from its start, and it fails 5,000 ms after it
/// began, the moment of its late limit and of its first tick. It is not said late, and it does not ask its tree.
#[test]
fn b923_a_look_that_ends_at_its_late_limit_and_a_tick_of_5000_ms_is_not_said_late_and_does_not_ask_its_tree()
 {
    the_asked_look_fails_at_5000_ms_unasked(
        "b923_a_look_that_ends_at_its_late_limit_and_a_tick_of_5000_ms_is_not_said_late_and_does_not_ask_its_tree",
        Duration::from_millis(100),
        Duration::from_millis(5_000),
    );
}

/// Review of 13cb2fa3e: as the test above with a stored look of 2,000 ms, so the look that a request to end asks for is late
/// at 6,000 ms, three times that, and fails at 5,000 ms, the moment of its first tick alone. It does not ask its tree.
#[test]
fn b923_a_look_that_ends_at_a_tick_of_5000_ms_before_its_late_limit_of_6000_ms_does_not_ask_its_tree()
 {
    the_asked_look_fails_at_5000_ms_unasked(
        "b923_a_look_that_ends_at_a_tick_of_5000_ms_before_its_late_limit_of_6000_ms_does_not_ask_its_tree",
        Duration::from_millis(2_000),
        Duration::from_millis(6_000),
    );
}

/// A thread for blocking work that the test took again after one question of the worker went through it
/// ([`one_question_goes_through`]).
struct ThreadTakenAgain {
    go: std::sync::mpsc::Sender<()>,
    parked: tokio::task::JoinHandle<()>,
}

impl ThreadTakenAgain {
    /// The test gives the thread back: a question that waited for it runs.
    async fn let_go(self) {
        drop(self.go);
        let _ = self.parked.await;
    }
}

/// While the test holds the threads for blocking work ([`TreeQuestionHeld`]) and a question of the worker waits for one, the test
/// lets that question through and takes the thread again before the worker can ask another. The pool of threads for blocking work
/// runs what waits in the order it was given: the test queues a task of its own behind the question, gives one thread back, and
/// returns when its task runs on it, so the question has run before, and a question the worker asks from then on waits again.
async fn one_question_goes_through(held: &mut TreeQuestionHeld) -> ThreadTakenAgain {
    let (runs, ran) = std::sync::mpsc::channel::<()>();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let parked = tokio::task::spawn_blocking(move || {
        let _ = runs.send(());
        let _ = wait.recv();
    });
    let (Some(free), Some(task)) = (held.go.pop(), held.parked.pop()) else {
        panic!("the test holds no thread to give back");
    };
    drop(free);
    let _ = task.await;
    let began = Instant::now();
    while ran.try_recv().is_err() {
        assert!(
            began.elapsed() < REAL_WORK_WITHIN,
            "TRIGGER PRESENT: the task the test queued behind the question did not run on the thread it gave back"
        );
        std::thread::sleep(Duration::from_millis(1));
        tokio::task::yield_now().await;
    }
    ThreadTakenAgain { go, parked }
}

/// Review of 13cb2fa3e: a request to end comes before the question in the order of the arms of `watched_look`. The set limit,
/// and with it the period of the question, is 400 ms; the stored look took 100 ms, so the held look is late at 400 ms. At 399 ms the
/// test takes every thread the worker could ask on: the question asked at 400 ms waits for a thread. At 600 ms the worker is asked to
/// end, and at 800 ms the next tick of the question comes: neither is taken up while the question waits. At 1,000 ms the test lets
/// that question through and takes its thread again at once. The request to end is taken up when the question has been answered, at
/// 1,000 ms, before a second question is asked. It counts the time of the copy of a stop, 30 s, from then: the look, which still
/// waits in `git add`, is given up at 31,000 ms, not a millisecond before, 30,400 ms after the request, within that time and the
/// 5 s a question has.
#[test]
fn b923_a_request_to_end_that_comes_with_a_tick_while_a_question_waits_is_taken_up_when_the_question_ends()
 {
    on_a_driven_clock_with(
        "b923_a_request_to_end_that_comes_with_a_tick_while_a_question_waits_is_taken_up_when_the_question_ends",
        Some(HELD_QUESTION_THREADS),
        |dir| async move {
            let ms = Duration::from_millis(1);
            let set = Duration::from_millis(400);
            let last = Duration::from_millis(100);
            let mut look =
                a_look_held_after_a_stored_look(last, set, set + HELD_PAST_THE_LIMIT, &dir).await;
            assert_eq!(
                (STOP_COPY_BUDGET, look.d.app.cfg.direct.stop_copy_budget),
                (STOP_BUDGET, None),
                "TRIGGER PRESENT: the copy of a stop has 30 s, and the test rig sets no other time"
            );
            let began = look.began;
            look.clock.runs_to(began + Duration::from_millis(399)).await;
            let mut held = TreeQuestionHeld::from_now().await;
            // 400 ms: the late line, then the question, which waits for a thread.
            look.clock.jumps_and_settles(ms).await;
            assert!(
                held.held_for(QUESTION_HELD_FOR, || late_lines(&look.d).len() == 1)
                    .await,
                "TRIGGER PRESENT: the look was said late at 400 ms before its question; log:\n{}",
                look.log()
            );
            // 600 ms: the request to end comes while the question waits.
            look.clock
                .jumps_and_settles(Duration::from_millis(200))
                .await;
            let request = DrivenClock::now();
            let _ = look.d.app.ending.ask();
            let taken_while_asking = held
                .held_for(QUESTION_HELD_FOR, || look.log().contains(TAKEN_UP))
                .await;
            // 1,000 ms: the tick of 800 ms has come too, and the question goes through.
            look.clock
                .jumps_and_settles(Duration::from_millis(400))
                .await;
            let answered = DrivenClock::now();
            let taken_again = one_question_goes_through(&mut held).await;
            let taken = held
                .held_for(REAL_WORK_WITHIN, || look.log().contains(TAKEN_UP))
                .await;
            assert_eq!(
                DrivenClock::now(),
                answered,
                "the clock stood while the test looked for the request to be taken up"
            );
            taken_again.let_go().await;
            held.released().await;
            // One millisecond before the copy of the stop has had its 30 s, counted from when the request was taken up.
            let end_by = answered + STOP_BUDGET;
            look.clock.runs_to(end_by - ms).await;
            let ended = |d: &Direct| {
                d.log
                    .lines()
                    .join("\n")
                    .contains("direct turn session ending: no copy of the work could be saved")
            };
            let early = ended(&look.d) || !failed_lines(&look.d).is_empty();
            look.clock.runs_to(end_by).await;
            let at_end = look
                .clock
                .stands_until(REAL_WORK_WITHIN, || ended(&look.d))
                .await;
            println!(
                "DRIVEN: the last stored look took 100 ms and the next look was held, late at 400 ms; its question at 400 ms waited; \
                 the request to end came at 600 ms; taken up while the question waited: {taken_while_asking}; the question went \
                 through at 1,000 ms; taken up then: {taken}; the look ended by 30,999 ms: {early}; at 31,000 ms: {at_end}, {:?} after \
                 the request",
                end_by - request
            );
            assert!(
                !taken_while_asking,
                "the request to end waits for the question that is under way; it was taken up while the question waited; log:\n{}",
                look.log()
            );
            assert!(
                taken,
                "a request to end comes before the question in the order of the arms, so it is taken up when the question that \
                 was under way has been answered, before a second one; it was not (the tick of 800 ms had come too); log:\n{}",
                look.log()
            );
            assert!(
                !early,
                "the look ended before the copy of the stop had had its 30 s from when the request was taken up; log:\n{}",
                look.log()
            );
            assert!(
                at_end,
                "the look that waits in `git add` is given up when the copy of the stop has had its 30 s from when the request was \
                 taken up, 31,000 ms; it was not; log:\n{}",
                look.log()
            );
            assert!(
                end_by - request <= STOP_BUDGET + ANSWER_WITHIN,
                "the look ended within the time of the copy of a stop and the time a question has, counted from the request"
            );
            look.ends();
        },
    );
}
