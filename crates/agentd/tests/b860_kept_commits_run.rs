//! The copy task around the keep of the run's commits, with whole turns: a real worker process, the stand-in engine, the
//! stand-in bucket and a stand-in for git that holds the keep's own `pack-objects`.
//!
//! Root invariant 9 binds every case: a keep never stands between a look and its copy and never makes a copy later. Ruling 10 of the
//! lead: the tests assert the next stored copy, by when it is stored and by which work it holds (a file whose content names the
//! moment it was written), never the moment a signal was sent:
//! - a look that falls due while the keep's git runs ends the keep, and its copy is stored;
//! - the copy of a look is in the bucket, bundle and record, before the keep's git starts;
//! - a tool call's wake that is already pending when the keep would start is taken as before, and no git of a keep is started;
//! - a tool call that ends while the keep's git runs ends the keep, and a later look by the timer keeps;
//! - a request to end while the keep's git runs stores its copy inside the time a stop has, and one right after a commit starts no keep;
//! - a worker that is killed inside the keep leaves this look's copy in the bucket, restorable through its record;
//! - a worker that starts on a kept pack with wrong bytes says once that the pack is gone, and its first copy restores;
//! - the line of the first reading of the tree names what git's check of a kept pack cost the start of the turn;
//! - a look of the timer whose keep had not started when the engine ended its turn has its line once, with `keep=0`, and
//!   the line of the copy of the end follows it.
//!
//! The worker's log line of a look is read here too: one line per look that stored a copy or failed, with what started the look,
//! the times of its steps, and what came of the store and of the keep; and the line of the copy of the end, `by=final`.
//!
//! No test here fails because the machine is crowded. A keep follows a look that the timer started, and all tests but two bring
//! that look about without waiting for a tick of a short timer that a slow step could miss: the timer is ten minutes, the looks
//! before it are the ones tool calls ask for, the bucket refuses one copy, and the tick that tries a refused copy again
//! (`AGENTD_DIRECT_SNAPSHOT_RETRY_MS`) is the timer's. What a test waits for has two minutes, and what it asserts is which work a
//! copy holds and what started its look, which a slow machine does not change. Of the two tests with a short timer, one asserts
//! that a copy comes at all while the keep's git is held without end. The other needs the copy task to take a wake that
//! is pending before it turns to the keep, which the runtime chooses one time in two: it runs turns until one shows that, and a
//! turn that does not is left, never failed.
#![allow(clippy::unwrap_used)]

mod b860_kept_support;
mod b860_thin_support;
mod common;
mod direct_support;

use b860_kept_support::{alive, keeps_started, names, stand_in_for_git};
use common::{git, scratch};
use direct_support::{Direct, Opts, Process, RUN, checkout_of_copy_bytes};
use harness_protocol::direct::s3 as keys;
use harness_protocol::envelope::EventType;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// What a test waits for at most: far more than a crowded machine needs, so that only a copy that never comes fails a wait.
const LONG: Duration = Duration::from_secs(120);
/// A timer of the copies that no test waits out: ten minutes.
const NO_TICK_MS: u64 = 600_000;
/// The timer of the two tests whose looks are the timer's: 3 s.
const DUE_EVERY_MS: u64 = 3_000;
/// What the worker gives a git that it has ended to be gone (`GIT_END_LIMIT` in the worker's `workspace.rs`): 2 s.
const GIT_END: Duration = Duration::from_secs(2);
/// How many turns the test of the line that waits for its keep runs at most to see its trigger. A turn shows it one time
/// in two, so all of them miss it one time in a million.
const PENDING_TURNS: u32 = 20;
/// How long that test holds the `git add` of the look a keep follows. The spacing after a look is nine times its git work,
/// at most five periods of a timer that a test set: 15 s after a look held this long.
const ADD_HELD: Duration = Duration::from_secs(2);
/// How long that test waits for the line of a look whose copy is stored before it takes the line to wait for its keep. A
/// line that the copy task's wait wrote is in the log the moment the store has answered.
const PENDING_GRACE: Duration = Duration::from_secs(2);

/// Waits until `seen` holds, at most `limit`; `false` when it never did.
async fn wait_for(limit: Duration, mut seen: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while !seen() {
        if began.elapsed() > limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// A process of the stand-in engine that a turn of these tests started: its number, and the time the system says it started at
/// (field 22 of its `stat`), so that a number the system gave to another process is never taken for it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Engine {
    pid: u32,
    started: String,
}

/// When the process `pid` started, as the system says it (field 22 of `/proc/<pid>/stat`), if it is a process of the stand-in
/// engine that still runs; `None` for any other process, for one that has ended and for none.
fn engine_started(pid: u32) -> Option<String> {
    let command = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if !String::from_utf8_lossy(&command).contains("fake-acp-engine") {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The fields after the command's name in brackets start with the third (the state); the start time is the 22nd.
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(19).map(str::to_string)
}

/// The processes of the stand-in engine below the process `root`: among its children, their children and so on.
fn engines_below(root: u32) -> Vec<Engine> {
    let mut found = Vec::new();
    let mut todo = vec![root];
    while let Some(pid) = todo.pop() {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            continue;
        };
        for task in tasks.flatten() {
            let children =
                std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
            for child in children
                .split_whitespace()
                .filter_map(|c| c.parse::<u32>().ok())
            {
                if let Some(started) = engine_started(child) {
                    found.push(Engine {
                        pid: child,
                        started,
                    });
                }
                todo.push(child);
            }
        }
    }
    found
}

/// Whether the engine `engine` still runs: the process with its number that started when it started.
fn engine_runs(engine: &Engine) -> bool {
    engine_started(engine.pid).as_deref() == Some(engine.started.as_str())
}

/// One turn of a real worker whose engine says nothing until the test lets it, and whose git calls go through the stand-in.
struct Run {
    d: Direct,
    /// The folder of the stand-in for git and of the files that steer it.
    dir: PathBuf,
    /// The file of `FAKE_ACP_STEP_FILE`: how many messages of its reply the engine may have written.
    steps: PathBuf,
    /// How many messages the engine may have written so far.
    said: AtomicU64,
    checkouts: AtomicUsize,
    /// The processes of the stand-in engine that the worker of this turn started ([`engines_below`]), found once the engine had
    /// its prompt. The drop of the turn ends them.
    engines: Vec<Engine>,
}

impl Drop for Run {
    /// Ends the stand-in engine of the turn. The drop of the worker ends `agentd serve` with KILL, and a worker that is ended
    /// that way cannot end the engine it started: the engine of these tests waits for the file that lets it speak, and stayed
    /// behind after every test whose worker did not end by itself. Each process found below the worker is ended when the
    /// process with its number still is the one that was found.
    fn drop(&mut self) {
        for engine in self.engines.iter().filter(|engine| engine_runs(engine)) {
            let _ = std::process::Command::new("kill")
                .args(["-s", "KILL", &engine.pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

impl Run {
    /// Starts the turn with the timer of the copies at `every_ms`. The tick that tries a refused copy again comes after 1.5 s.
    async fn start(label: &str, every_ms: u64, more: Vec<(&'static str, String)>) -> Self {
        Self::start_on(label, every_ms, more, |_, _| {}).await
    }

    /// [`Self::start`], with `before` called when the worker's process runs and has no turn yet: what it leaves in the worker's
    /// data folder is what the worker finds when it opens its repositories. It is given the worker and the test's folder.
    async fn start_on(
        label: &str,
        every_ms: u64,
        more: Vec<(&'static str, String)>,
        before: impl FnOnce(&Direct, &Path),
    ) -> Self {
        let dir = scratch(label);
        let steps = dir.join("engine-steps");
        std::fs::write(&steps, "0\n").unwrap();
        let bin = stand_in_for_git(&dir);
        let mut agentd_env = vec![
            ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", every_ms.to_string()),
            ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
            ("AGENTD_DIRECT_SNAPSHOT_RETRY_MS", "1500".to_string()),
            ("AGENTD_DIRECT_UPLOAD_PAUSE_MS", "20".to_string()),
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ];
        agentd_env.extend(more);
        let d = Direct::start_process(Opts {
            scenario: "four_edits",
            engine_env: vec![("FAKE_ACP_STEP_FILE", steps.display().to_string())],
            agentd_env,
            ..Opts::default()
        })
        .await;
        before(&d, &dir);
        let (code, text) = d.post().await;
        assert_eq!(code, 200, "{text}");
        let mut run = Self {
            d,
            dir,
            steps,
            said: AtomicU64::new(0),
            checkouts: AtomicUsize::new(0),
            engines: Vec::new(),
        };
        assert!(
            wait_for(LONG, || !run.d.prompts_received().is_empty()).await,
            "the engine got its prompt"
        );
        run.engines = engines_below(run.d.process.as_ref().unwrap().pid());
        run
    }

    /// How many tool calls agentd has seen end.
    fn calls_ended(&self) -> usize {
        self.d
            .events_of(EventType::TurnProgress)
            .iter()
            .filter(|v| {
                v.get("toolCallId").is_some_and(|id| !id.is_null()) && v["status"] == "completed"
            })
            .count()
    }

    /// The engine ends its next tool call (two more messages of its reply), and agentd has seen it end: the copy task is woken.
    async fn tool_call_ends(&self) {
        let ended = self.calls_ended();
        let said = self.said.fetch_add(2, Ordering::SeqCst) + 2;
        std::fs::write(&self.steps, format!("{said}\n")).unwrap();
        assert!(
            wait_for(LONG, || self.calls_ended() > ended).await,
            "TRIGGER PRESENT is not shown: the engine ended no tool call; log:\n{}",
            self.log()
        );
    }

    /// Every bundle of a copy the bucket has stored, oldest first.
    fn bundles(&self) -> Vec<Vec<u8>> {
        self.d
            .aws
            .versions_of(&keys::snapshot_bundle_key(RUN))
            .into_iter()
            .map(|(_, bytes)| bytes)
            .collect()
    }

    /// Every record of a copy the bucket has stored, oldest first.
    fn records(&self) -> Vec<serde_json::Value> {
        self.d
            .aws
            .versions_of(&keys::snapshot_json_key(RUN))
            .iter()
            .map(|(_, b)| serde_json::from_slice(b).unwrap())
            .collect()
    }

    /// How many copies are stored: a copy is stored once its record is.
    fn stored(&self) -> usize {
        self.records().len()
    }

    fn log(&self) -> String {
        self.d
            .process
            .as_ref()
            .map(Process::log_text)
            .unwrap_or_default()
    }

    /// The worker's log lines of its looks.
    fn look_lines(&self) -> Vec<String> {
        self.log()
            .lines()
            .filter(|l| l.contains("direct turn copy look n="))
            .map(str::to_string)
            .collect()
    }

    /// The log line of the look that stored the copy with index `index` (from 0): the line that names the number its record has.
    fn line_of(&self, index: usize) -> String {
        let n = self.records()[index]["count"].to_string();
        self.look_lines()
            .into_iter()
            .find(|l| l.contains(" stored ") && field(l, "n") == Some(n.as_str()))
            .unwrap_or_default()
    }

    /// [`Self::line_of`] for a copy that [`Self::wait_copy`] has just found, once that copy is stored and its look is logged. The
    /// bucket has the bundle of a copy before it has the record, and the worker writes the line of a look after the store has
    /// answered: right after the bundle is there, neither need be. Waits at most [`LONG`]; the line is empty when it never came,
    /// and the assertion on it fails.
    async fn line_when_stored(&self, index: usize) -> String {
        wait_for(LONG, || {
            self.stored() > index && !self.line_of(index).is_empty()
        })
        .await;
        if self.stored() > index {
            self.line_of(index)
        } else {
            String::new()
        }
    }

    /// The file `file` of the stored copy number `index` (from 0), as a checkout reads it that has only the bundle the run started
    /// from beside the copy; `None` when the copy does not hold the file.
    fn file_of(&self, index: usize, file: &str) -> Option<String> {
        let n = self.checkouts.fetch_add(1, Ordering::SeqCst);
        let bundles = self.bundles();
        let wt = checkout_of_copy_bytes(
            &self.d.aws,
            &self.dir,
            &bundles[index],
            &format!("checkout-{n}"),
        );
        std::fs::read_to_string(wt.join(file)).ok()
    }

    /// Waits, at most `limit`, for a stored copy from index `from` on whose file `file` reads `content`; the answer is the index
    /// of that copy. Each copy is checked out once.
    async fn wait_copy(
        &self,
        from: usize,
        file: &str,
        content: &str,
        limit: Duration,
    ) -> Option<usize> {
        let mut next = from;
        let mut found = None;
        wait_for(limit, || {
            let stored = self.bundles().len();
            while next < stored {
                if self.file_of(next, file).as_deref() == Some(content) {
                    found = Some(next);
                    return true;
                }
                next += 1;
            }
            false
        })
        .await;
        found
    }

    fn write(&self, file: &str, content: &str) {
        std::fs::write(self.d.worktree().join(file), content).unwrap();
    }

    /// Writes `content` into `file` of the work tree and waits, at most `limit`, for a stored copy that holds it; the answer is the
    /// index of that copy.
    async fn write_and_wait(&self, file: &str, content: &str, limit: Duration) -> Option<usize> {
        let from = self.bundles().len();
        self.write(file, content);
        self.wait_copy(from, file, content, limit).await
    }

    /// The engine commits in the work tree: its head is above the commit the run started from.
    fn commit(&self, round: u32) -> String {
        let wt = self.d.worktree();
        std::fs::create_dir_all(wt.join("run")).unwrap();
        std::fs::write(
            wt.join(format!("run/file{round}.txt")),
            format!("the file of round {round}\n"),
        )
        .unwrap();
        git(&wt, &["add", "run"]);
        git(&wt, &["commit", "-q", "-m", &format!("round {round}")]);
        git(&wt, &["rev-parse", "HEAD"])
    }

    /// The stand-in holds the keep's `pack-objects` from now on.
    fn hold_the_keep(&self) {
        let _ = std::fs::remove_file(self.dir.join("keep-running"));
        std::fs::write(self.dir.join("keep-hold"), "").unwrap();
    }

    /// The stand-in holds the `git add` of the next look from now on; `add-running` is there once it does.
    fn hold_the_add(&self) {
        let _ = std::fs::remove_file(self.dir.join("add-running"));
        std::fs::write(self.dir.join("add-hold"), "").unwrap();
    }

    /// The held `git add` goes on.
    fn let_the_add_go(&self) {
        let _ = std::fs::remove_file(self.dir.join("add-hold"));
    }

    /// Waits for the held `pack-objects` of the keep that follows the stored copy with index `index`. The copy of that look was
    /// stored before the keep's git started: its bundle and its record are in the bucket. The answer is the process id of the call.
    async fn the_held_keep(&self, index: usize) -> u32 {
        assert!(
            wait_for(LONG, || self.dir.join("keep-running").exists()).await,
            "TRIGGER PRESENT: no keep was started after the second look in a row, by the timer, of the same head; log:\n{}",
            self.log()
        );
        assert_eq!(
            (self.bundles().len(), self.stored()),
            (index + 1, index + 1),
            "TRIGGER PRESENT: the keep's pack-objects started before the copy of its look was stored"
        );
        std::fs::read_to_string(self.dir.join("keep-running"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// Brings about the look that a keep follows, without a tick of a short timer. The engine commits; the look a tool call asks
    /// for finds the new head; the look a second tool call asks for finds it again, and the bucket refuses its copy at every try;
    /// the tick that tries the copy again is the timer's, and its look finds the same head for the second time running and stores
    /// its copy. The answer is the index of that copy. Two tool calls of the engine are used.
    async fn to_the_look_a_keep_follows(&self, round: u32, file: &str) -> usize {
        self.commit(round);
        let from = self.bundles().len();
        self.write(file, "found by the look of a tool call\n");
        self.tool_call_ends().await;
        assert!(
            self.wait_copy(from, file, "found by the look of a tool call\n", LONG)
                .await
                .is_some(),
            "the look that finds the engine's new head stores its copy; log:\n{}",
            self.log()
        );
        let key = keys::snapshot_bundle_key(RUN);
        let attempts = self.d.aws.put_attempts(&key);
        let from = self.bundles().len();
        self.d.aws.fail_puts("snapshot.bundle", 4);
        let refused =
            "in the copy the bucket refused, and in the copy of the tick that tried again\n";
        self.write(file, refused);
        self.tool_call_ends().await;
        assert!(
            wait_for(LONG, || self.d.aws.put_attempts(&key) >= attempts + 4).await,
            "TRIGGER PRESENT is not shown: the bucket did not refuse the copy of the second tool call's look; log:\n{}",
            self.log()
        );
        self.wait_copy(from, file, refused, LONG)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "the tick that tries a refused copy again stores it; log:\n{}",
                    self.log()
                )
            })
    }

    /// How many looks the log names whose keep ran to its end and kept a pack: `keep=` is a number above 0 and `kept=` is.
    fn kept_lines(&self) -> usize {
        self.look_lines()
            .iter()
            .filter(|l| {
                field(l, "keep").is_some_and(|v| v.parse::<u64>().is_ok_and(|ms| ms > 0))
                    && field(l, "kept")
                        .is_some_and(|v| v.parse::<u64>().is_ok_and(|bytes| bytes > 0))
            })
            .count()
    }

    /// Lets a held keep go and waits for its git to end.
    async fn let_go(&self, pid: u32) {
        let _ = std::fs::remove_file(self.dir.join("keep-hold"));
        wait_for(Duration::from_secs(30), || !alive(pid)).await;
    }
}

/// The value of `name=` in a log line of a look.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(name)?.strip_prefix('='))
}

/// The process `pid` is gone within thirty seconds.
async fn gone(pid: u32) -> bool {
    wait_for(Duration::from_secs(30), || !alive(pid)).await
}

/// A look that falls due while the keep's git runs ends the keep, and its copy, which holds what was written while the keep ran,
/// is stored: the keep never holds the copy task. The stand-in holds the keep's git without end, so a copy task that waited for the
/// keep would store no copy at all. The copy of the look before the keep was in the bucket before the keep's git started. A keep
/// that the next look cut is not tried again for the same head. The worker's log has one line for each look that stored a copy or
/// failed, with every field once.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_look_that_is_due_ends_the_keep_and_its_copy_is_not_later() {
    // The one test with a short timer: 3 s. Its looks are the timer's.
    let mut run = Run::start("b860-kept-due", DUE_EVERY_MS, Vec::new()).await;
    assert!(
        wait_for(LONG, || run.stored() >= 1).await,
        "the copy of the session's start is stored; log:\n{}",
        run.log()
    );
    run.hold_the_keep();
    // A look has just ended when its copy is seen: the commit and the write after it fall before the same next look.
    assert!(
        run.write_and_wait("a.txt", "written to stand right behind a look\n", LONG)
            .await
            .is_some()
    );
    run.commit(1);
    assert!(
        run.write_and_wait("a.txt", "first\n", LONG).await.is_some(),
        "the look that finds the engine's new head stores its copy; log:\n{}",
        run.log()
    );
    let second = run
        .write_and_wait("a.txt", "second\n", LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the second look of the same head stores its copy; log:\n{}",
                run.log()
            )
        });
    let pid = run.the_held_keep(second).await;
    let seen = Instant::now();

    // The next look is due while the keep's git is held.
    let third = "third, written while the keep's git ran\n";
    let from = run.bundles().len();
    run.write("a.txt", third);
    let stored = wait_for(LONG, || run.stored() > from).await;
    let took = seen.elapsed();
    assert!(
        stored && run.wait_copy(from, "a.txt", third, LONG).await == Some(from),
        "TRIGGER PRESENT: the copy of the look that fell due while the keep ran was not stored within {LONG:?} (the timer is 3 s): \
         the keep held the copy task; log:\n{}",
        run.log()
    );
    // When that copy is stored at the latest is what the product itself states, not two minutes (ruling 10 of the lead). The look
    // starts at the tick of the timer, one period after the start of the look before it, or nine times the git work of that look
    // after its end when that is later, at most five periods of a timer that a test set (`Cadence::delay` in the worker's
    // `direct/snapshot.rs`). A keep adds nothing to it: what starts the look drops the keep, and the keep's git is told to end and
    // is not waited for; a git that the worker ends has two seconds to be gone (`GIT_END_LIMIT` in the worker's `workspace.rs`),
    // and that is the allowance here. Then the look takes what its own log line says it took. All of it is counted from the
    // moment the keep's git was seen running, which is after the look before had ended. A keep that held this look for ten
    // seconds fails here on a machine whose looks take the tenth of a second they take alone.
    assert!(
        wait_for(LONG, || !run.line_of(from).is_empty()
            && !run.line_of(second).is_empty())
        .await,
        "the worker's log has the lines of both looks; lines:\n{}",
        run.look_lines().join("\n")
    );
    let (before, line) = (run.line_of(second), run.line_of(from));
    let ms = |line: &str, name: &str| -> u64 {
        field(line, name)
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("the field {name} of the line: {line}"))
    };
    assert_eq!(
        field(&line, "by"),
        Some("timer"),
        "the look that fell due was the timer's: {line}"
    );
    let git_before = ms(&before, "total").saturating_sub(ms(&before, "store"));
    let wait = (9 * git_before).clamp(DUE_EVERY_MS, 5 * DUE_EVERY_MS);
    let allowed = Duration::from_millis(wait + ms(&line, "total")) + GIT_END;
    assert!(
        took <= allowed,
        "TRIGGER PRESENT: the copy of the look that fell due while the keep's git ran was stored {took:?} after that git was seen \
         running. The timer, the spacing of the looks, the time a git has to end and the time the look's own line names allow \
         {allowed:?}: the keep held the look. The line of the look before, then of this look:\n{before}\n{line}"
    );
    // Inside the look: the time of its line that is no step of its own is the wait for the file system's answer and for the
    // private repository, which a keep must not hold.
    let steps: u64 = ["tidy", "tree", "head", "commit", "bundle", "hash", "store"]
        .iter()
        .map(|name| ms(&line, name))
        .sum();
    let waited = Duration::from_millis(ms(&line, "total").saturating_sub(steps));
    assert!(
        waited <= GIT_END,
        "TRIGGER PRESENT: the look that fell due while the keep's git ran waited {waited:?} that are no step of its own: the \
         private repository was not free for it: {line}"
    );
    assert!(
        gone(pid).await,
        "TRIGGER PRESENT: the keep's git was not ended when the next look was due"
    );
    assert!(
        run.look_lines()
            .iter()
            .any(|l| field(l, "keep") == Some("ended")),
        "the line of the look whose keep was cut says so; lines:\n{}",
        run.look_lines().join("\n")
    );
    run.let_go(pid).await;
    // Cut by the next look: one try for this head.
    for content in ["fourth\n", "fifth\n"] {
        assert!(run.write_and_wait("a.txt", content, LONG).await.is_some());
    }
    assert_eq!(
        keeps_started(&run.dir),
        1,
        "a keep that the next look cut is not tried again for the same head"
    );

    // A keep that git fails: the head moves and comes to rest, the stand-in fails the keep's pack-objects, and the log says why.
    std::fs::write(run.dir.join("keep-fail"), "").unwrap();
    run.commit(2);
    for content in ["sixth\n", "seventh\n", "eighth\n"] {
        assert!(run.write_and_wait("a.txt", content, LONG).await.is_some());
    }
    assert!(
        wait_for(LONG, || run
            .log()
            .contains("direct turn copy keep missed: "))
        .await,
        "TRIGGER PRESENT: a keep that git failed is not said in the worker's log; lines:\n{}",
        run.look_lines().join("\n")
    );
    assert!(
        run.look_lines()
            .iter()
            .any(|l| field(l, "keep") == Some("missed")),
        "the line of the look whose keep was missed says so; lines:\n{}",
        run.look_lines().join("\n")
    );
    assert!(
        run.write_and_wait("a.txt", "ninth\n", LONG).await.is_some(),
        "a missed keep changes no copy"
    );
    assert_eq!(keeps_started(&run.dir), 2, "one try for the second head");

    // A look that fails: the bucket refuses the bundle at every try of one store.
    run.d.aws.fail_puts("snapshot.bundle", 4);
    run.write("a.txt", "the bucket refuses this copy\n");
    assert!(
        wait_for(LONG, || run.look_lines().iter().any(|l| l
            .contains(" failed ")
            && field(l, "why") == Some("store")))
        .await,
        "a look whose store failed has its line; lines:\n{}",
        run.look_lines().join("\n")
    );
    assert!(
        run.write_and_wait("a.txt", "stored again\n", LONG)
            .await
            .is_some()
    );

    // One line per look that stored a copy or failed, each field once.
    assert!(
        wait_for(LONG, || run
            .look_lines()
            .iter()
            .filter(|l| l.contains(" stored "))
            .count()
            == run.stored())
        .await,
        "one line per stored copy ({} copies); lines:\n{}",
        run.stored(),
        run.look_lines().join("\n")
    );
    let lines = run.look_lines();
    for line in &lines {
        for name in [
            "n", "by", "engine", "total", "tidy", "tree", "head", "commit", "bundle", "hash",
            "store", "keep", "bytes", "kept",
        ] {
            let times = line
                .split_whitespace()
                .filter(|w| {
                    w.strip_prefix(name)
                        .is_some_and(|rest| rest.starts_with('='))
                })
                .count();
            assert_eq!(times, 1, "the field {name} of the line: {line}");
        }
        assert!(
            ["timer", "tool", "retry", "stop"].contains(&field(line, "by").unwrap()),
            "what started the look: {line}"
        );
        assert!(
            ["moved", "same", "gone"].contains(&field(line, "engine").unwrap()),
            "{line}"
        );
        assert!(
            !line.contains("cpu_ms"),
            "no processor time that is not this look's own: {line}"
        );
    }
    assert!(lines.iter().any(|l| field(l, "engine") == Some("moved")));
    assert!(lines.iter().any(|l| field(l, "engine") == Some("same")));
    assert!(lines.iter().any(|l| field(l, "by") == Some("timer")));

    // The worker ends as a signal asks, so that it ends by itself.
    run.d.process.as_ref().unwrap().signal("TERM");
    assert_eq!(
        exit_of(&mut run, LONG).await,
        Some(Some(143)),
        "log:\n{}",
        run.log()
    );
}

/// A tool call that ends while the keep's git runs ends that keep: the copy its wake asks for holds what was written then, and the
/// look that stored it was the tool call's. The keep that a tool call cut is tried again: the next look by the timer of the same
/// head keeps a pack.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_tool_call_ends_the_keep() {
    let mut run = Run::start("b860-kept-tool", NO_TICK_MS, Vec::new()).await;
    run.hold_the_keep();
    let index = run.to_the_look_a_keep_follows(1, "b.txt").await;
    let pid = run.the_held_keep(index).await;
    assert_eq!(
        field(&run.line_of(index), "by"),
        None,
        "the line of that look waits for its keep"
    );

    let at_the_call = "written when the third tool call ended\n";
    let from = run.bundles().len();
    run.write("b.txt", at_the_call);
    run.tool_call_ends().await;
    let found = run.wait_copy(from, "b.txt", at_the_call, LONG).await;
    assert!(
        found.is_some(),
        "TRIGGER PRESENT: no copy holds the work of the moment the tool call ended (the timer is ten minutes): the keep held the \
         copy task; log:\n{}",
        run.log()
    );
    assert!(
        gone(pid).await,
        "TRIGGER PRESENT: the keep's git was not ended when a tool call ended"
    );
    let line = run.line_when_stored(found.unwrap()).await;
    assert_eq!(
        field(&line, "by"),
        Some("tool"),
        "the look that stored that copy was the tool call's: {line}"
    );
    assert_eq!(
        field(&run.line_of(index), "keep"),
        Some("ended"),
        "the line of the look whose keep the tool call cut; lines:\n{}",
        run.look_lines().join("\n")
    );
    run.let_go(pid).await;

    // The bucket refuses the copy of the fourth tool call's look; the tick that tries again is the timer's, at the same head.
    let key = keys::snapshot_bundle_key(RUN);
    let attempts = run.d.aws.put_attempts(&key);
    let from = run.bundles().len();
    run.d.aws.fail_puts("snapshot.bundle", 4);
    let again = "in the copy of the tick after the tool call cut the keep\n";
    run.write("b.txt", again);
    run.tool_call_ends().await;
    assert!(wait_for(LONG, || run.d.aws.put_attempts(&key) >= attempts + 4).await);
    assert!(
        run.wait_copy(from, "b.txt", again, LONG).await.is_some(),
        "log:\n{}",
        run.log()
    );
    assert!(
        wait_for(LONG, || run.kept_lines() >= 1).await,
        "TRIGGER PRESENT: the keep that a tool call cut was not made after a later look by the timer; lines:\n{}",
        run.look_lines().join("\n")
    );
    assert_eq!(keeps_started(&run.dir), 2);

    // The worker ends as a signal asks, so that it ends by itself.
    run.d.process.as_ref().unwrap().signal("TERM");
    assert_eq!(
        exit_of(&mut run, LONG).await,
        Some(Some(143)),
        "log:\n{}",
        run.log()
    );
}

/// A tool call's wake that is already pending when the keep would start is taken as before: the copy it asks for, which holds the
/// work written after the tool call ended, is stored by a look that the tool call started, and no git of a keep is started while
/// the wake is pending. The wake is made pending by holding the store of the look that the keep follows: the tool call ends while
/// the bucket holds that look's upload. The timer is ten minutes away, so a wake that was lost would leave the work without a copy.
/// That a keep was due then is shown afterwards: the next look by the timer of the same head, with no wake pending, is followed
/// by one.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_wake_that_is_pending_starts_the_next_look_as_before() {
    let mut run = Run::start("b860-kept-wake", NO_TICK_MS, Vec::new()).await;
    run.hold_the_keep();
    run.commit(1);
    let from = run.bundles().len();
    run.write("c.txt", "found by the look of a tool call\n");
    run.tool_call_ends().await;
    assert!(
        run.wait_copy(from, "c.txt", "found by the look of a tool call\n", LONG)
            .await
            .is_some(),
        "the look after the first tool call stores its copy; log:\n{}",
        run.log()
    );
    // The bucket refuses the copy of the second tool call's look, then holds the upload of the tick that tries it again.
    let key = keys::snapshot_bundle_key(RUN);
    let attempts = run.d.aws.put_attempts(&key);
    run.d.aws.fail_puts("snapshot.bundle", 4);
    run.write("c.txt", "second\n");
    run.tool_call_ends().await;
    assert!(
        wait_for(LONG, || run.d.aws.put_attempts(&key) >= attempts + 4).await,
        "TRIGGER PRESENT is not shown: the bucket did not refuse the copy of the second tool call's look; log:\n{}",
        run.log()
    );
    run.d.aws.hold_put_body("snapshot.bundle", 16);
    assert!(
        wait_for(LONG, || run.d.aws.put_body_held()).await,
        "TRIGGER PRESENT is not shown: the look of the timer's tick did not reach its upload; log:\n{}",
        run.log()
    );
    // A third tool call ends while that look stores its copy: its wake is pending when the look ends.
    run.tool_call_ends().await;
    let third = "third, written after the tool call ended\n";
    let from = run.bundles().len();
    run.write("c.txt", third);
    run.d.aws.release_put_body();
    let found = run.wait_copy(from + 1, "c.txt", third, LONG).await;
    assert!(
        found.is_some(),
        "TRIGGER PRESENT: the copy that the pending wake asked for was not stored within {LONG:?} (the timer is ten minutes): the \
         wake did not survive the keep; log:\n{}",
        run.log()
    );
    assert!(
        !run.dir.join("keep-running").exists(),
        "TRIGGER PRESENT: a keep's git was started while a tool call's wake was pending"
    );
    let held = run.line_of(from);
    assert_eq!(
        (field(&held, "by"), field(&held, "keep")),
        (Some("timer"), Some("0")),
        "the look whose upload was held was the timer's, and no keep ran after it: {held}"
    );
    let line = run.line_when_stored(found.unwrap()).await;
    assert_eq!(
        field(&line, "by"),
        Some("tool"),
        "the look that stored that copy was the tool call's: {line}"
    );

    // That a keep was due after the look whose upload was held: the next look by the timer of the same head is followed by one.
    // The bucket refuses the copy of the fourth tool call's look, and the tick that tries it again is the timer's.
    assert_eq!(keeps_started(&run.dir), 0, "no keep was started so far");
    let _ = std::fs::remove_file(run.dir.join("keep-hold"));
    let attempts = run.d.aws.put_attempts(&key);
    let from = run.bundles().len();
    run.d.aws.fail_puts("snapshot.bundle", 4);
    let again = "in the copy of the tick after the wake was taken\n";
    run.write("c.txt", again);
    run.tool_call_ends().await;
    assert!(wait_for(LONG, || run.d.aws.put_attempts(&key) >= attempts + 4).await);
    assert!(
        run.wait_copy(from, "c.txt", again, LONG).await.is_some(),
        "log:\n{}",
        run.log()
    );
    assert!(
        wait_for(LONG, || run.kept_lines() >= 1).await,
        "TRIGGER PRESENT is not shown: no keep follows a look by the timer of this head, so none was due when the wake was \
         pending; lines:\n{}",
        run.look_lines().join("\n")
    );
    assert_eq!(keeps_started(&run.dir), 1);

    // The worker ends as a signal asks, so that it ends by itself.
    run.d.process.as_ref().unwrap().signal("TERM");
    assert_eq!(
        exit_of(&mut run, LONG).await,
        Some(Some(143)),
        "log:\n{}",
        run.log()
    );
}

/// The exit code of the worker once it has ended, waited for at most `limit`.
async fn exit_of(run: &mut Run, limit: Duration) -> Option<Option<i32>> {
    let began = Instant::now();
    loop {
        if let Some(code) = run.d.process.as_mut().unwrap().ended() {
            return Some(code);
        }
        if began.elapsed() > limit {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A request to end is not held by the keep. While the keep's git runs, the worker is asked to end: the keep is ended and the copy
/// of the stop, which holds the file written right before the signal, is stored inside the time a stop has (two minutes here; the
/// stand-in holds the keep's git without end); the worker ends as the signal asks. And in a second session: right after the engine
/// committed, the worker is asked to end, and the copy of the stop stands on the new head while no keep is started for it.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_stop_is_not_held_by_the_keep() {
    let budget = ("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", "120000".to_string());
    let mut run = Run::start("b860-kept-stop", NO_TICK_MS, vec![budget.clone()]).await;
    run.hold_the_keep();
    let index = run.to_the_look_a_keep_follows(1, "a.txt").await;
    let pid = run.the_held_keep(index).await;
    let late = "written right before the signal, while the keep's git ran\n";
    let from = run.bundles().len();
    run.write("late.txt", late);
    let asked = Instant::now();
    run.d.process.as_ref().unwrap().signal("TERM");
    let found = run
        .wait_copy(from, "late.txt", late, Duration::from_secs(100))
        .await;
    assert!(
        found.is_some(),
        "TRIGGER PRESENT: no copy with the file written before the signal was stored within 100 s of the request to end (a stop \
         has 120 s here): the keep held the stop; log:\n{}",
        run.log()
    );
    println!(
        "the copy of the stop was stored {:?} after the signal",
        asked.elapsed()
    );
    assert!(
        gone(pid).await,
        "TRIGGER PRESENT: the keep's git was not ended by the request to end"
    );
    assert_eq!(
        exit_of(&mut run, LONG).await,
        Some(Some(143)),
        "log:\n{}",
        run.log()
    );
    let record = run.records().pop().unwrap();
    assert!(
        record.get("final").is_none(),
        "the copy of a stop is no copy of the end: {record}"
    );
    let line = run.line_of(found.unwrap());
    assert_eq!(
        field(&line, "by"),
        Some("stop"),
        "the look of a stop: {line}"
    );
    run.let_go(pid).await;

    // Right after a commit: no keep is started for a head the copy task has not found twice.
    let mut run = Run::start("b860-kept-stop-commit", NO_TICK_MS, vec![budget]).await;
    run.to_the_look_a_keep_follows(1, "a.txt").await;
    assert!(
        wait_for(LONG, || run.kept_lines() >= 1).await,
        "TRIGGER PRESENT is not shown: no pack was kept for the first head; log:\n{}",
        run.log()
    );
    assert_eq!(keeps_started(&run.dir), 1);
    let head = run.commit(2);
    let late = "written right after the engine's commit\n";
    let from = run.bundles().len();
    run.write("late.txt", late);
    run.d.process.as_ref().unwrap().signal("TERM");
    let found = run
        .wait_copy(from, "late.txt", late, Duration::from_secs(100))
        .await;
    assert!(
        found.is_some(),
        "the copy of the stop is stored; log:\n{}",
        run.log()
    );
    assert_eq!(
        exit_of(&mut run, LONG).await,
        Some(Some(143)),
        "log:\n{}",
        run.log()
    );
    let bundles = run.bundles();
    let wt = checkout_of_copy_bytes(
        &run.d.aws,
        &run.dir,
        &bundles[found.unwrap()],
        "stop-after-commit",
    );
    assert_eq!(
        git(&wt, &["rev-parse", "HEAD^"]),
        head,
        "the copy of the stop stands on the engine's new head"
    );
    assert_eq!(
        keeps_started(&run.dir),
        1,
        "TRIGGER PRESENT: a keep was started for a head that a request to end found right after its commit"
    );
}

/// The worst moment (ruling 14 of the lead): the worker is killed while the keep's `pack-objects` runs, which is after the store of
/// its look. The bucket holds this look's copy: its record names the commit the bundle's ref names, the bundle a reader gets at the
/// key is that bundle, and a checkout that has only the bundle the run started from beside it holds the work of that look and
/// every object it needs. The copy before it restores too. The hash a look makes of its bundle, under which the store sends it, is
/// held against the bytes of the file in `b860_kept_the_copy_is_the_releases_copy`.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_session_cut_in_the_keep_loses_nothing() {
    let run = Run::start("b860-kept-killed", NO_TICK_MS, Vec::new()).await;
    run.hold_the_keep();
    let index = run.to_the_look_a_keep_follows(1, "a.txt").await;
    let pid = run.the_held_keep(index).await;
    run.d.process.as_ref().unwrap().signal("KILL");
    run.let_go(pid).await;

    let records = run.records();
    let bundles = run.bundles();
    assert_eq!(
        records.len(),
        bundles.len(),
        "every stored bundle has its record"
    );
    let newest = bundles.len() - 1;
    assert_eq!(
        newest, index,
        "the copy of the look the keep followed is the newest"
    );
    let wt = checkout_of_copy_bytes(&run.d.aws, &run.dir, &bundles[newest], "killed-newest");
    assert_eq!(
        git(&wt, &["rev-parse", "HEAD"]),
        records[newest]["headSha"].as_str().unwrap(),
        "the record of the newest copy names the commit its bundle holds"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "in the copy the bucket refused, and in the copy of the tick that tried again\n",
        "the work of the look the keep followed"
    );
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");
    git(&wt, &["fsck", "--no-progress", "--connectivity-only"]);
    assert_eq!(
        run.d.aws.get(&keys::snapshot_bundle_key(RUN)).as_deref(),
        Some(bundles[newest].as_slice()),
        "the bundle a reader gets at the key is the one the newest record names"
    );
    assert_eq!(
        run.file_of(newest - 1, "a.txt").as_deref(),
        Some("found by the look of a tool call\n"),
        "the copy before it restores too"
    );
}

/// A whole turn inside the test's own process: the engine commits, the looks of the timer find its head twice, and a pack is
/// kept; the worker's log says so in the line of that look, and the folder of the private repository holds the one pack. Then the
/// engine ends its turn: the copy of the end is stored as the last record and has its save line, a checkout that has only
/// the bundle the run started from beside it holds the work, and the copy of the end has built no pack and removed none. The test waits for what it asserts
/// and holds no time.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_turn_keeps_a_pack_and_ends_with_the_copy_of_the_end() {
    let dir = scratch("b860-kept-turn");
    let steps = dir.join("engine-steps");
    std::fs::write(&steps, "0\n").unwrap();
    let d = Direct::start(Opts {
        scenario: "four_edits",
        engine_env: vec![("FAKE_ACP_STEP_FILE", steps.display().to_string())],
        edit_cfg: Some(Box::new(|cfg| {
            cfg.direct.snapshot_interval = Some(Duration::from_millis(500));
            cfg.direct.snapshot_floor = Some(Duration::from_millis(50));
        })),
        ..Opts::default()
    })
    .await;
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(LONG, || !d.prompts_received().is_empty()).await,
        "the engine got its prompt"
    );
    let look_lines = || -> Vec<String> {
        d.log
            .lines()
            .into_iter()
            .filter(|l| l.contains("direct turn copy look n="))
            .collect()
    };
    let stored = || d.aws.versions_of(&keys::snapshot_json_key(RUN)).len();

    // The engine commits, and three looks of the timer each find a changed tree.
    let wt = d.worktree();
    std::fs::create_dir_all(wt.join("run")).unwrap();
    std::fs::write(wt.join("run/file.txt"), "the file of the engine's commit\n").unwrap();
    git(&wt, &["add", "run"]);
    git(&wt, &["commit", "-q", "-m", "the engine's commit"]);
    for content in ["first\n", "second\n", "third\n"] {
        let before = stored();
        std::fs::write(wt.join("t.txt"), content).unwrap();
        assert!(
            wait_for(LONG, || stored() > before).await,
            "a look of the timer stores the copy of {content:?}; lines:\n{}",
            look_lines().join("\n")
        );
    }
    assert!(
        wait_for(LONG, || look_lines().iter().any(|l| {
            field(l, "keep").is_some_and(|v| v.parse::<u64>().is_ok_and(|ms| ms > 0))
                && field(l, "kept").is_some_and(|v| v.parse::<u64>().is_ok_and(|bytes| bytes > 0))
        }))
        .await,
        "TRIGGER PRESENT: no pack was kept in a turn whose engine committed and whose looks found its head twice; lines:\n{}",
        look_lines().join("\n")
    );
    let kept = d.agent_data().join("snapshot.git/kept/pack");
    let packs = |dir: &std::path::Path| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".pack"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    };
    let before_the_end = packs(&kept);
    assert_eq!(
        before_the_end.len(),
        1,
        "one kept pack in the worker's private repository"
    );

    // The engine ends its turn: the copy of the end is the last record.
    std::fs::write(&steps, "1000000\n").unwrap();
    let resp = d.wait_result().await;
    assert_eq!(
        resp.outcome,
        harness_protocol::runtime::TurnOutcome::Completed,
        "{resp:?}"
    );
    let records: Vec<serde_json::Value> = d
        .aws
        .versions_of(&keys::snapshot_json_key(RUN))
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
        .collect();
    let end = records.last().unwrap();
    assert_eq!(
        end["final"],
        serde_json::json!(true),
        "the copy of the end: {end}"
    );
    // The copy of the end writes the save line the looks write, with `by=final` and the number its record has. Before, it
    // wrote none, and the changelog of release 5.28 says that the worker logs one line for each save that stored a copy or failed.
    let end_lines: Vec<String> = look_lines()
        .into_iter()
        .filter(|l| field(l, "by") == Some("final"))
        .collect();
    assert_eq!(
        end_lines.len(),
        1,
        "the copy of the end has its save line; lines:\n{}",
        look_lines().join("\n")
    );
    assert!(
        end_lines[0].contains(" stored by=final "),
        "{}",
        end_lines[0]
    );
    assert_eq!(
        field(&end_lines[0], "n"),
        Some(end["count"].to_string().as_str()),
        "the line names the number of the copy of the end: {}",
        end_lines[0]
    );
    let bundle = d.aws.get(&keys::snapshot_bundle_key(RUN)).unwrap();
    let checkout = checkout_of_copy_bytes(&d.aws, &dir, &bundle, "the-end");
    assert_eq!(
        git(&checkout, &["rev-parse", "HEAD"]),
        end["headSha"].as_str().unwrap(),
        "the record of the copy of the end names the commit its bundle holds"
    );
    assert_eq!(
        std::fs::read_to_string(checkout.join("t.txt")).unwrap(),
        "third\n"
    );
    assert_eq!(
        std::fs::read_to_string(checkout.join("run/file.txt")).unwrap(),
        "the file of the engine's commit\n"
    );
    assert_eq!(
        packs(&kept),
        before_the_end,
        "the copy of the end builds no pack and removes none"
    );
    assert!(
        look_lines().iter().all(|l| !l.contains("cpu_ms")),
        "no line of a look names processor time"
    );
}

/// A look of the timer whose keep had not started when the engine ended its turn has its save line once, with `keep=0`,
/// and the line of the copy of the end follows it, once.
///
/// The line of a look that a keep follows waits for what comes of the keep. The wait of the copy task takes the line up when it
/// turns to the keep. When it takes a tool call's wake that is already pending first, the line stays where it is while the task
/// waits out the spacing of the looks; an engine that ends its turn then ends the copy task there, and the copy of the end writes
/// the look's line before its own work. Which of the two the wait takes first is the runtime's choice, one time in two with a
/// pending wake, so the test runs turns until one shows the trigger, at most [`PENDING_TURNS`]. The trigger is shown when the
/// record of the look's copy is in the bucket and its line is not in the log [`PENDING_GRACE`] later, and when the copy of the
/// end is stored before the spacing after that look has run out, so that no later look wrote the line. The spacing is long
/// without a clock: the stand-in holds the `git add` of that look for [`ADD_HELD`], and the spacing is nine times the git work
/// of the look, at most five periods of the timer.
///
/// A turn that does not show the trigger is left and the next one starts: no turn fails because the machine is crowded.
#[tokio::test(flavor = "multi_thread")]
async fn b930_a_line_that_waits_for_its_keep_at_the_engines_end_comes_once_before_the_end_copys() {
    let spacing = Duration::from_millis(5 * DUE_EVERY_MS);
    let mut left = Vec::new();
    for turn in 1..=PENDING_TURNS {
        let run = Run::start(&format!("b930-pending-{turn}"), DUE_EVERY_MS, Vec::new()).await;
        assert!(
            wait_for(LONG, || run.stored() >= 1).await,
            "the copy of the session's start is stored; log:\n{}",
            run.log()
        );
        // A look has just ended when its copy is seen: the commit and the write after it fall before the same next look.
        assert!(
            run.write_and_wait("a.txt", "written to stand right behind a look\n", LONG)
                .await
                .is_some()
        );
        run.commit(1);
        assert!(
            run.write_and_wait("a.txt", "first\n", LONG).await.is_some(),
            "the look that finds the engine's new head stores its copy; log:\n{}",
            run.log()
        );
        // The second look of the same head, which a keep follows: the stand-in holds its `git add`.
        run.hold_the_add();
        let from = run.bundles().len();
        run.write("a.txt", "second\n");
        assert!(
            wait_for(LONG, || run.dir.join("add-running").exists()).await,
            "TRIGGER PRESENT is not shown: no look read the tree after the write; log:\n{}",
            run.log()
        );
        // A tool call ends while that look works: its wake is pending when the look ends.
        run.tool_call_ends().await;
        tokio::time::sleep(ADD_HELD).await;
        let let_go = Instant::now();
        run.let_the_add_go();
        let index = run
            .wait_copy(from, "a.txt", "second\n", LONG)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "the look whose `git add` was held stores its copy; log:\n{}",
                    run.log()
                )
            });
        // The bucket has the bundle of a copy before it has the record, and the line of a look is found by the number its
        // record has: the record of this copy is waited for before its line is read.
        assert!(
            wait_for(LONG, || run.stored() > index).await,
            "the look whose `git add` was held stores the record of its copy; log:\n{}",
            run.log()
        );
        // A line that the wait of the copy task wrote is in the log at once; a line that still waits for its keep is not.
        if wait_for(PENDING_GRACE, || !run.line_of(index).is_empty()).await {
            left.push(format!(
                "turn {turn}: the wait turned to the keep first: {}",
                run.line_of(index)
            ));
            continue;
        }

        // The engine ends its turn while the line waits: the copy of the end is the last record.
        std::fs::write(&run.steps, "1000000\n").unwrap();
        let ended = wait_for(LONG, || {
            run.records().last().is_some_and(|r| r["final"] == true)
        })
        .await;
        let took = let_go.elapsed();
        assert!(
            ended,
            "the engine's turn ends with the copy of the end; log:\n{}",
            run.log()
        );
        if took >= spacing {
            left.push(format!(
                "turn {turn}: the copy of the end was stored {took:?} after the look's `git add` was let go, and the spacing after \
                 that look is {spacing:?}: a later look may have written the line"
            ));
            continue;
        }
        assert!(
            wait_for(LONG, || run
                .look_lines()
                .iter()
                .any(|l| field(l, "by") == Some("final")))
            .await,
            "the copy of the end writes its save line; lines:\n{}",
            run.look_lines().join("\n")
        );
        let records = run.records();
        let n = records[index]["count"].to_string();
        let end_n = records.last().unwrap()["count"].to_string();
        let lines = run.look_lines();
        let of_the_look: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| field(l, "n") == Some(n.as_str()))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            of_the_look.len(),
            1,
            "TRIGGER PRESENT: the look of the timer that stored copy {n}, whose keep had not started when the engine ended its \
             turn, has {} save lines; lines:\n{}",
            of_the_look.len(),
            lines.join("\n")
        );
        let line = &lines[of_the_look[0]];
        assert!(line.contains(" stored by=timer "), "{line}");
        assert_eq!(
            field(line, "keep"),
            Some("0"),
            "no keep had started after that look: {line}"
        );
        let after = &lines[of_the_look[0] + 1..];
        assert_eq!(
            after.len(),
            1,
            "one save line follows the line of that look, the line of the copy of the end; lines:\n{}",
            lines.join("\n")
        );
        assert!(after[0].contains(" stored by=final "), "{}", after[0]);
        assert_eq!(
            field(&after[0], "n"),
            Some(end_n.as_str()),
            "the line names the number of the copy of the end: {}",
            after[0]
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| field(l, "by") == Some("final"))
                .count(),
            1,
            "the copy of the end has one save line; lines:\n{}",
            lines.join("\n")
        );
        return;
    }
    panic!(
        "TRIGGER PRESENT is not shown: in {PENDING_TURNS} turns no look of the timer still waited for its keep when the engine \
         ended its turn:\n{}",
        left.join("\n")
    );
}

/// Finding 5 of the review of this branch: a turn of these tests ends its engine when it ends. The drop of the worker ends
/// `agentd serve` with KILL, and a worker that is ended that way cannot end the engine it started: the stand-in engine of these
/// tests waits for the file that lets it speak and stayed behind for as long as the machine ran. The turn finds the engine's
/// processes below its worker once the engine has its prompt, and its drop ends them, also after a test that killed the worker
/// itself and after one that failed.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_turn_of_these_tests_ends_its_engine() {
    let run = Run::start("b860-kept-engine", NO_TICK_MS, Vec::new()).await;
    let engines = run.engines.clone();
    assert!(
        !engines.is_empty() && engines.iter().all(engine_runs),
        "TRIGGER PRESENT is not shown: no running process of the stand-in engine was found below the worker: {engines:?}"
    );
    drop(run);
    assert!(
        wait_for(Duration::from_secs(30), || !engines.iter().any(engine_runs)).await,
        "TRIGGER PRESENT: the stand-in engine of a turn that has ended still runs: {engines:?}"
    );
}

/// A pack in the folder of the kept pack of the private repository that the worker of `d` opens when its turn starts, with a
/// worker's marker beside it, as an earlier worker of the run left it. git made the pack, of twenty commits of a small repository
/// in the test's folder `dir`; with `wrong`, 48 bytes in its middle are not the bytes git wrote. The answer is the size of the
/// pack file.
fn plant_a_kept_pack(d: &Direct, dir: &Path, wrong: bool) -> u64 {
    let source = dir.join("planted");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q"]);
    for round in 0..20 {
        std::fs::write(
            source.join(format!("file{round}.txt")),
            format!("the file of round {round}\n"),
        )
        .unwrap();
        git(&source, &["add", "-A"]);
        git(&source, &["commit", "-q", "-m", &format!("round {round}")]);
    }
    let head = git(&source, &["rev-parse", "HEAD"]);
    git(&source, &["repack", "-a", "-d", "-q"]);
    let made = source.join(".git/objects/pack");
    let kept = d.agent_data().join("snapshot.git/kept/pack");
    std::fs::create_dir_all(&kept).unwrap();
    let mut pack = None;
    for file in names(&made) {
        std::fs::copy(made.join(&file), kept.join(&file)).unwrap();
        if file.ends_with(".pack") {
            pack = Some(kept.join(&file));
        }
    }
    let pack = pack.expect("git made a pack of the small repository");
    if wrong {
        let mut bytes = std::fs::read(&pack).unwrap();
        let middle = bytes.len() / 2;
        for byte in &mut bytes[middle - 24..middle + 24] {
            *byte = !*byte;
        }
        // The file of a pack cannot be written to: the wrong bytes take its place under its name.
        let swap = pack.with_extension("wrong");
        std::fs::write(&swap, &bytes).unwrap();
        std::fs::rename(&swap, &pack).unwrap();
    }
    std::fs::write(
        pack.with_extension("keep"),
        format!("agentd kept {head}\nengine packs 0:none\n"),
    )
    .unwrap();
    std::fs::metadata(&pack).unwrap().len()
}

/// The worker's log lines that say that a kept pack was not adopted.
fn not_adopted_lines(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.contains("direct turn copy keep: "))
        .map(str::to_string)
        .collect()
}

/// Ruling 23 (d) of the lead: a real worker starts its turn on a private repository whose kept pack has wrong bytes. Its log has
/// exactly one line for the pack that was not adopted, the pack is gone, and the first copy of the turn restores in a checkout
/// that has only the bundle the run started from beside it.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_worker_that_starts_on_a_pack_with_wrong_bytes_says_so_once() {
    let run = Run::start_on("b860-kept-wrong-start", NO_TICK_MS, Vec::new(), |d, dir| {
        plant_a_kept_pack(d, dir, true);
    })
    .await;
    let kept = run.d.agent_data().join("snapshot.git/kept/pack");
    wait_for(LONG, || !not_adopted_lines(&run.log()).is_empty()).await;
    let said = not_adopted_lines(&run.log());
    assert!(
        said.len() == 1 && said[0].contains("git's check"),
        "TRIGGER PRESENT: the worker's log has {} lines for the kept pack that was not adopted, where it has one; log:\n{}",
        said.len(),
        run.log()
    );
    assert_eq!(
        names(&kept),
        Vec::<String>::new(),
        "the pack with wrong bytes is gone"
    );

    let content = "the first work of the turn\n";
    run.write("a.txt", content);
    run.tool_call_ends().await;
    assert_eq!(
        run.wait_copy(0, "a.txt", content, LONG).await,
        Some(0),
        "the first copy of the turn holds the work; log:\n{}",
        run.log()
    );
    // The bucket has the bundle of a copy before it has the record: the record is read once it is there.
    assert!(
        wait_for(LONG, || run.stored() >= 1).await,
        "the record of the first copy is stored; log:\n{}",
        run.log()
    );
    let bundles = run.bundles();
    let wt = checkout_of_copy_bytes(&run.d.aws, &run.dir, &bundles[0], "wrong-start-first");
    assert_eq!(
        git(&wt, &["rev-parse", "HEAD"]),
        run.records()[0]["headSha"].as_str().unwrap(),
        "the record of the first copy names the commit its bundle holds"
    );
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");
    git(&wt, &["fsck", "--no-progress", "--connectivity-only"]);
    assert_eq!(
        not_adopted_lines(&run.log()).len(),
        1,
        "it is said once in the turn"
    );
}

/// Ruling 23 (e) of the lead: what git's check of a kept pack costs the start of a turn is in the worker's log, in the line of
/// the first reading of the tree: the size of the pack and the time the check took. Here the pack is whole: it is adopted, and
/// no line says that it was not.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_the_line_of_the_first_reading_names_the_check_of_a_kept_pack() {
    let planted = AtomicU64::new(0);
    let run = Run::start_on("b860-kept-start-check", NO_TICK_MS, Vec::new(), |d, dir| {
        planted.store(plant_a_kept_pack(d, dir, false), Ordering::SeqCst);
    })
    .await;
    wait_for(LONG, || run.log().contains("the first reading of the tree")).await;
    let log = run.log();
    let first: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("the first reading of the tree"))
        .collect();
    assert_eq!(first.len(), 1, "one line of the first reading; log:\n{log}");
    let words: Vec<&str> = first[0]
        .split_once("git's check of the kept pack of ")
        .map(|(_, rest)| rest.split_whitespace().collect())
        .unwrap_or_default();
    let (bytes, ms) = match words.as_slice() {
        [bytes, "bytes", "took", ms, "ms", ..] => {
            (bytes.parse::<u64>().ok(), ms.parse::<u64>().ok())
        }
        _ => (None, None),
    };
    assert_eq!(
        (bytes, ms.is_some()),
        (Some(planted.load(Ordering::SeqCst)), true),
        "TRIGGER PRESENT: the line of the first reading does not name the size of the kept pack and the time git's check of it \
         took: {}",
        first[0]
    );
    assert_eq!(
        not_adopted_lines(&log),
        Vec::<String>::new(),
        "a whole pack is adopted"
    );
    let kept = run.d.agent_data().join("snapshot.git/kept/pack");
    assert!(
        names(&kept).iter().any(|n| n.ends_with(".pack")),
        "the adopted pack stands: {:?}",
        names(&kept)
    );
}

/// Ruling 14 of the lead, the cut between the store's two writes (finding 6 of the review of this branch). The store of a copy
/// writes the bundle and then the record. Here a look makes its copy beside a kept pack, the bucket takes the bundle and holds
/// the record, and the worker is killed there. The bucket then holds the record of the copy before, the bundle of that copy as
/// an earlier version at the key, and a newer bundle without a record. A reader restores through the published record and its
/// hash: the newest record names a commit, and the bundle that was stored with that record, checked out with only the bundle the
/// run started from beside it, stands at that commit, holds the work of that copy and every object it needs. The bundle without
/// a record is the one a reader gets at the key: it restores too, and it holds the work that was written last. No stored byte is
/// lost to the cut. (The hash a look makes of its bundle, under which the store sends it, is held against the bytes of the file
/// in `b860_kept_the_copy_is_the_releases_copy`.)
///
/// The other cut of ruling 14, between the renames that install a kept pack, is not made with a killed worker: the renames have
/// no wait between them, and what a worker killed there leaves is the state that the start of a worker is tested with
/// (`b860_kept_a_keep_cut_between_its_renames_is_cleared_at_the_start` in `b860_kept_commits.rs`).
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_session_cut_between_the_stores_two_writes_restores_through_the_record() {
    let mut run = Run::start("b860-kept-store-cut", NO_TICK_MS, Vec::new()).await;
    let index = run.to_the_look_a_keep_follows(1, "a.txt").await;
    assert!(
        wait_for(LONG, || run.kept_lines() >= 1).await,
        "TRIGGER PRESENT is not shown: no pack was kept before the look whose store is cut; log:\n{}",
        run.log()
    );

    // The look of the next tool call makes its copy beside the kept pack; the bucket takes its bundle and holds its record.
    run.d.aws.hold_put_body("snapshot.json", 0);
    let last = "written before the look whose store was cut\n";
    run.write("a.txt", last);
    run.tool_call_ends().await;
    assert!(
        wait_for(LONG, || run.d.aws.put_body_held()).await,
        "TRIGGER PRESENT is not shown: the store did not reach the write of its record; log:\n{}",
        run.log()
    );
    assert_eq!(
        (run.bundles().len(), run.stored()),
        (index + 2, index + 1),
        "TRIGGER PRESENT is not shown: the bundle of the look is in the bucket and its record is not"
    );
    run.d.process.as_ref().unwrap().signal("KILL");
    assert_eq!(
        exit_of(&mut run, LONG).await,
        Some(None),
        "the worker was ended by the signal"
    );
    // The held write of the record is never let go: it did not reach the bucket.
    let (records, bundles) = (run.records(), run.bundles());
    assert_eq!(
        (bundles.len(), records.len()),
        (index + 2, index + 1),
        "the cut is between the two writes: one bundle more than records"
    );

    // Through the published record and its hash: the newest record, the commit it names, and the bundle stored with it.
    let published = &records[index];
    let wt = checkout_of_copy_bytes(&run.d.aws, &run.dir, &bundles[index], "cut-published");
    assert_eq!(
        git(&wt, &["rev-parse", "HEAD"]),
        published["headSha"].as_str().unwrap(),
        "the bundle that was stored with the published record stands at the commit the record names"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "in the copy the bucket refused, and in the copy of the tick that tried again\n",
        "the work of the copy the published record names"
    );
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");
    git(&wt, &["fsck", "--no-progress", "--connectivity-only"]);

    // The bundle without a record: what a reader gets at the key, made beside the kept pack.
    let key = keys::snapshot_bundle_key(RUN);
    assert_eq!(
        run.d.aws.get(&key).as_deref(),
        Some(bundles[index + 1].as_slice()),
        "the bundle at the key is the one whose record was not written"
    );
    let wt = checkout_of_copy_bytes(&run.d.aws, &run.dir, &bundles[index + 1], "cut-newest");
    assert_ne!(
        git(&wt, &["rev-parse", "HEAD"]),
        published["headSha"].as_str().unwrap(),
        "the bundle without a record is a newer copy than the published record names"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        last,
        "the work that was written last"
    );
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");
    git(&wt, &["fsck", "--no-progress", "--connectivity-only"]);
    // The stand-in bucket ends the request it held: its worker is gone, and everything above was read before this.
    run.d.aws.release_put_body();
}
