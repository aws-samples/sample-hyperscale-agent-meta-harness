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
//!   the line of the copy of the end follows it;
//! - the keep after a burst of commits: a keep follows the first look of the
//!   timer after the burst at once when that look left the timer, the engine's head stood still while it ran and the engine's
//!   packs did not change; a keep that outlasts the tick of the timer is not cut; a tool call that ends while a keep runs does
//!   not end it; a keep at once that did not end as kept leaves the rule of the second look as it is; a tool call that ended
//!   while the slow look ran does not cost that look its keep at once, and the tool call's look starts when it would start
//!   without a keep; and only a look that read no kept pack is followed by a keep at once, so steady commits get one keep for
//!   their first burst and a second burst keeps the rule of the second look. Ruling 43 of the lead: a keep at once that did not
//!   end as kept is tried once and not again until a keep is kept; a request to end ends the wait that a keep runs through;
//!   and the keep at once also follows a tool call's look that left the timer, on the tool call's own line, while the rule
//!   of the second look follows no look of a tool call, as before.
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

use b860_kept_support::{alive, git_calls, is_the_keep, keeps_started, names, stand_in_for_git};
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
/// How long a turn of these tests has before the worker ends it for its time: an hour, which no test of this file comes to.
/// The rig's own turn has 120 s, and a test of this file waits up to [`LONG`] for each of its copies. On a crowded machine
/// the turn of `b860_kept_a_look_that_is_due_ends_the_keep_and_its_copy_is_not_later` came to its 120 s before the test's
/// last part: the worker ended the engine, and the copy of the end met what the test had prepared for a look. A turn of
/// these tests ends when its test asks for it.
///
/// The hour bounds no wait of a test: each wait has its own limit ([`LONG`], [`RESULT_WITHIN`]). It is also not the first
/// limit of the worker that a turn of these tests would come to. The engine of these tests says nothing while it waits for
/// the file that lets it speak, and the worker gives an engine that says nothing 900 s when the turn names no other time
/// (`DEFAULT_PROVIDER_SILENCE_S` in the worker's `acp.rs`, which `direct/mod.rs` takes when `provider_silence_s` of the
/// request is not set; the rig's `Opts` leaves it unset). That is read from the code: no test of this file waits that
/// long, and no run was made to that limit.
const TURN_S: u32 = 3_600;
/// How long a test of this file waits for the result of a turn at the most: 210 s, what the rig's own wait was while
/// the turns of this file had the rig's 120 s (the rig waits the turn's time and 90 s more). With [`TURN_S`] the rig's own
/// wait is 3,690 s, and a worker whose turn end hangs would hold a test that long where it failed after three and a half
/// minutes. So no test of this file asks the rig for a turn's result itself: it asks [`the_turns_result`], whatever the
/// turn's time is.
const RESULT_WITHIN: Duration = Duration::from_secs(210);
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

/// The result of the turn of `d`, waited for [`RESULT_WITHIN`] at the most, counted from this call as the rig counts
/// its own wait. The one line of code in this file that asks the rig for a turn's result
/// (`b948_a_turns_result_is_waited_for_within_the_tests_own_bound`).
async fn the_turns_result(d: &Direct) -> harness_protocol::runtime::InvocationResponse {
    tokio::time::timeout(RESULT_WITHIN, d.wait_result())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "no result of the turn within {RESULT_WITHIN:?} at {}",
                d.result_path().display()
            )
        })
}

/// A test of this file waits for a turn's result within its own bound, whatever the turn's time. The rig's wait
/// follows the turn's time, so it is asked in one line of code of this file, the one in [`the_turns_result`], and the bound
/// is the 210 s that the wait was before the turns of this file got their hour.
#[test]
fn b948_a_turns_result_is_waited_for_within_the_tests_own_bound() {
    assert_eq!(RESULT_WITHIN, Duration::from_secs(210));
    let rigs_wait = concat!("wait_", "result");
    let asked: Vec<String> = include_str!("b860_kept_commits_run.rs")
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//") && line.contains(rigs_wait))
        .map(|(n, line)| format!("{}: {}", n + 1, line.trim()))
        .collect();
    assert!(
        asked.len() == 1
            && asked
                .first()
                .is_some_and(|line| line.contains("timeout(RESULT_WITHIN, ")),
        "the rig's wait for a turn's result follows the turn's time, which is an hour here: only `the_turns_result` asks \
         the rig, inside its own bound; the lines of code that ask it:\n{}",
        asked.join("\n")
    );
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
            timeout_s: TURN_S,
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
    /// The bucket has the bundle of a copy before it has the record: while the record of that copy is not there, the answer is
    /// the empty line, as for a look whose line is not written yet.
    fn line_of(&self, index: usize) -> String {
        let Some(n) = self
            .records()
            .get(index)
            .map(|record| record["count"].to_string())
        else {
            return String::new();
        };
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
    // The one test with a short timer: 3 s. Its looks are the timer's, but for the two that first find a new head.
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
    // The look that first finds a new head stores no copy here (the bucket refuses it), so that no keep at once is
    // counted on a crowded machine. The tick that tries the copy again is the timer's: the second look of the same head.
    let from = run.bundles().len();
    run.first_look_of_a_head_stores_nothing("a.txt", "second\n")
        .await;
    let second = run
        .wait_copy(from, "a.txt", "second\n", LONG)
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
    let from = run.bundles().len();
    run.first_look_of_a_head_stores_nothing("a.txt", "sixth\n")
        .await;
    assert!(
        run.wait_copy(from, "a.txt", "sixth\n", LONG)
            .await
            .is_some(),
        "the tick that tries the refused copy again stores it; log:\n{}",
        run.log()
    );
    for content in ["seventh\n", "eighth\n"] {
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
    // The turn still runs: every line so far is the line of a look, and none is the line of a copy of the turn's end.
    assert!(
        !run.engines.is_empty() && run.engines.iter().all(engine_runs),
        "TRIGGER PRESENT is not shown: the engine of the turn has ended before the test asked the worker to end, so a copy \
         after that is the copy of the turn's end; log:\n{}",
        run.log()
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
    // The worker has ended, so nothing is under way: the copy of the stop has its record now or never gets it. The record and
    // the line of that copy are read by its place among the copies, so that place is asked for first.
    let found = found.unwrap();
    let records = run.records();
    assert!(
        records.len() > found,
        "the worker ended and the copy of the stop has no record: its bundle is copy {found} of the bucket, which has {} \
         records; log:\n{}",
        records.len(),
        run.log()
    );
    let record = &records[records.len() - 1];
    assert!(
        record.get("final").is_none(),
        "the copy of a stop is no copy of the end: {record}"
    );
    let line = run.line_of(found);
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
/// and holds no time. For the result of its turn it waits 210 s at the most ([`RESULT_WITHIN`]), as it did before its turn had
/// an hour: a worker whose turn end takes four minutes longer fails it there.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_turn_keeps_a_pack_and_ends_with_the_copy_of_the_end() {
    let dir = scratch("b860-kept-turn");
    let steps = dir.join("engine-steps");
    std::fs::write(&steps, "0\n").unwrap();
    let d = Direct::start(Opts {
        scenario: "four_edits",
        engine_env: vec![("FAKE_ACP_STEP_FILE", steps.display().to_string())],
        timeout_s: TURN_S,
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
    let resp = the_turns_result(&d).await;
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
        let run = Run::start(
            &format!("b930-pending-{turn}"),
            DUE_EVERY_MS,
            vec![retry_at_the_tick()],
        )
        .await;
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
        // The look that first finds the new head stores no copy (the bucket refuses it), so that the keep is the one
        // after the second look, whatever the machine does.
        let from = run.bundles().len();
        run.first_look_of_a_head_stores_nothing("a.txt", "second\n")
            .await;
        // The second look of the same head, which a keep follows, is the look of the tick that tries the refused copy again:
        // the stand-in holds its `git add`.
        run.hold_the_add();
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

// ----: the keep after a burst of commits ---------------------------------------------------------------------------------------
//
// Ruling 32 of the lead, two rules built as one change. A: what ends a keep in flight is the start of the next look, not the tick
// of the timer and not a tool call's wake. B: a keep also follows a look of the timer that stored its copy when the spacing holds
// the next look past its tick, the engine's head read once more after the bundle step is the head the look read at its start, and
// the engine's packs did not change while the look ran; a keep at once that did not end as kept leaves the rule of release 5.28
// (the keep after a second look of the same head, once) as it is for the looks that follow.
//
// The looks of these tests are the timer's, at 3 s ([`BURST_EVERY_MS`]): a look leaves that timer when its git work takes more
// than a tenth of it, 300 ms. The stand-in holds the bundle step of the look that is to leave it for [`SLOW`] or [`HELD`]; each
// test reads from the look's own line that it did (TRIGGER PRESENT), and no test asserts that a look was fast. A test that needs a
// look that does not leave the timer has a timer of ten minutes, where no look of these tests can.

/// The timer of the tests of the keep after a burst: 3 s. The spacing holds the look after a look past its tick when the
/// git work of that look took more than a tenth of the timer: 300 ms.
const BURST_EVERY_MS: u64 = 3_000;
/// How long the stand-in holds the bundle step of a look that is to leave the timer: 600 ms, twice the tenth of the timer.
/// The spacing after such a look is nine times its git work, 5.4 s at the least.
const SLOW: Duration = Duration::from_millis(600);
/// How long the stand-in holds the bundle step of a look whose spacing is to be the longest a timer of a test has, five of
/// its periods (15 s): 2 s, nine times which is 18 s.
const HELD: Duration = Duration::from_secs(2);

impl Run {
    /// The stand-in holds the bundle step of the next look from now on; `bundle-running` is there once it does.
    fn hold_the_bundle(&self) {
        let _ = std::fs::remove_file(self.dir.join("bundle-running"));
        std::fs::write(self.dir.join("bundle-hold"), "").unwrap();
    }

    /// The held bundle step goes on.
    fn let_the_bundle_go(&self) {
        let _ = std::fs::remove_file(self.dir.join("bundle-hold"));
    }

    /// The private repository of the copies of this turn's worker.
    fn private(&self) -> PathBuf {
        self.d.agent_data().join("snapshot.git")
    }

    /// The bundle steps of the looks so far, oldest first: whether each found a kept pack in its folder when it started.
    fn bundle_steps(&self) -> Vec<bool> {
        b860_kept_support::kept_seen(&self.dir, &self.private())
            .iter()
            .filter(|seen| b860_kept_support::command_of(&seen.call) == "bundle")
            .map(|seen| seen.pack.iter().any(|name| name.ends_with(".pack")))
            .collect()
    }

    /// The engine commits `n` times from round `from` and nothing packs its store: a burst. The answer is its head.
    fn burst(&self, from: u32, n: u32) -> String {
        let mut head = String::new();
        for round in from..from + n {
            head = self.commit(round);
        }
        head
    }

    /// A look of the timer whose bundle step the stand-in holds for `held`. `content` is written into `file`, the look that
    /// finds it is held in its bundle step, `during` is called while it is held, and after `held` the step goes on. The answer is
    /// the index of the copy that look stored.
    async fn held_look(
        &self,
        file: &str,
        content: &str,
        held: Duration,
        during: impl FnOnce(&Self),
    ) -> usize {
        self.hold_the_bundle();
        let from = self.bundles().len();
        self.write(file, content);
        assert!(
            wait_for(LONG, || self.dir.join("bundle-running").exists()).await,
            "TRIGGER PRESENT is not shown: no look reached its bundle step after the write; log:\n{}",
            self.log()
        );
        during(self);
        tokio::time::sleep(held).await;
        self.let_the_bundle_go();
        self.wait_copy(from, file, content, LONG)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "the look whose bundle step was held stores its copy; log:\n{}",
                    self.log()
                )
            })
    }

    /// The look that first finds a new head of the engine stores no copy: `content` is written into `file`, and the
    /// bucket refuses the copy of the look that finds it, the timer's next look, at every try. A look of the timer or of a
    /// tool call (ruling 43 (d) of the lead) that finds a new head is followed by a keep at once when its git work took more
    /// than a tenth of the timer, 300 ms at the 3 s timer of these tests, which a look of a crowded machine can take; the keeps
    /// of a test would then be counted by the machine. No keep follows a look that stored no copy. The tick that tries the copy
    /// again is the timer's, and its look finds the same head and stores `content`: it is the second look of the head, so the
    /// one keep of the head is the keep after that look, whatever the machine does. The answer comes once the bucket has
    /// refused the copy; the look of the tick has not started then: it comes one retry time after the refused look started at
    /// the earliest. No tool call of the engine is used: a wake that a tick of the timer overtook would start the second look
    /// of the head, and the rule of the second look follows no look of a tool call.
    async fn first_look_of_a_head_stores_nothing(&self, file: &str, content: &str) {
        let key = keys::snapshot_bundle_key(RUN);
        let attempts = self.d.aws.put_attempts(&key);
        self.d.aws.fail_puts("snapshot.bundle", 4);
        self.write(file, content);
        assert!(
            wait_for(LONG, || self.d.aws.put_attempts(&key) >= attempts + 4).await,
            "TRIGGER PRESENT is not shown: the bucket did not refuse the copy of the look that found the write; log:\n{}",
            self.log()
        );
    }

    /// The first copies of a turn of these tests: the copy of the session's start and one more, which is seen right after
    /// its look ended, so that what the test does next falls before the same next look.
    async fn behind_a_look(&self, file: &str) {
        assert!(
            wait_for(LONG, || self.stored() >= 1).await,
            "the copy of the session's start is stored; log:\n{}",
            self.log()
        );
        assert!(
            self.write_and_wait(file, "written to stand right behind a look\n", LONG)
                .await
                .is_some(),
            "log:\n{}",
            self.log()
        );
    }
}

/// The milliseconds of the field `name` of a look's line.
fn ms_of(line: &str, name: &str) -> u64 {
    field(line, name)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("the field {name} of the line: {line}"))
}

/// What the spacing counts of the look of `line`, in milliseconds: its time up to its store.
fn git_ms(line: &str) -> u64 {
    ms_of(line, "total").saturating_sub(ms_of(line, "store"))
}

/// The look of `line` was the timer's and its git work took more than a tenth of the timer of [`BURST_EVERY_MS`]: the
/// spacing holds the look after it past its tick. Says so for the record; a look that did not is a test that did not show its
/// trigger.
fn left_the_timer(line: &str) {
    assert_eq!(
        field(line, "by"),
        Some("timer"),
        "TRIGGER PRESENT is not shown: the held look was not the timer's: {line}"
    );
    let git = git_ms(line);
    assert!(
        git > BURST_EVERY_MS / 10,
        "TRIGGER PRESENT is not shown: the git work of the held look took {git} ms, no more than a tenth of the timer: {line}"
    );
    println!(
        "TRIGGER PRESENT: a look of the timer of {BURST_EVERY_MS} ms whose git work took {git} ms (more than {} ms): the spacing \
         holds the next look past its tick: {line}",
        BURST_EVERY_MS / 10
    );
}

/// Whether the field `keep` of `line` is a number of milliseconds above 0: a keep ran to its end.
fn keep_ran(line: &str) -> bool {
    field(line, "keep").is_some_and(|v| v.parse::<u64>().is_ok_and(|ms| ms > 0))
}

/// Whether `line` names a kept pack: `kept=` is a number of bytes above 0.
fn names_a_kept_pack(line: &str) -> bool {
    field(line, "kept").is_some_and(|v| v.parse::<u64>().is_ok_and(|bytes| bytes > 0))
}

/// The worker ends as a signal asks, so that it ends by itself.
async fn ends_as_asked(run: &mut Run) {
    run.d.process.as_ref().unwrap().signal("TERM");
    assert_eq!(
        exit_of(run, LONG).await,
        Some(Some(143)),
        "log:\n{}",
        run.log()
    );
}

/// After a burst of commits and a rest, exactly one look runs its bundle step without a kept pack. The first look of
/// the timer after the burst finds a moved head, and its git work leaves the timer; the keep follows that look at once, its line
/// carries what came of the keep, and the bundle step of the next look reads the kept pack. Release 5.28 kept only after a second
/// look of the same head, so two looks ran the slow step.
#[tokio::test(flavor = "multi_thread")]
async fn b932_after_a_burst_one_look_runs_its_bundle_step_without_a_kept_pack() {
    let mut run = Run::start("b932-burst", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    let before = run.bundle_steps().len();
    run.burst(1, 5);
    let first = run
        .held_look("a.txt", "first after the burst\n", SLOW, |_| {})
        .await;
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert_eq!(field(&line, "engine"), Some("moved"), "{line}");
    println!(
        "TRIGGER PRESENT: the first look after a burst of 5 commits found a moved head and left the timer; release 5.28 keeps \
         nothing after it"
    );
    assert!(
        keep_ran(&line) && names_a_kept_pack(&line),
        "the line of the first look after the burst carries the keep that followed it at once (keep= its milliseconds, kept= \
         the bytes of the pack): {line}"
    );
    for content in ["second after the burst\n", "third after the burst\n"] {
        assert!(
            run.write_and_wait("a.txt", content, LONG).await.is_some(),
            "log:\n{}",
            run.log()
        );
    }
    let steps = run.bundle_steps()[before..].to_vec();
    assert_eq!(
        steps,
        vec![false, true, true],
        "after the burst, the bundle steps of three looks: only the first runs without a kept pack (release 5.28: the first \
         two); lines:\n{}",
        run.look_lines().join("\n")
    );
    assert_eq!(keeps_started(&run.dir), 1, "one keep for the head");
    ends_as_asked(&mut run).await;
}

/// (b), a pin: a head that moved while the look ran gets no keep at once. The engine commits while the stand-in holds the
/// bundle step of the first look after the burst: the head the look read at its start is not the engine's head after the bundle
/// step. Release 5.28 keeps nothing after such a look either.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_head_that_moved_while_the_look_ran_gets_no_keep_at_once() {
    let mut run = Run::start("b932-moved", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    run.burst(1, 5);
    let first = run
        .held_look("a.txt", "first after the burst\n", SLOW, |run| {
            run.commit(99);
        })
        .await;
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert_eq!(field(&line, "engine"), Some("moved"), "{line}");
    println!("TRIGGER PRESENT: the engine committed while the bundle step of that look was held");
    assert_eq!(
        (field(&line, "keep"), keeps_started(&run.dir)),
        (Some("0"), 0),
        "no keep follows at once a look during which the engine's head moved: {line}"
    );
    ends_as_asked(&mut run).await;
}

/// (c), a pin: when the engine's packs changed between the look's start and its end, no keep follows at once. The engine's
/// store is packed (`git repack -a -d`, what git's own `gc` does) while the stand-in holds the bundle step of the first look
/// after the burst. Release 5.28 keeps nothing after such a look either.
#[tokio::test(flavor = "multi_thread")]
async fn b932_packs_that_changed_while_the_look_ran_get_no_keep_at_once() {
    let mut run = Run::start("b932-packs", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    run.burst(1, 5);
    let packs = run.d.worktree().join(".git/objects/pack");
    let before: Vec<String> = names(&packs)
        .into_iter()
        .filter(|n| n.ends_with(".pack"))
        .collect();
    let first = run
        .held_look("a.txt", "first after the burst\n", SLOW, |run| {
            git(&run.d.worktree(), &["repack", "-a", "-d", "-q"]);
        })
        .await;
    let after: Vec<String> = names(&packs)
        .into_iter()
        .filter(|n| n.ends_with(".pack"))
        .collect();
    assert_ne!(
        before, after,
        "TRIGGER PRESENT is not shown: the repack left the engine's packs as they were"
    );
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert_eq!(field(&line, "engine"), Some("moved"), "{line}");
    println!(
        "TRIGGER PRESENT: the engine's packs changed while the bundle step of that look was held: {before:?} then {after:?}"
    );
    assert_eq!(
        (field(&line, "keep"), keeps_started(&run.dir)),
        (Some("0"), 0),
        "no keep follows at once a look during which the engine's packs changed: {line}"
    );
    ends_as_asked(&mut run).await;
}

/// A keep at once that git failed is followed by the rule of release 5.28. The stand-in fails the keep that follows
/// the first look after the burst: its line says `missed`. The second look of the same head is then followed by a keep, once, as
/// in release 5.28, and the third by none: no run is worse than under that rule.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_keep_at_once_that_was_missed_is_followed_by_the_rule_of_the_second_look() {
    let mut run = Run::start("b932-missed", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    std::fs::write(run.dir.join("keep-fail"), "").unwrap();
    run.burst(1, 5);
    let first = run
        .held_look("a.txt", "first after the burst\n", SLOW, |_| {})
        .await;
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert_eq!(
        field(&line, "keep"),
        Some("missed"),
        "the keep that followed the first look after the burst at once was missed, and its line says so: {line}"
    );
    std::fs::remove_file(run.dir.join("keep-fail")).unwrap();
    let second = run
        .write_and_wait("a.txt", "second after the burst\n", LONG)
        .await
        .unwrap();
    let line = run.line_when_stored(second).await;
    assert!(
        field(&line, "engine") == Some("same") && keep_ran(&line) && names_a_kept_pack(&line),
        "the second look of the same head is followed by a keep, as in release 5.28: {line}"
    );
    let third = run
        .write_and_wait("a.txt", "third after the burst\n", LONG)
        .await
        .unwrap();
    let line = run.line_when_stored(third).await;
    assert_eq!(field(&line, "keep"), Some("0"), "{line}");
    assert_eq!(
        keeps_started(&run.dir),
        2,
        "the keep at once and the one keep of the second look"
    );
    ends_as_asked(&mut run).await;
}

/// A keep at once that the next look cut is followed by the rule of release 5.28. The stand-in holds the keep that
/// follows the first look after the burst until the next look starts: that look ends it, and its line says `ended`. That look is
/// the second of the same head and is followed by a keep, once, as in release 5.28.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_keep_at_once_that_was_cut_is_followed_by_the_rule_of_the_second_look() {
    let mut run = Run::start("b932-cut", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    run.hold_the_keep();
    run.burst(1, 5);
    let first = run
        .held_look("a.txt", "first after the burst\n", SLOW, |_| {})
        .await;
    let pid = run.the_held_keep(first).await;
    println!(
        "TRIGGER PRESENT: a keep followed the first look after the burst at once and its git is held"
    );
    let from = run.bundles().len();
    run.write("a.txt", "second after the burst\n");
    assert!(
        gone(pid).await,
        "the look that started next ended the keep's git; log:\n{}",
        run.log()
    );
    let _ = std::fs::remove_file(run.dir.join("keep-hold"));
    let second = run
        .wait_copy(from, "a.txt", "second after the burst\n", LONG)
        .await
        .unwrap();
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert_eq!(
        field(&line, "keep"),
        Some("ended"),
        "the line of the look whose keep the next look cut: {line}"
    );
    let line = run.line_when_stored(second).await;
    assert!(
        field(&line, "engine") == Some("same") && keep_ran(&line) && names_a_kept_pack(&line),
        "the second look of the same head is followed by a keep, as in release 5.28: {line}"
    );
    assert_eq!(
        keeps_started(&run.dir),
        2,
        "the keep at once and the one keep of the second look"
    );
    ends_as_asked(&mut run).await;
}

/// A keep that outlasts the tick of the timer is not cut. The second look of a head is the timer's and its bundle step
/// is held for 2 s, so the spacing holds the look after it for 15 s where the tick comes after 3 s. The stand-in holds the keep
/// that follows that look until the tick has passed: the keep's git still runs then, and when it is let go the keep runs to its
/// end. The line of the look names its milliseconds and the bytes of the pack, not `ended`, and the bundle step of the next look
/// reads the kept pack. Release 5.28 ended the keep at the tick and marked the head as tried: every later look ran the slow step.
///
/// The look that first finds the head stores no copy (the bucket refuses it), so that the keep is the one of the second look
/// under both rules, whatever the machine does: the held look is the look of the tick that tries the refused copy again.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_keep_that_outlasts_the_tick_is_not_cut() {
    let mut run = Run::start("b932-tick", BURST_EVERY_MS, vec![retry_at_the_tick()]).await;
    run.behind_a_look("a.txt").await;
    run.burst(1, 3);
    run.hold_the_keep();
    let from = run.bundles().len();
    run.first_look_of_a_head_stores_nothing("a.txt", "second, of the timer\n")
        .await;
    run.hold_the_bundle();
    assert!(
        wait_for(LONG, || run.dir.join("bundle-running").exists()).await,
        "TRIGGER PRESENT is not shown: no look reached its bundle step; log:\n{}",
        run.log()
    );
    // The look started before its bundle step was seen: its tick comes 3 s after its start, so it has passed 3 s from now.
    let tick_passed = tokio::time::Instant::now() + Duration::from_millis(BURST_EVERY_MS);
    tokio::time::sleep(HELD).await;
    run.let_the_bundle_go();
    let second = run
        .wait_copy(from, "a.txt", "second, of the timer\n", LONG)
        .await
        .unwrap();
    let pid = run.the_held_keep(second).await;
    tokio::time::sleep_until(tick_passed + Duration::from_millis(500)).await;
    println!(
        "TRIGGER PRESENT: the keep's git is held, and the tick of the look it follows (3 s after that look's start) passed half \
         a second ago; the spacing holds the next look for 15 s"
    );
    assert!(
        alive(pid)
            && !run
                .look_lines()
                .iter()
                .any(|l| field(l, "keep") == Some("ended")),
        "the tick of the timer does not end a keep: its git still runs after the tick; lines:\n{}",
        run.look_lines().join("\n")
    );
    let _ = std::fs::remove_file(run.dir.join("keep-hold"));
    let line = run.line_when_stored(second).await;
    left_the_timer(&line);
    assert!(
        field(&line, "engine") == Some("same") && keep_ran(&line) && names_a_kept_pack(&line),
        "the keep that outlasted the tick ran to its end: the line names its milliseconds and the bytes of the pack: {line}"
    );
    let before = run.bundle_steps().len();
    assert!(
        run.write_and_wait("a.txt", "third\n", LONG).await.is_some(),
        "log:\n{}",
        run.log()
    );
    assert_eq!(
        run.bundle_steps()[before..].to_vec(),
        vec![true],
        "the bundle step of the look after that keep reads the kept pack"
    );
    assert_eq!(keeps_started(&run.dir), 1);
    ends_as_asked(&mut run).await;
}

/// What one turn of the tests of a look's start measured ([`next_look_after_a_held_one`]).
struct NextLook {
    /// How long after the copy of the held look was seen stored the copy of the look after it was seen stored.
    took: Duration,
    /// What the product's own rule allows for that at the most: the spacing after the held look (nine times its git work, between
    /// one and five periods of the timer), the time the next look's own line names, and the two seconds a git that the worker
    /// ends has to be gone.
    allowed: Duration,
    /// How many milliseconds after the spacing the next look started, as this turn can read it: `took`, less the time the next
    /// look's own line names (its start), less the spacing computed from the held look's line (nine times its git work, at most
    /// five periods of the timer). The machine and the test's own readings are in it, the same in a turn with the keep and in one
    /// without.
    late_ms: i64,
    /// The lines of the held look and of the look after it.
    lines: (String, String),
}

/// One turn of the tests of a look's start, with the keep on or off. A look of the timer has its bundle step held for
/// [`SLOW`], so the spacing holds the look after it past its tick. With the keep on, the stand-in holds the keep that follows the
/// held look without end; the look after the held one ends it.
///
/// `wake` false: the held look is the second look of a head, its keep is the keep of the second look, and the look after it
/// is the timer's. The look that first finds that head stores no copy (the bucket refuses it), so that no keep at once
/// follows it, whatever the machine does; the held look is the look of the tick that tries the refused copy again. `wake` true
/// (ruling 37 (b) of the lead): the held look is the first look after a burst, a tool call ends while it runs, its keep is the
/// keep at once, which starts after the task has taken the tool call's wake, and the look after it is the tool call's.
///
/// The trigger of a turn with a wake is proved in two parts ([`first_after_the_burst`]), and a turn is judged by its keep
/// only when both hold. The burst, the hold of the bundle step and the write must all fall before the timer's next look:
/// the test stands right behind a look when it starts them, and on a crowded machine the 3 s to the next tick can pass
/// while it makes the burst's commits. A look that starts among those steps finds a part of the burst or misses the write,
/// the stand-in holds the bundle step of that look, and the copy that holds the write is then the copy of a later look that
/// nothing held and that found no new head. This part needs no line of the worker's log (the stand-in's list of git calls
/// and the bucket show it) and is checked first, before the test looks at the held keep. And the held look must end before
/// the tick that follows its own start, 3 s after it: a look that takes longer, as it can on a machine whose disk is busy,
/// ends with that tick due, and a tick that overtakes a wake starts the look after as the timer's. This part is read from
/// the held look's own line, after the test has looked at the held keep, counted the copies and written, when the lines of
/// both looks are in the log. With the keep on, the worker writes the line of the held look only when the keep is cut,
/// which is when the look after it starts: a test that waits for that line before it looks at the keep and counts the
/// copies does both too late, and it failed when it was itself half a second late. A turn that lacks either part
/// shows nothing about a keep: the answer is `Err` with what was missing, the worker is ended as asked, and
/// [`HeldLook::turn`] makes the turn again.
async fn next_look_after_a_held_one(
    label: &str,
    keep_on: bool,
    wake: bool,
) -> Result<NextLook, String> {
    next_look_after_a_held_one_holding(label, keep_on, wake, false).await
}

/// [`next_look_after_a_held_one`], with what the stand-in holds named. With `form` it holds the form's
/// `index-pack --stdin` through `check-hold` where it otherwise holds the `pack-objects` of release 5.28's keep through
/// `keep-hold` (behind a form that the stand-in's rule makes give way at its filter): the look after the held one ends a
/// form that is in its own git.
async fn next_look_after_a_held_one_holding(
    label: &str,
    keep_on: bool,
    wake: bool,
    form: bool,
) -> Result<NextLook, String> {
    let mut more = vec![retry_at_the_tick()];
    if !keep_on {
        more.push(("AGENTD_DIRECT_KEEP_COMMITS", "0".to_string()));
    }
    let mut run = Run::start(label, BURST_EVERY_MS, more).await;
    run.behind_a_look("a.txt").await;
    let calls = git_calls(&run.dir).len();
    run.burst(1, 3);
    if form {
        run.hold_the_form("check-hold", "check-running");
    } else {
        run.hold_the_keep();
    }
    let from = run.bundles().len();
    let held_work = "written for the look whose bundle step is held\n";
    if wake {
        run.hold_the_bundle();
        run.write("a.txt", held_work);
    } else {
        run.first_look_of_a_head_stores_nothing("a.txt", held_work)
            .await;
        run.hold_the_bundle();
    }
    // How many git calls the worker started while the test made the burst, set the holds and wrote: none, when no look ran
    // among those steps.
    let among = git_calls(&run.dir).len() - calls;
    assert!(
        wait_for(LONG, || run.dir.join("bundle-running").exists()).await,
        "TRIGGER PRESENT is not shown: no look reached its bundle step after the write; log:\n{}",
        run.log()
    );
    if wake {
        run.tool_call_ends().await;
        println!(
            "TRIGGER PRESENT: a tool call ended while the bundle step of the first look after the burst was held: its wake is \
             there when the look ends"
        );
    }
    tokio::time::sleep(SLOW).await;
    run.let_the_bundle_go();
    assert!(
        wait_for(LONG, || run.stored() > from).await,
        "the look whose bundle step was held stores its copy; log:\n{}",
        run.log()
    );
    let seen = Instant::now();
    let held = run
        .wait_copy(from, "a.txt", held_work, LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the copy of the held look holds its work; log:\n{}",
                run.log()
            )
        });
    // First what needs no line of the worker's log: no look ran among the test's steps, and the copy that holds the write
    // is the first one after the burst. The held look's own line is read further down: with the keep on it is not written
    // before the look after the held one starts.
    if wake && let Some(missing) = first_after_the_burst(among, from, held, "") {
        ends_as_asked(&mut run).await;
        return Err(missing);
    }
    if keep_on && form {
        run.the_held_form(held, "check-running").await;
    } else if keep_on {
        run.the_held_keep(held).await;
    }
    let first = from;
    let from = run.bundles().len();
    // With the keep on, the keep's git is held at this moment: the test has just seen it held and has waited for nothing
    // since.
    run.write("a.txt", "third, written while the keep's git was held\n");
    assert!(
        wait_for(LONG, || run.stored() > from).await,
        "the copy of the look after the held one is stored; log:\n{}",
        run.log()
    );
    let took = seen.elapsed();
    assert!(
        wait_for(LONG, || !run.line_of(from).is_empty()
            && !run.line_of(held).is_empty())
        .await,
        "the worker's log has the lines of both looks; lines:\n{}",
        run.look_lines().join("\n")
    );
    let (before, line) = (run.line_of(held), run.line_of(from));
    // Then what the held look's own line says: its bundle step is the one that was held, it found a new head, and it ended
    // before the tick that follows its start.
    if wake && let Some(missing) = first_after_the_burst(among, first, held, &before) {
        ends_as_asked(&mut run).await;
        return Err(missing);
    }
    left_the_timer(&before);
    if form {
        assert_eq!(
            run.form_calls(),
            (usize::from(keep_on), 0, 0),
            "TRIGGER PRESENT is not shown: the keep is {}, and with the keep on the held call is the form's indexing, with no \
             filter call and no call of the older keep; lines:\n{}",
            if keep_on { "on" } else { "off" },
            run.look_lines().join("\n")
        );
    } else {
        assert_eq!(
            keeps_started(&run.dir),
            usize::from(keep_on),
            "TRIGGER PRESENT is not shown: the keep is {}; lines:\n{}",
            if keep_on { "on" } else { "off" },
            run.look_lines().join("\n")
        );
    }
    assert_eq!(
        (field(&before, "keep"), field(&line, "by")),
        (
            Some(if keep_on { "ended" } else { "0" }),
            Some(if wake { "tool" } else { "timer" })
        ),
        "the look after the held one is the {} and its start ended the keep that was held; the held look: {before}; the look \
         after it: {line}",
        if wake { "tool call's" } else { "timer's" }
    );
    let wait = (9 * git_ms(&before)).clamp(BURST_EVERY_MS, 5 * BURST_EVERY_MS);
    let allowed = Duration::from_millis(wait + ms_of(&line, "total")) + GIT_END;
    let spacing = (9 * git_ms(&before)).min(5 * BURST_EVERY_MS);
    let late_ms = i64::try_from(took.as_millis()).unwrap()
        - i64::try_from(ms_of(&line, "total") + spacing).unwrap();
    ends_as_asked(&mut run).await;
    Ok(NextLook {
        took,
        allowed,
        late_ms,
        lines: (before, line),
    })
}

/// Whether the look whose bundle step a turn with a wake held is the first look after the burst and holds the
/// write, from three things the test reads for itself; `Some` says which of them is missing, `None` that the trigger is
/// there. `among`: how many git calls the worker started, by the stand-in's own list, between the start of the burst and
/// the write: a look reads the engine's head with a git call when it starts and the tree with another, so with none among
/// the steps the next look read the head after the last commit of the burst and the tree after the write. `from` and
/// `held`: how many copies the bucket held before the write, and the index of the copy that holds the write: the same,
/// when no look stored a copy between them. `held_line`: the worker's line of that copy's look: the stand-in held its
/// bundle step for [`SLOW`] at the least, it found a head that the look before it had not found, and it took less than
/// the timer's period ([`BURST_EVERY_MS`]), so the tick that follows its start had not come when it ended and the wake
/// was the first thing the copy task found. An empty line shows nothing missing by the line: a turn asks first with no
/// line, for the two things that need none, and again with the line once both looks are logged.
fn first_after_the_burst(
    among: usize,
    from: usize,
    held: usize,
    held_line: &str,
) -> Option<String> {
    if among > 0 {
        return Some(format!(
            "the worker started {among} git calls while the test made the burst, set the holds and wrote: a look ran among \
             those steps"
        ));
    }
    if held != from {
        return Some(format!(
            "the copy that holds the write is copy {held} and not copy {from}, the first one after the burst: a look \
             stored a copy between the burst and the write"
        ));
    }
    if held_line.is_empty() {
        return None;
    }
    let bundle_ms = field(held_line, "bundle").and_then(|ms| ms.parse::<u128>().ok());
    if field(held_line, "engine") != Some("moved")
        || bundle_ms.is_none_or(|ms| ms < SLOW.as_millis())
    {
        return Some(format!(
            "the look that stored the write is not the look whose bundle step was held for {SLOW:?} or found no new head: \
             {held_line}"
        ));
    }
    let total_ms = field(held_line, "total").and_then(|ms| ms.parse::<u64>().ok());
    if total_ms.is_none_or(|ms| ms >= BURST_EVERY_MS) {
        return Some(format!(
            "the held look took as long as the timer's period of {BURST_EVERY_MS} ms or longer, so the tick that follows \
             its start came before it ended and overtook the wake: {held_line}"
        ));
    }
    None
}

/// The four things [`first_after_the_burst`] reads, each alone: a git call of the worker among the test's
/// steps, a copy stored between the burst and the write, a line that names a bundle step shorter than the hold or no new
/// head, and a line of a look that took the timer's period or longer show that the trigger is missing; the line of a held
/// first look after a burst shows nothing missing, and neither does a line that is not there yet. Without a line the first
/// two still show: a turn asks for them before it has the line.
#[test]
fn b932_a_turn_with_a_wake_proves_that_its_held_look_is_the_first_after_the_burst() {
    let held = "agentd: direct turn copy look n=3 stored by=timer engine=moved ms total=900 tidy=6 tree=40 head=10 \
                commit=20 bundle=640 hash=0 store=180 keep=ended bytes=2010 kept=0";
    assert_eq!(first_after_the_burst(0, 2, 2, held), None);
    assert_eq!(first_after_the_burst(0, 2, 2, ""), None);
    assert!(first_after_the_burst(1, 2, 2, "").is_some_and(|why| why.contains("1 git calls")));
    assert!(
        first_after_the_burst(0, 2, 3, "").is_some_and(|why| why.contains("copy 3 and not copy 2"))
    );
    assert!(first_after_the_burst(1, 2, 2, held).is_some_and(|why| why.contains("1 git calls")));
    assert!(
        first_after_the_burst(0, 2, 3, held)
            .is_some_and(|why| why.contains("copy 3 and not copy 2"))
    );
    // The lines of the run that failed on a crowded machine: the look that stored the write was not held and found no
    // new head.
    let not_held = "agentd: direct turn copy look n=4 stored by=timer engine=same ms total=2317 tidy=6 tree=572 head=10 \
                    commit=148 bundle=17 hash=0 store=1560 keep=0 bytes=2010 kept=0";
    assert!(first_after_the_burst(0, 3, 3, not_held).is_some_and(|why| why.contains("bundle=17")));
    let no_new_head = held.replace("engine=moved", "engine=same");
    assert!(first_after_the_burst(0, 2, 2, &no_new_head).is_some());
    let short = held.replace("bundle=640", "bundle=599");
    assert!(first_after_the_burst(0, 2, 2, &short).is_some());
    // A line of a run under an io load: the look that was held took 3,064 ms, its next tick came before it ended, and
    // the look after it was the timer's.
    let overtaken = "agentd: direct turn copy look n=3 stored by=timer engine=moved ms total=3064 tidy=7 tree=304 head=227 \
                     commit=454 bundle=1446 hash=0 store=623 keep=0 bytes=1771 kept=0";
    assert!(
        first_after_the_burst(0, 2, 2, overtaken).is_some_and(|why| why.contains("total=3064"))
    );
    let just_in_time = held.replace("total=900", "total=2999");
    assert_eq!(first_after_the_burst(0, 2, 2, &just_in_time), None);
    let at_the_tick = held.replace("total=900", "total=3000");
    assert!(first_after_the_burst(0, 2, 2, &at_the_tick).is_some());
}

/// What two readings of [`NextLook::late_ms`] can differ by with nothing between them: the stand-in sees within
/// 50 ms that its bundle step is let go and the test sees a stored copy within 20 ms, twice in a turn; the lines name whole
/// milliseconds, and the git work of the held look counts nine times in the spacing.
const READING_MS: i64 = 150;
/// How many pairs of turns, one with the keep and one without, [`the_keep_never_moves_the_start_of_the_next_look`]
/// runs at most. A machine that is crowded for the length of one turn makes one pair differ; a keep that moves the start of a
/// look makes every pair differ. So one pair of the five may show the look later with the keep on, and no more (ruling 43 (e)
/// of the lead).
const START_PAIRS: u32 = 5;

/// The look after a held one starts at the same moment with the keep on and off. One pair of turns measures how long
/// after the spacing the next look started ([`NextLook::late_ms`]): with the keep on and its git held without end, and with the
/// keep off. With the keep on that look may start later than with the keep off by no more than what the turn without the keep
/// itself shows as its distance from the spacing, or by [`READING_MS`] when that is more. A pair that differs by more shows
/// the look later with the keep on. The test makes [`START_PAIRS`] pairs at the most and fails when more than one of them
/// shows the look later with the keep on (ruling 43 (e) of the lead; before, one pair of the five that did not show it was
/// enough to pass). It stops as soon as that is decided: after four pairs when none showed it, and after the second pair
/// that shows it. It prints every pair's figures. Each turn also holds the upper bound of the product's own rule
/// ([`NextLook::allowed`]).
async fn the_keep_never_moves_the_start_of_the_next_look(label: &str, held: HeldLook) {
    // Of each pair: the milliseconds after the spacing with the keep on and with the keep off, and whether the pair shows the
    // look later with the keep on.
    let mut figures: Vec<(i64, i64, bool)> = Vec::new();
    for pair in 0..START_PAIRS {
        let on = held.turn(&format!("{label}-on-{pair}"), true).await;
        let off = held.turn(&format!("{label}-off-{pair}"), false).await;
        println!(
            "pair {pair}: the look after the held one started {} ms after the spacing with the keep on and held (its copy {:?} \
             after the held look's copy was seen, allowed {:?}), {} ms after it with the keep off ({:?}, allowed {:?})\n\
             keep on, the held look: {}\nkeep on, the look after it: {}\nkeep off, the held look: {}\nkeep off, the look after \
             it: {}",
            on.late_ms,
            on.took,
            on.allowed,
            off.late_ms,
            off.took,
            off.allowed,
            on.lines.0,
            on.lines.1,
            off.lines.0,
            off.lines.1
        );
        assert!(
            on.took <= on.allowed,
            "with the keep's git held without end the next look's copy was stored {:?} after the held look's copy was seen; the \
             spacing, the look's own time and the time a git has to end allow {:?}: the keep held the look",
            on.took,
            on.allowed
        );
        assert!(
            off.took <= off.allowed,
            "TRIGGER PRESENT is not shown: with the keep off the next look's copy was stored {:?} after the held look's copy \
             was seen, more than the {:?} the rule allows: the machine, not a keep, held the look",
            off.took,
            off.allowed
        );
        let bound = READING_MS.max(off.late_ms);
        figures.push((on.late_ms, off.late_ms, on.late_ms - off.late_ms > bound));
        if start_pairs_say(later_of(&figures), pair + 1).is_some() {
            break;
        }
    }
    let (later, made) = (later_of(&figures), u32::try_from(figures.len()).unwrap());
    println!(
        "every pair's figures (milliseconds after the spacing with the keep on, the same with the keep off, whether the pair \
         shows the look later with the keep on): {figures:?}"
    );
    assert_eq!(
        start_pairs_say(later, made),
        Some(true),
        "in {later} of {made} pairs of turns the look after the held one started later with the keep on than with the keep off, \
         by more than the turn without the keep shows; one pair of {START_PAIRS} may (milliseconds after the spacing with the \
         keep on, the same with the keep off, whether the pair shows it): {figures:?}: a keep moved the start of a look"
    );
}

/// How many of the pairs of turns show the look later with the keep on.
fn later_of(figures: &[(i64, i64, bool)]) -> u32 {
    u32::try_from(figures.iter().filter(|pair| pair.2).count()).unwrap()
}

/// (f), ruling 43 (e) of the lead: what the pairs of turns made so far say, when `later` of the `made` pairs show the
/// look later with the keep on. `Some(false)`: more than one pair shows it, a keep moved the start of a look. `Some(true)`:
/// at most one pair shows it, and the pairs still to come of [`START_PAIRS`] cannot change that. `None`: not decided, one
/// more pair is made.
fn start_pairs_say(later: u32, made: u32) -> Option<bool> {
    let to_come = START_PAIRS.saturating_sub(made);
    if later > 1 {
        Some(false)
    } else if later + to_come <= 1 {
        Some(true)
    } else {
        None
    }
}

/// The tests of a look's start fail when more than one pair of five shows the look later
/// with the keep on. No pair later: decided after four pairs, because the fifth alone cannot fail the test. One pair later:
/// all five are made, and the test passes. Two pairs later: the test fails at once. Before, the first pair that did not show
/// the look later passed the test, so four pairs of five could show it.
#[test]
fn b932_the_start_tests_fail_when_more_than_one_pair_of_five_shows_the_look_later() {
    for made in 1..=3 {
        assert_eq!(start_pairs_say(0, made), None, "no pair later, {made} made");
    }
    assert_eq!(start_pairs_say(0, 4), Some(true));
    for made in 1..=4 {
        assert_eq!(
            start_pairs_say(1, made),
            None,
            "one pair later, {made} made"
        );
    }
    assert_eq!(start_pairs_say(1, 5), Some(true));
    for made in 2..=5 {
        assert_eq!(
            start_pairs_say(2, made),
            Some(false),
            "two pairs later, {made} made"
        );
    }
    assert_eq!(start_pairs_say(5, 5), Some(false));
}

/// (f), a pin: a keep in flight never moves the start of a look. The look after a look that left the timer starts when the
/// spacing says, with the keep on and its git held without end, and with the keep off: the start of that look, read from its
/// own line, lies as far behind the spacing in both turns, and in both its copy is stored within what the product's own rule
/// allows. Release 5.28 holds this too (its keep ends at the tick, before the spacing has run out).
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_keep_in_flight_never_moves_the_start_of_a_look() {
    the_keep_never_moves_the_start_of_the_next_look("b932-start", HeldLook::SecondOfTheTimer).await;
}

/// A keep at once that started after a taken wake never moves the start of the tool call's
/// look. A tool call ends while the first slow look after a burst runs. The keep at once starts in the wait that follows the
/// taken wake, and its git is held without end: the look that the tool call asked for starts when the spacing says, as with the
/// keep off, and its start ends the keep. Before, the taken wake left that keep unstarted.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_keep_at_once_after_a_taken_wake_never_moves_the_start_of_the_tool_calls_look() {
    the_keep_never_moves_the_start_of_the_next_look("b932-start-wake", HeldLook::FirstWithAWake)
        .await;
}

/// A tool call that ends while a keep runs does not end it; the look that the tool call starts does. The look a keep
/// follows had its bundle step held for 2 s, so the look that a tool call asks for is held by the spacing for 18 s. A tool call
/// ends while the keep's git is held; then the keep is let go: it runs to its end, and the line of its look names its
/// milliseconds and the bytes of the pack. Release 5.28 ended the keep at the tool call's wake. The copy that the wake asked for
/// is stored by a look that the tool call started: the wake is not lost. (A request to end ends a keep, as before:
/// `b860_kept_a_stop_is_not_held_by_the_keep`.)
///
/// The timer is ten minutes: the look a keep follows is the timer's by the tick that tries a refused copy again, which comes
/// 10 s after the refused look started here, so that the test has the time to hold its bundle step.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_tool_call_that_ends_while_a_keep_runs_does_not_end_it() {
    let mut run = Run::start(
        "b932-wake",
        NO_TICK_MS,
        vec![("AGENTD_DIRECT_SNAPSHOT_RETRY_MS", "10000".to_string())],
    )
    .await;
    run.hold_the_keep();
    run.commit(1);
    let from = run.bundles().len();
    run.write("g.txt", "found by the look of a tool call\n");
    run.tool_call_ends().await;
    assert!(
        run.wait_copy(from, "g.txt", "found by the look of a tool call\n", LONG)
            .await
            .is_some(),
        "log:\n{}",
        run.log()
    );
    // The bucket refuses the copy of the second tool call's look; the tick that tries it again is the timer's, at the same head.
    let key = keys::snapshot_bundle_key(RUN);
    let attempts = run.d.aws.put_attempts(&key);
    let from = run.bundles().len();
    run.d.aws.fail_puts("snapshot.bundle", 4);
    let refused = "in the copy the bucket refused, and in the copy of the tick that tried again\n";
    run.write("g.txt", refused);
    run.tool_call_ends().await;
    assert!(
        wait_for(LONG, || run.d.aws.put_attempts(&key) >= attempts + 4).await,
        "TRIGGER PRESENT is not shown: the bucket did not refuse the copy of the second tool call's look; log:\n{}",
        run.log()
    );
    run.hold_the_bundle();
    assert!(
        wait_for(LONG, || run.dir.join("bundle-running").exists()).await,
        "TRIGGER PRESENT is not shown: the tick that tries the copy again did not reach its bundle step; log:\n{}",
        run.log()
    );
    tokio::time::sleep(HELD).await;
    run.let_the_bundle_go();
    let index = run
        .wait_copy(from, "g.txt", refused, LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the tick that tries a refused copy again stores it; log:\n{}",
                run.log()
            )
        });
    let pid = run.the_held_keep(index).await;

    // A third tool call ends while the keep's git is held: agentd has seen it end, so the copy task was woken.
    let at_the_call = "written when the third tool call ended\n";
    let from = run.bundles().len();
    run.write("g.txt", at_the_call);
    run.tool_call_ends().await;
    println!(
        "TRIGGER PRESENT: a tool call ended while the keep's git (process {pid}) was held; the look it asks for is held by the \
         spacing for nine times the 2 s of the look before"
    );
    let _ = std::fs::remove_file(run.dir.join("keep-hold"));
    let line = run.line_when_stored(index).await;
    assert!(
        git_ms(&line) >= 2_000,
        "TRIGGER PRESENT is not shown: the look the keep follows took less than the 2 s its bundle step was held: {line}"
    );
    assert!(
        field(&line, "by") == Some("timer") && keep_ran(&line) && names_a_kept_pack(&line),
        "a tool call that ended while the keep ran did not end it: the keep ran to its end, and the line of its look names its \
         milliseconds and the bytes of the pack: {line}"
    );
    let found = run.wait_copy(from, "g.txt", at_the_call, LONG).await;
    assert!(
        found.is_some(),
        "the copy that the tool call's wake asked for is stored (the timer is ten minutes): the wake is not lost; log:\n{}",
        run.log()
    );
    let line = run.line_when_stored(found.unwrap()).await;
    assert_eq!(
        field(&line, "by"),
        Some("tool"),
        "the look that stored that copy was the tool call's: {line}"
    );
    assert_eq!(keeps_started(&run.dir), 1);
    ends_as_asked(&mut run).await;
}

/// A tool call that ends while the first slow look after a burst runs does not cost that look
/// its keep at once. The wake of the tool call is there when the look ends, and the copy task takes it before it turns to the
/// keep. The keep at once starts all the same, in the wait for the floor and the spacing that follows the taken wake: the line
/// of the slow look names the keep's milliseconds and the bytes of the pack, the look that the tool call asked for is the tool
/// call's, and its bundle step reads the kept pack. Before, a taken wake left that keep unstarted (`keep=0`), and the next look
/// ran its bundle step without a kept pack again.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_tool_call_that_ends_while_the_slow_look_runs_does_not_cost_the_keep_at_once() {
    let mut run = Run::start("b932-wake-pending", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    let before = run.bundle_steps().len();
    run.burst(1, 5);
    run.hold_the_bundle();
    let from = run.bundles().len();
    run.write("a.txt", "first after the burst\n");
    assert!(
        wait_for(LONG, || run.dir.join("bundle-running").exists()).await,
        "TRIGGER PRESENT is not shown: no look reached its bundle step after the write; log:\n{}",
        run.log()
    );
    run.tool_call_ends().await;
    println!(
        "TRIGGER PRESENT: a tool call ended while the bundle step of the first look after the burst was held: its wake is there \
         when the look ends"
    );
    tokio::time::sleep(SLOW).await;
    run.let_the_bundle_go();
    let first = run
        .wait_copy(from, "a.txt", "first after the burst\n", LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the look whose bundle step was held stores its copy; log:\n{}",
                run.log()
            )
        });
    let second = run
        .write_and_wait(
            "a.txt",
            "second, for the look the tool call asked for\n",
            LONG,
        )
        .await
        .unwrap_or_else(|| {
            panic!(
                "the look that the tool call asked for stores its copy; log:\n{}",
                run.log()
            )
        });
    let line = run.line_when_stored(first).await;
    let next = run.line_when_stored(second).await;
    left_the_timer(&line);
    assert_eq!(field(&line, "engine"), Some("moved"), "{line}");
    assert_eq!(
        field(&next, "by"),
        Some("tool"),
        "TRIGGER PRESENT is not shown: the look after the held one was not the tool call's, so the copy task did not take the \
         wake before it turned to the keep: {next}"
    );
    println!(
        "TRIGGER PRESENT: the copy task took the tool call's wake after the first slow look of the timer, on a head and packs \
         that stood still: {line}\nthe look the tool call asked for: {next}"
    );
    assert!(
        keep_ran(&line) && names_a_kept_pack(&line),
        "a keep follows the first slow look after the burst at once, also when a tool call's wake was taken first (keep= its \
         milliseconds, kept= the bytes of the pack): {line}; the look the tool call asked for: {next}"
    );
    assert_eq!(
        run.bundle_steps()[before..].to_vec(),
        vec![false, true],
        "the bundle step of the tool call's look reads the kept pack; lines:\n{}",
        run.look_lines().join("\n")
    );
    assert_eq!(keeps_started(&run.dir), 1, "one keep for the head");
    ends_as_asked(&mut run).await;
}

/// (h), a pin: a look that the spacing does not hold past its tick keeps the rule of release 5.28. The timer is ten
/// minutes, so no look of this test leaves it. The first look of the timer that finds the engine's new head is followed by no
/// keep; the next look of the timer, of the same head, is followed by one.
///
/// A look of the timer comes without waiting for the timer: the bucket refuses the copy of a tool call's look, and the tick that
/// tries it again is the timer's, 10 s after the refused look started here, so that the engine's commit falls before it.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_look_that_stays_on_the_timer_gets_no_keep_at_once() {
    let mut run = Run::start(
        "b932-cheap",
        NO_TICK_MS,
        vec![("AGENTD_DIRECT_SNAPSHOT_RETRY_MS", "10000".to_string())],
    )
    .await;
    let key = keys::snapshot_bundle_key(RUN);
    let mut found = Vec::new();
    for (round, content) in [
        (1, "refused first, before the engine's commit\n"),
        (2, "refused again, at the same head\n"),
    ] {
        let attempts = run.d.aws.put_attempts(&key);
        let from = run.bundles().len();
        run.d.aws.fail_puts("snapshot.bundle", 4);
        run.write("h.txt", content);
        run.tool_call_ends().await;
        assert!(
            wait_for(LONG, || run.d.aws.put_attempts(&key) >= attempts + 4).await,
            "TRIGGER PRESENT is not shown: the bucket did not refuse the copy of the tool call's look; log:\n{}",
            run.log()
        );
        if round == 1 {
            // The refused look read the head the run started from; the look of the tick finds the engine's commit.
            run.commit(1);
        }
        let index = run
            .wait_copy(from, "h.txt", content, LONG)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "the tick that tries a refused copy again stores it; log:\n{}",
                    run.log()
                )
            });
        found.push(run.line_when_stored(index).await);
    }
    let (first, second) = (&found[0], &found[1]);
    assert_eq!(
        (field(first, "by"), field(first, "engine")),
        (Some("timer"), Some("moved")),
        "TRIGGER PRESENT is not shown: the first look of the timer did not find the engine's new head: {first}"
    );
    println!(
        "TRIGGER PRESENT: a look of a timer of ten minutes found a moved head; its git work took {} ms, which holds no look \
         past a tick: {first}",
        git_ms(first)
    );
    assert_eq!(
        field(first, "keep"),
        Some("0"),
        "a look that stays on the timer is followed by no keep at once: {first}"
    );
    assert!(
        (field(second, "by"), field(second, "engine")) == (Some("timer"), Some("same"))
            && keep_ran(second)
            && names_a_kept_pack(second),
        "the second look of the timer of the same head is followed by a keep, as in release 5.28: {second}"
    );
    assert_eq!(keeps_started(&run.dir), 1);
    ends_as_asked(&mut run).await;
}

/// (i), finding 9 of the challenge: what a short run pays. A run whose engine ends its turn right after the first look
/// after its burst pays one keep, the one that follows that look at once: one `pack-objects` of the run's commits, whose pack no
/// later look reads. Release 5.28 paid none in such a run, because its keep waited for a second look. The copy of the end is
/// stored and holds the work.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_short_run_pays_the_one_keep_after_its_burst() {
    let run = Run::start("b932-short", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    run.burst(1, 5);
    let last = "the last work of a short run\n";
    let first = run.held_look("a.txt", last, SLOW, |_| {}).await;
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert!(
        keep_ran(&line) && names_a_kept_pack(&line),
        "the keep followed the one look after the burst at once: {line}"
    );
    std::fs::write(&run.steps, "1000000\n").unwrap();
    assert!(
        wait_for(LONG, || run
            .records()
            .last()
            .is_some_and(|r| r["final"] == true))
        .await,
        "the engine's turn ends with the copy of the end; log:\n{}",
        run.log()
    );
    assert_eq!(
        keeps_started(&run.dir),
        1,
        "a short run pays one keep after its burst: one pack-objects, and no second one"
    );
    println!(
        "a short run paid one keep of {} ms for a pack of {} bytes that no later look read: {line}",
        field(&line, "keep").unwrap_or_default(),
        field(&line, "kept").unwrap_or_default()
    );
    assert_eq!(
        run.file_of(run.bundles().len() - 1, "a.txt").as_deref(),
        Some(last),
        "the copy of the end holds the work"
    );
}

/// (i), finding 10 of the challenge: a look that is slow for another reason than loose commits. The engine's store is
/// packed before the look (nothing of the run's commits is loose), and the stand-in holds the look's bundle step: the look
/// leaves the timer all the same, so a keep follows it at once. That keep costs two git calls: the one read of the engine's head
/// after the bundle step, and one `pack-objects` that finds nothing to pack and writes nothing. Its line names the keep's
/// milliseconds and no kept pack.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_look_that_is_slow_for_another_reason_costs_two_git_calls() {
    let mut run = Run::start("b932-other", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    run.burst(1, 3);
    git(&run.d.worktree(), &["repack", "-a", "-d", "-q"]);
    let calls = git_calls(&run.dir).len();
    let first = run
        .held_look(
            "a.txt",
            "first, after the engine packed its store\n",
            SLOW,
            |_| {},
        )
        .await;
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert_eq!(field(&line, "engine"), Some("moved"), "{line}");
    let window = git_calls(&run.dir)[calls..].to_vec();
    let head_reads = window
        .iter()
        .filter(|call| call.ends_with("rev-parse HEAD"))
        .count();
    let keeps = window.iter().filter(|call| is_the_keep(call)).count();
    println!(
        "the git calls from the write to the look's line: {:?}",
        window
            .iter()
            .map(|call| b860_kept_support::command_of(call))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        (head_reads, keeps),
        (2, 1),
        "the keep at once after a look that is slow for another reason: one read of the engine's head more than the look's own, \
         and one pack-objects; calls:\n{}",
        window.join("\n")
    );
    assert!(
        keep_ran(&line) && field(&line, "kept") == Some("0"),
        "that keep found nothing to pack: its line names its milliseconds and no kept pack: {line}"
    );
    assert!(
        !names(&run.private().join("kept/pack"))
            .iter()
            .any(|n| n.ends_with(".pack")),
        "no pack is kept"
    );
    ends_as_asked(&mut run).await;
}

/// A keep at once that found nothing to pack leaves the mark of ruling 43 (b) as it was,
/// so a burst that nothing packs still gets its keep at once. The engine packs its store, a look that is slow for another
/// reason leaves the timer on a moved head, and the keep at once that follows it finds nothing to pack: two git calls
/// (`b932_a_look_that_is_slow_for_another_reason_costs_two_git_calls`). Then the engine commits five times and nothing packs
/// those commits. The first look after that burst leaves the timer on the new head and reads no kept pack: the keep follows
/// it at once and stands on its line, and the two looks after it read the kept pack. After the burst exactly one look runs
/// its bundle step without a kept pack. Before, the keep that found nothing to pack set the mark: no keep at once followed
/// the first look after the burst, the keep came after the second look of that head, and two looks ran their bundle step
/// without a kept pack, as in release 5.28.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_burst_after_a_keep_at_once_that_found_nothing_to_pack_gets_its_keep_at_once() {
    let mut run = Run::start("b932-after-nothing", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    run.burst(1, 3);
    git(&run.d.worktree(), &["repack", "-a", "-d", "-q"]);
    let first = run
        .held_look(
            "a.txt",
            "first, after the engine packed its store\n",
            SLOW,
            |_| {},
        )
        .await;
    let line1 = run.line_when_stored(first).await;
    left_the_timer(&line1);
    assert_eq!(
        field(&line1, "engine"),
        Some("moved"),
        "TRIGGER PRESENT is not shown: {line1}"
    );
    assert!(
        keep_ran(&line1) && field(&line1, "kept") == Some("0"),
        "TRIGGER PRESENT is not shown: the keep at once after the first look did not find nothing to pack: {line1}"
    );
    println!("TRIGGER PRESENT: a keep at once found nothing to pack: {line1}");
    let before = run.bundle_steps().len();
    run.burst(4, 5);
    let second = run
        .held_look(
            "a.txt",
            "second, after a burst that nothing packed\n",
            SLOW,
            |_| {},
        )
        .await;
    let line2 = run.line_when_stored(second).await;
    let third = run
        .write_and_wait("a.txt", "third, of the same head\n", LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the second look after the burst stores its copy; log:\n{}",
                run.log()
            )
        });
    let line3 = run.line_when_stored(third).await;
    let fourth = run
        .write_and_wait("a.txt", "fourth, of the same head\n", LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the third look after the burst stores its copy; log:\n{}",
                run.log()
            )
        });
    let line4 = run.line_when_stored(fourth).await;
    let steps = run.bundle_steps()[before..].to_vec();
    println!(
        "line 2: {line2}\nline 3: {line3}\nline 4: {line4}\nkeeps started: {}; bundle steps after the burst (true: read a \
         kept pack): {steps:?}",
        keeps_started(&run.dir)
    );
    left_the_timer(&line2);
    assert_eq!(
        field(&line2, "engine"),
        Some("moved"),
        "TRIGGER PRESENT is not shown: {line2}"
    );
    assert!(
        keep_ran(&line2) && names_a_kept_pack(&line2),
        "the keep follows the first look after the burst at once and stands on its line (keep= its milliseconds, kept= the \
         bytes of the pack), also after a keep at once that found nothing to pack: {line2}; the looks after it: {line3}; \
         {line4}"
    );
    assert_eq!(
        steps,
        vec![false, true, true],
        "after the burst exactly one look runs its bundle step without a kept pack; lines:\n{}",
        run.look_lines().join("\n")
    );
    assert_eq!(
        keeps_started(&run.dir),
        2,
        "two keeps in the turn: the one that found nothing to pack, and the one after the burst"
    );
    ends_as_asked(&mut run).await;
}

/// Steady commits get one keep, for their first burst. The keep at once follows a look only
/// when that look read no kept pack. After the keep that follows the first slow look, the engine commits once before each of
/// three more looks of the timer; each of them finds a moved head, leaves the timer (its bundle step is held, which stands for
/// any slow step) and reads the kept pack: none is followed by a keep, as in release 5.28. Without the rule a keep followed
/// every one of them, and each packed every unpacked object of the run again: four keeps where release 5.28 makes one.
#[tokio::test(flavor = "multi_thread")]
async fn b932_steady_commits_get_one_keep_for_their_first_burst() {
    let mut run = Run::start("b932-steady", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    run.burst(1, 5);
    let first = run
        .held_look("a.txt", "first after the burst\n", SLOW, |_| {})
        .await;
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert!(
        keep_ran(&line) && names_a_kept_pack(&line),
        "TRIGGER PRESENT is not shown: no keep followed the first look after the burst at once: {line}"
    );
    let before = run.bundle_steps().len();
    let mut lines = Vec::new();
    for round in 0..3u32 {
        run.commit(100 + round);
        let index = run
            .held_look("a.txt", &format!("after commit {round}\n"), SLOW, |_| {})
            .await;
        let line = run.line_when_stored(index).await;
        left_the_timer(&line);
        assert_eq!(
            field(&line, "engine"),
            Some("moved"),
            "TRIGGER PRESENT is not shown: the look after commit {round} did not find a moved head: {line}"
        );
        lines.push(line);
    }
    assert_eq!(
        run.bundle_steps()[before..].to_vec(),
        vec![true, true, true],
        "TRIGGER PRESENT is not shown: the three looks on a moved head did not all read the kept pack; lines:\n{}",
        lines.join("\n")
    );
    println!(
        "TRIGGER PRESENT: three looks of the timer, each on a moved head, each left the timer and read the kept pack; keeps \
         started: {}; lines:\n{}",
        keeps_started(&run.dir),
        lines.join("\n")
    );
    let keeps = keeps_started(&run.dir);
    ends_as_asked(&mut run).await;
    assert_eq!(
        keeps,
        1,
        "one keep for the first burst; a look on a moved head that read the kept pack is followed by no keep, as in release \
         5.28; lines:\n{}",
        lines.join("\n")
    );
    for line in &lines {
        assert_eq!(
            field(line, "keep"),
            Some("0"),
            "no keep follows a look that read the kept pack on a moved head: {line}"
        );
    }
}

/// (ruling 37 (c) of the lead), a pin: a second burst after a kept pack keeps the rule of release 5.28. After the keep
/// that follows the first slow look, the engine commits five times more. The first look of the timer after that burst finds a
/// moved head, leaves the timer and reads the kept pack: no keep follows it at once. The second look of that head is followed
/// by a keep, once, and the third by none: the keep after a second look of the same head, as.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_second_burst_after_a_kept_pack_keeps_the_rule_of_the_second_look() {
    let mut run = Run::start("b932-second-burst", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    run.burst(1, 5);
    let first = run
        .held_look("a.txt", "first after the first burst\n", SLOW, |_| {})
        .await;
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert!(
        keep_ran(&line) && names_a_kept_pack(&line),
        "TRIGGER PRESENT is not shown: no keep followed the first look after the first burst at once: {line}"
    );
    let before = run.bundle_steps().len();
    run.burst(100, 5);
    let first = run
        .held_look("a.txt", "first after the second burst\n", SLOW, |_| {})
        .await;
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert_eq!(field(&line, "engine"), Some("moved"), "{line}");
    assert_eq!(
        run.bundle_steps()[before..].to_vec(),
        vec![true],
        "TRIGGER PRESENT is not shown: the first look after the second burst did not read the kept pack: {line}"
    );
    println!(
        "TRIGGER PRESENT: the first look after a second burst found a moved head, left the timer and read the kept pack: {line}"
    );
    assert_eq!(
        (field(&line, "keep"), keeps_started(&run.dir)),
        (Some("0"), 1),
        "no keep follows at once a look that read the kept pack: {line}"
    );
    let second = run
        .write_and_wait("a.txt", "second after the second burst\n", LONG)
        .await
        .unwrap();
    let line = run.line_when_stored(second).await;
    assert!(
        (field(&line, "by"), field(&line, "engine")) == (Some("timer"), Some("same"))
            && keep_ran(&line)
            && names_a_kept_pack(&line),
        "the second look of the same head is followed by a keep, as in release 5.28: {line}"
    );
    let third = run
        .write_and_wait("a.txt", "third after the second burst\n", LONG)
        .await
        .unwrap();
    let line = run.line_when_stored(third).await;
    assert_eq!(field(&line, "keep"), Some("0"), "{line}");
    assert_eq!(
        keeps_started(&run.dir),
        2,
        "the keep at once of the first burst and the one keep of the second look after the second burst"
    );
    ends_as_asked(&mut run).await;
}

/// A keep at once that did not end as kept is tried once and not again until a keep is
/// kept. The stand-in fails the `pack-objects` of every keep. The engine commits once before each of four looks of the
/// timer; each look leaves the timer, finds a moved head and reads no kept pack. The keep at once that follows the first is
/// missed, and no keep follows the other three. Before the mark each of the four started a `pack-objects` over every
/// unpacked object of the run, where release 5.28 starts none in this sequence: no head is found twice. Between two kept
/// packs at most one keep at once is not kept.
#[tokio::test(flavor = "multi_thread")]
async fn b932_steady_commits_whose_keeps_at_once_fail_get_one_keep_at_once() {
    let mut run = Run::start("b932-steady-fail", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    std::fs::write(run.dir.join("keep-fail"), "").unwrap();
    run.burst(1, 5);
    let mut lines = Vec::new();
    for round in 0..4u32 {
        if round > 0 {
            run.commit(100 + round);
        }
        let index = run
            .held_look("a.txt", &format!("look {round}\n"), SLOW, |_| {})
            .await;
        let line = run.line_when_stored(index).await;
        left_the_timer(&line);
        assert_eq!(field(&line, "engine"), Some("moved"), "{line}");
        lines.push(line);
    }
    let keeps = keeps_started(&run.dir);
    let steps = run.bundle_steps();
    println!(
        "keeps started over four moved heads: {keeps}; bundle steps (true: read a kept pack): {steps:?}\nthe lines of the four \
         looks:\n{}",
        lines.join("\n")
    );
    assert_eq!(
        field(&lines[0], "keep"),
        Some("missed"),
        "TRIGGER PRESENT is not shown: the keep at once after the first look was not missed: {}",
        lines[0]
    );
    assert!(
        steps.iter().all(|kept| !kept),
        "TRIGGER PRESENT is not shown: a look read a kept pack; bundle steps {steps:?}"
    );
    println!(
        "TRIGGER PRESENT: four looks of the timer left the timer, each on a moved head and without a kept pack, and the keep \
         at once after the first was missed"
    );
    assert_eq!(
        keeps,
        1,
        "a keep at once that was not kept is tried once and not again until a keep is kept: one keep over four moved heads; \
         lines:\n{}",
        lines.join("\n")
    );
    for line in &lines[1..] {
        assert_eq!(
            field(line, "keep"),
            Some("0"),
            "no keep follows the look: {line}"
        );
    }
    ends_as_asked(&mut run).await;
}

/// The lines of the worker's log that name a refused file call or a look that failed.
fn refused_or_failed(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.contains("has not answered an earlier call") || l.contains(" failed "))
        .map(str::to_string)
        .collect()
}

/// The longest a worker of these tests takes to end after a request to end that comes
/// while a keep runs through the wait for the spacing: 8 s. The spacing has more than 6 s to run then, and a worker that
/// waited for it would also have its last look and its end in front of it.
const END_WITHIN: Duration = Duration::from_secs(8);

/// A request to end ends the wait that a
/// keep runs through. The bundle step of the first look after a burst is held for 2 s, so the spacing holds the look after
/// it for 15 s, five periods of the 3 s timer, and the stand-in holds the `pack-objects` of the keep at once that follows
/// it. Once the tick of the timer has passed, the copy task is in its second wait, for the floor and the spacing, with the
/// keep as one arm: the request to end comes there. With `wake` a tool call ended while the slow look ran, so that second
/// wait is the one after a taken wake (ruling 37 (b)).
///
/// The worker ends as the signal asks while the spacing still has seconds to run, the copy that the request asked for
/// holds the work written right before it, no look fails, and the held git is gone. A wait that did not take the request
/// would end with the spacing: the worker would end after it. The test asserts that order, and beside it a bound of its
/// own (ruling 45 (b) of the lead): the worker ends within [`END_WITHIN`], 8 s, of the request; a review measured 307 to
/// 309 ms. The spacing ends no sooner than 15 s after the held bundle step was let go, because the git work of that look
/// ends after that moment.
///
/// With `form` the stand-in
/// holds, without end, a git call of the form of that keep at once in place of the `pack-objects` of release 5.28's keep:
/// the indexing (`check-hold`) or the filter (`filter-hold`), with the file its process id is written into. The held call
/// is the form's own and no call of the older keep has started. At the end nothing of the form is left in its folders, no
/// pack and no marker stands, no form line is written, and release 5.28's keep did not run behind the form that the
/// request ended. A worker that waited for the form would wait for the keep's limit, 300 s.
async fn a_request_to_end_ends_the_wait_a_keep_runs_through(
    label: &str,
    wake: bool,
    form: Option<(&str, &str)>,
) {
    let mut run = Run::start(label, BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    run.burst(1, 5);
    match form {
        Some((hold, running)) => run.hold_the_form(hold, running),
        None => run.hold_the_keep(),
    }
    // What the stand-in holds: a git call of the form, or the `pack-objects` of release 5.28's keep.
    let what = if form.is_some() {
        "the form's git call"
    } else {
        "the pack-objects"
    };
    run.hold_the_bundle();
    let from = run.bundles().len();
    run.write("a.txt", "first after the burst\n");
    assert!(
        wait_for(LONG, || run.dir.join("bundle-running").exists()).await,
        "TRIGGER PRESENT is not shown: no look reached its bundle step; log:\n{}",
        run.log()
    );
    // The look started before its bundle step was seen: its tick comes 3 s after its start, so it has passed 3 s from now.
    let tick_passed = tokio::time::Instant::now() + Duration::from_millis(BURST_EVERY_MS);
    if wake {
        run.tool_call_ends().await;
    }
    tokio::time::sleep(HELD).await;
    run.let_the_bundle_go();
    let spacing_ends = Instant::now() + Duration::from_millis(5 * BURST_EVERY_MS);
    let first = run
        .wait_copy(from, "a.txt", "first after the burst\n", LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the look whose bundle step was held stores its copy; log:\n{}",
                run.log()
            )
        });
    let pid = match form {
        Some((_, running)) => run.the_held_form(first, running).await,
        None => run.the_held_keep(first).await,
    };
    // The form's calls so far, when a call of the form is held: its indexing, its filter when that is the held
    // call, and no call of the older keep.
    let form_calls = form.map(|(hold, _)| (1, usize::from(hold == "filter-hold"), 0));
    if let Some(wanted) = form_calls {
        assert_eq!(
            run.form_calls(),
            wanted,
            "TRIGGER PRESENT is not shown: the held call of the keep at once is not the form's own; log:\n{}",
            run.log()
        );
    }
    tokio::time::sleep_until(tick_passed + Duration::from_millis(500)).await;
    assert!(
        alive(pid),
        "TRIGGER PRESENT is not shown: {what} of the keep at once is no longer held; log:\n{}",
        run.log()
    );
    assert_eq!(
        (run.stored(), run.line_of(first).is_empty()),
        (first + 1, true),
        "TRIGGER PRESENT is not shown: a look ran before the request to end, or the held look's line does not wait for its \
         keep (copies stored, the line still waits); log:\n{}",
        run.log()
    );
    let left = spacing_ends.saturating_duration_since(Instant::now());
    assert!(
        left > Duration::from_secs(6),
        "TRIGGER PRESENT is not shown: the spacing after the slow look had only {left:?} to run when the request to end was \
         to come"
    );
    println!(
        "TRIGGER PRESENT: {what} of a keep at once (process {pid}) is held, the tick of the timer has passed, and \
         the spacing of 15 s after the slow look has at least {left:?} to run (a tool call's wake was taken first: {wake})"
    );
    let last = "written right before the request to end\n";
    let from = run.bundles().len();
    run.write("a.txt", last);
    let asked = Instant::now();
    run.d.process.as_ref().unwrap().signal("TERM");
    let exit = exit_of(&mut run, LONG).await;
    let took = asked.elapsed();
    let refused = refused_or_failed(&run.log());
    let holds =
        (from..run.bundles().len()).any(|i| run.file_of(i, "a.txt").as_deref() == Some(last));
    let is_gone = gone(pid).await;
    println!(
        "the worker ended ({exit:?}) {} ms after the request, and the spacing had at least {} ms to run then; a stored copy \
         holds the work written before the request: {holds}; the held pack-objects is gone: {is_gone}; keeps started: {}\n\
         look lines:\n{}\nrefused or failed:\n{}",
        took.as_millis(),
        left.as_millis(),
        keeps_started(&run.dir),
        run.look_lines().join("\n"),
        refused.join("\n")
    );
    assert_eq!(
        exit,
        Some(Some(143)),
        "the worker ends as the signal asks; log:\n{}",
        run.log()
    );
    assert!(
        took < END_WITHIN,
        "a request to end ends the wait that a keep runs through within {} s: the worker ended {} ms after the request",
        END_WITHIN.as_secs(),
        took.as_millis()
    );
    assert!(
        took + Duration::from_secs(2) < left,
        "a request to end ends the wait that a keep runs through: the worker ended {} ms after the request, and the spacing \
         after the slow look had at least {} ms to run then: the wait for the spacing held the request",
        took.as_millis(),
        left.as_millis()
    );
    assert!(
        holds,
        "the copy that the request to end asked for holds the work written before the request"
    );
    assert!(
        refused.is_empty(),
        "no look fails because of a keep:\n{}",
        refused.join("\n")
    );
    assert!(
        is_gone,
        "the git of the keep that the request ended is gone"
    );
    if let Some(wanted) = form_calls {
        assert_eq!(
            (run.keep_folders(), run.form_lines(), run.form_calls()),
            ((vec![], vec![], vec![]), Vec::<String>::new(), wanted),
            "a form that a request to end cut in the wait for the spacing leaves nothing in its folders and no pack, \
             writes no form line, and release 5.28's keep did not run behind it; log:\n{}",
            run.log()
        );
    }
}

/// A request to end while a keep at once packs, after the tick of the timer has passed,
/// ends the wait for the spacing ([`a_request_to_end_ends_the_wait_a_keep_runs_through`]).
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_request_to_end_ends_the_wait_for_the_spacing_that_a_keep_runs_through() {
    a_request_to_end_ends_the_wait_a_keep_runs_through("b932-stop-tick", false, None).await;
}

/// A request to end while a keep at once packs in the wait that follows a taken wake of a
/// tool call ends that wait ([`a_request_to_end_ends_the_wait_a_keep_runs_through`]).
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_request_to_end_ends_the_wait_after_a_taken_wake_that_a_keep_runs_through() {
    a_request_to_end_ends_the_wait_a_keep_runs_through("b932-stop-wake", true, None).await;
}

// ----, ruling 43 (d) of the lead: rule B also follows the copy after a tool call ----------------------------------------------
//
// The first look after a burst is a tool call's when the burst ends while the copy task waits for its tick. The turns of these
// tests have a timer of 10 s ([`TOOL_EVERY_MS`]), so that no tick comes before the tool call's wake is taken and the burst falls
// before the first look, whatever the machine does. A look leaves that timer when its git work takes more than a tenth of it,
// 1 s; the stand-in holds the bundle step of the tool call's look for [`TOOL_SLOW`], and each test reads from the look's own
// line that it did (TRIGGER PRESENT).

/// The retry time of the turns whose first look of a head stores no copy and whose second look the test holds
/// ([`Run::first_look_of_a_head_stores_nothing`]): 3 s, the timer of those turns. The tick that tries the refused copy again
/// then comes 3 s after the refused look started, and the test has that long to place its hold after the bucket's refusal.
fn retry_at_the_tick() -> (&'static str, String) {
    ("AGENTD_DIRECT_SNAPSHOT_RETRY_MS", "3000".to_string())
}

/// The timer of the tests whose first look after a burst is a tool call's: 10 s. The spacing holds the
/// look after a look past its tick when the git work of that look took more than a tenth of the timer: 1 s.
const TOOL_EVERY_MS: u64 = 10_000;
/// How long the stand-in holds the bundle step of a tool call's look that is to leave the timer of
/// [`TOOL_EVERY_MS`]: 1.5 s. The spacing after such a look is nine times its git work, 13.5 s at the least.
const TOOL_SLOW: Duration = Duration::from_millis(1_500);

impl Run {
    /// A tool call's look whose bundle step the stand-in holds for `held`. `content` is written into
    /// `file`, a tool call of the engine ends, the look it asks for is held in its bundle step, and after `held` the step goes
    /// on. The answer is the index of the copy that look stored. One tool call of the engine is used.
    async fn held_look_of_a_tool_call(&self, file: &str, content: &str, held: Duration) -> usize {
        self.hold_the_bundle();
        let from = self.bundles().len();
        self.write(file, content);
        self.tool_call_ends().await;
        assert!(
            wait_for(LONG, || self.dir.join("bundle-running").exists()).await,
            "TRIGGER PRESENT is not shown: no look reached its bundle step after the tool call ended; log:\n{}",
            self.log()
        );
        tokio::time::sleep(held).await;
        self.let_the_bundle_go();
        self.wait_copy(from, file, content, LONG)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "the tool call's look whose bundle step was held stores its copy; log:\n{}",
                    self.log()
                )
            })
    }

    /// A turn with the timer of [`TOOL_EVERY_MS`], once the copy of the session's start is stored.
    async fn start_for_a_tool_calls_look(label: &str, more: Vec<(&'static str, String)>) -> Self {
        let run = Self::start(label, TOOL_EVERY_MS, more).await;
        assert!(
            wait_for(LONG, || run.stored() >= 1).await,
            "the copy of the session's start is stored; log:\n{}",
            run.log()
        );
        run
    }
}

/// The look of `line` was a tool call's and its git work took more than a tenth of the timer of
/// [`TOOL_EVERY_MS`]: the spacing holds the look after it past its tick. Says so for the record; a look that did not is a test
/// that did not show its trigger.
fn a_tool_calls_look_left_the_timer(line: &str) {
    assert_eq!(
        field(line, "by"),
        Some("tool"),
        "TRIGGER PRESENT is not shown: the held look was not the tool call's: {line}"
    );
    let git = git_ms(line);
    assert!(
        git > TOOL_EVERY_MS / 10,
        "TRIGGER PRESENT is not shown: the git work of the held look took {git} ms, no more than a tenth of the timer: {line}"
    );
    println!(
        "TRIGGER PRESENT: a tool call's look under a timer of {TOOL_EVERY_MS} ms whose git work took {git} ms (more than {} \
         ms): the spacing holds the next look past its tick: {line}",
        TOOL_EVERY_MS / 10
    );
}

/// Rule B also follows the copy after a tool call. When a burst of commits ends while the
/// copy task waits for its tick, the wake of the next tool call is taken first, and the first look after the burst is the
/// tool call's. Its git work leaves the timer, the engine's head and packs stand still while it runs, and it reads no kept
/// pack: the keep follows it at once and stands on the tool call's own line, and the bundle step of the next look reads the
/// kept pack. After the burst exactly one look runs its bundle step without a kept pack. Before, a keep followed only a look
/// of the timer: no keep followed the tool call's look, and the next look ran its bundle step without a kept pack again, as in
/// release 5.28.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_burst_whose_first_look_is_a_tool_calls_gets_its_keep_at_once() {
    let mut run = Run::start_for_a_tool_calls_look("b932-tool-first", Vec::new()).await;
    run.burst(1, 5);
    let first = run
        .held_look_of_a_tool_call("a.txt", "first after the burst\n", TOOL_SLOW)
        .await;
    let line1 = run.line_when_stored(first).await;
    a_tool_calls_look_left_the_timer(&line1);
    assert_eq!(field(&line1, "engine"), Some("moved"), "{line1}");
    let second = run
        .write_and_wait("a.txt", "second, of the same head\n", LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the look after the tool call's stores its copy; log:\n{}",
                run.log()
            )
        });
    let line2 = run.line_when_stored(second).await;
    // The bundle steps of the looks from the tool call's on: one for each copy stored since, the last ones of the turn.
    let steps = run.bundle_steps();
    let after = steps[steps.len() - (run.stored() - first)..].to_vec();
    println!(
        "line 1: {line1}\nline 2: {line2}\nkeeps started: {}; bundle steps from the tool call's look on (true: read a kept \
         pack): {after:?}",
        keeps_started(&run.dir)
    );
    assert!(
        keep_ran(&line1) && names_a_kept_pack(&line1),
        "the keep follows the tool call's look at once and stands on its own line (keep= its milliseconds, kept= the bytes \
         of the pack): {line1}; the look after it: {line2}"
    );
    assert_eq!(
        after,
        vec![false, true],
        "after the burst one look runs its bundle step without a kept pack, the tool call's, and the next look reads the \
         kept pack; lines:\n{}",
        run.look_lines().join("\n")
    );
    assert_eq!(keeps_started(&run.dir), 1, "one keep for the head");
    ends_as_asked(&mut run).await;
}

/// A tool call that ends while the slow look of a tool
/// call runs does not cost that look its keep at once. A burst of commits ends while the copy task waits for its tick, so the
/// first look after it is a tool call's; its bundle step is held, and a second tool call ends while it runs. The wake of that
/// second tool call is there when the look ends, and the copy task takes it before it turns to the keep. The keep at once
/// starts all the same, in the wait for the floor and the spacing that follows the taken wake: it stands on the first tool
/// call's own line with its milliseconds and the bytes of the pack, the look that the second tool call asked for is the tool
/// call's, and its bundle step reads the kept pack. After the burst exactly one look runs its bundle step without a kept
/// pack. A copy task that notes the wait for a keep at once only after a look of the timer leaves that keep unstarted
/// (`keep=0`); the rule of the second look follows no look of a tool call, so the look of the second tool call then runs its
/// bundle step without a kept pack again, as in release 5.28.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_tool_call_that_ends_while_a_tool_calls_slow_look_runs_does_not_cost_its_keep() {
    let mut run = Run::start_for_a_tool_calls_look("b932-tool-wake", Vec::new()).await;
    let before = run.bundle_steps().len();
    run.burst(1, 5);
    run.hold_the_bundle();
    let from = run.bundles().len();
    run.write("a.txt", "first after the burst\n");
    run.tool_call_ends().await;
    assert!(
        wait_for(LONG, || run.dir.join("bundle-running").exists()).await,
        "TRIGGER PRESENT is not shown: no look reached its bundle step after the tool call ended; log:\n{}",
        run.log()
    );
    run.tool_call_ends().await;
    assert_eq!(
        run.bundles().len(),
        from,
        "TRIGGER PRESENT is not shown: the look of the first tool call had stored its copy when the second tool call ended; \
         log:\n{}",
        run.log()
    );
    println!(
        "TRIGGER PRESENT: a second tool call ended while the bundle step of the first tool call's look was held: its wake is \
         there when the look ends"
    );
    tokio::time::sleep(TOOL_SLOW).await;
    run.let_the_bundle_go();
    let first = run
        .wait_copy(from, "a.txt", "first after the burst\n", LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the tool call's look whose bundle step was held stores its copy; log:\n{}",
                run.log()
            )
        });
    let second = run
        .write_and_wait(
            "a.txt",
            "second, for the look the second tool call asked for\n",
            LONG,
        )
        .await
        .unwrap_or_else(|| {
            panic!(
                "the look that the second tool call asked for stores its copy; log:\n{}",
                run.log()
            )
        });
    let line = run.line_when_stored(first).await;
    let next = run.line_when_stored(second).await;
    a_tool_calls_look_left_the_timer(&line);
    assert_eq!(field(&line, "engine"), Some("moved"), "{line}");
    assert_eq!(
        field(&next, "by"),
        Some("tool"),
        "TRIGGER PRESENT is not shown: the look after the held one was not the second tool call's, so the copy task did not \
         take its wake before it turned to the keep: {next}"
    );
    println!(
        "TRIGGER PRESENT: the copy task took the second tool call's wake after the first tool call's slow look, on a head and \
         packs that stood still: {line}\nthe look the second tool call asked for: {next}"
    );
    let steps = run.bundle_steps()[before..].to_vec();
    println!(
        "keeps started: {}; bundle steps after the burst (true: read a kept pack): {steps:?}",
        keeps_started(&run.dir)
    );
    assert!(
        keep_ran(&line) && names_a_kept_pack(&line),
        "a keep follows the tool call's slow look at once, also when a second tool call's wake was taken first, and stands on \
         the tool call's own line (keep= its milliseconds, kept= the bytes of the pack): {line}; the look the second tool \
         call asked for: {next}"
    );
    assert_eq!(
        steps,
        vec![false, true],
        "after the burst exactly one look runs its bundle step without a kept pack, the first tool call's, and the look of \
         the second tool call reads the kept pack; lines:\n{}",
        run.look_lines().join("\n")
    );
    assert_eq!(keeps_started(&run.dir), 1, "one keep for the head");
    ends_as_asked(&mut run).await;
}

/// Which look the tests of a look's start hold ([`the_keep_never_moves_the_start_of_the_next_look`]).
#[derive(Clone, Copy)]
enum HeldLook {
    /// The second look of a head, the timer's: its keep is the keep of the second look, and the look after it is the timer's.
    SecondOfTheTimer,
    /// The first look after a burst, the timer's, while which a tool call ends: its keep is the keep at once, which starts
    /// after the task has taken the tool call's wake, and the look after it is the tool call's (ruling 37 (b) of the lead).
    FirstWithAWake,
    /// The first look after a burst, a tool call's: its keep is the keep at once, and the look after it is the timer's
    /// (ruling 43 (d) of the lead).
    FirstOfAToolCall,
    /// [`HeldLook::SecondOfTheTimer`], with the form of the keep held in its `index-pack --stdin` through
    /// `check-hold`.
    SecondOfTheTimerWithTheFormHeld,
}

impl HeldLook {
    /// One turn of the tests of a look's start, with the keep on or off. A turn of a held look of the timer whose trigger
    /// was missing ([`next_look_after_a_held_one`]) is made again, [`TURNS_AGAIN`] times at the most, each in a folder of
    /// its own; when none of them shows the trigger, the test fails and says what each was missing.
    async fn turn(self, label: &str, keep_on: bool) -> NextLook {
        let (wake, form) = match self {
            HeldLook::SecondOfTheTimer => (false, false),
            HeldLook::FirstWithAWake => (true, false),
            HeldLook::FirstOfAToolCall => {
                return next_look_after_a_held_look_of_a_tool_call(label, keep_on).await;
            }
            HeldLook::SecondOfTheTimerWithTheFormHeld => (false, true),
        };
        let mut missing = Vec::new();
        for again in 0..=TURNS_AGAIN {
            let label = if again == 0 {
                label.to_string()
            } else {
                format!("{label}-r{again}")
            };
            let turn = if form {
                next_look_after_a_held_one_holding(&label, keep_on, wake, true).await
            } else {
                next_look_after_a_held_one(&label, keep_on, wake).await
            };
            match turn {
                Ok(next) => return next,
                Err(why) => {
                    println!("TRIGGER ABSENT in the turn {label}: {why}; the turn is made again");
                    missing.push(why);
                }
            }
        }
        panic!(
            "TRIGGER PRESENT is not shown: in none of {} turns was the held look the first look after the burst with the \
             write in its copy:\n{}",
            missing.len(),
            missing.join("\n")
        );
    }
}

/// How many times a turn of a test of a look's start is made again when its trigger was missing
/// ([`HeldLook::turn`]). The test in this file makes up to [`PENDING_TURNS`] turns to see its trigger.
const TURNS_AGAIN: u32 = 5;

/// One turn of the test of a look's start after a tool call's look, with the keep on or off
/// ([`NextLook`]). The first look after a burst is a tool call's, and its bundle step is held for [`TOOL_SLOW`], so the spacing
/// holds the look after it past its tick. With the keep on, the stand-in holds the keep at once that follows the tool call's
/// look without end; the look after it, the timer's, ends it.
async fn next_look_after_a_held_look_of_a_tool_call(label: &str, keep_on: bool) -> NextLook {
    let more = if keep_on {
        Vec::new()
    } else {
        vec![("AGENTD_DIRECT_KEEP_COMMITS", "0".to_string())]
    };
    let mut run = Run::start_for_a_tool_calls_look(label, more).await;
    run.burst(1, 3);
    run.hold_the_keep();
    run.hold_the_bundle();
    let from = run.bundles().len();
    let held_work = "written for the tool call's look whose bundle step is held\n";
    run.write("a.txt", held_work);
    run.tool_call_ends().await;
    assert!(
        wait_for(LONG, || run.dir.join("bundle-running").exists()).await,
        "TRIGGER PRESENT is not shown: no look reached its bundle step after the tool call ended; log:\n{}",
        run.log()
    );
    tokio::time::sleep(TOOL_SLOW).await;
    run.let_the_bundle_go();
    assert!(
        wait_for(LONG, || run.stored() > from).await,
        "the tool call's look whose bundle step was held stores its copy; log:\n{}",
        run.log()
    );
    let seen = Instant::now();
    let held = run
        .wait_copy(from, "a.txt", held_work, LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the copy of the held look holds its work; log:\n{}",
                run.log()
            )
        });
    if keep_on {
        run.the_held_keep(held).await;
    }
    let from = run.bundles().len();
    run.write("a.txt", "second, written while the keep's git was held\n");
    assert!(
        wait_for(LONG, || run.stored() > from).await,
        "the copy of the look after the held one is stored; log:\n{}",
        run.log()
    );
    let took = seen.elapsed();
    // The look after the held one is the timer's and finds the head of the tool call's look again. With the keep on, the keep
    // by the rule of the second look follows it: the keep at once that its start ended left that head unmarked. The stand-in
    // holds no keep from here on, so that keep goes to its end and the line of its look is written.
    let _ = std::fs::remove_file(run.dir.join("keep-hold"));
    assert!(
        wait_for(LONG, || !run.line_of(from).is_empty()
            && !run.line_of(held).is_empty())
        .await,
        "the worker's log has the lines of both looks; lines:\n{}",
        run.look_lines().join("\n")
    );
    let (before, line) = (run.line_of(held), run.line_of(from));
    a_tool_calls_look_left_the_timer(&before);
    assert_eq!(
        keeps_started(&run.dir),
        2 * usize::from(keep_on),
        "TRIGGER PRESENT is not shown: the keep is {} (on: the keep at once that the start of the next look ended, and the \
         keep by the rule of the second look after that look, which found the same head); lines:\n{}",
        if keep_on { "on" } else { "off" },
        run.look_lines().join("\n")
    );
    assert_eq!(
        (field(&before, "keep"), field(&line, "by")),
        (Some(if keep_on { "ended" } else { "0" }), Some("timer")),
        "the look after the tool call's is the timer's and its start ended the keep that was held; the held look: {before}; \
         the look after it: {line}"
    );
    let wait = (9 * git_ms(&before)).clamp(TOOL_EVERY_MS, 5 * TOOL_EVERY_MS);
    let allowed = Duration::from_millis(wait + ms_of(&line, "total")) + GIT_END;
    let spacing = (9 * git_ms(&before)).min(5 * TOOL_EVERY_MS);
    let late_ms = i64::try_from(took.as_millis()).unwrap()
        - i64::try_from(ms_of(&line, "total") + spacing).unwrap();
    ends_as_asked(&mut run).await;
    NextLook {
        took,
        allowed,
        late_ms,
        lines: (before, line),
    }
}

/// The keep at once that follows a tool call's look never moves the start of the look after
/// it. The first look after a burst is a tool call's and leaves the timer; with the keep on, the keep at once that follows it
/// is held without end. The look after it, the timer's, starts when the spacing says, as with the keep off, and its start ends
/// the keep.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_keep_at_once_after_a_tool_calls_look_never_moves_the_start_of_the_next_look() {
    the_keep_never_moves_the_start_of_the_next_look("b932-start-tool", HeldLook::FirstOfAToolCall)
        .await;
}

/// A second tool call that ends while the keep at once of a tool call's look runs does not
/// end it; the look that the second tool call starts does, once the spacing lets it start. That keep at once did not end as
/// kept: it leaves its head unmarked. The look of the second tool call finds the same head, and the rule of the second look
/// follows no look of a tool call, in release 5.28 and now: no keep follows it. The next look of the timer finds that head
/// again and is followed by the keep of release 5.28's rule, once, which keeps a pack. (That a keep at once that was cut is
/// not tried again until a keep is kept is held by `b932_a_keep_at_once_that_was_cut_is_not_tried_again_until_a_keep_is_kept`.)
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_second_tool_call_ends_the_keep_at_once_when_its_look_starts() {
    let mut run = Run::start_for_a_tool_calls_look("b932-tool-cut", Vec::new()).await;
    run.burst(1, 5);
    run.hold_the_keep();
    let first = run
        .held_look_of_a_tool_call("a.txt", "first after the burst\n", TOOL_SLOW)
        .await;
    let pid = run.the_held_keep(first).await;
    println!(
        "TRIGGER PRESENT: a keep followed the tool call's look at once and its git (process {pid}) is held"
    );
    // A second tool call ends while that keep's git is held: the look it asks for waits for the spacing.
    let work = "second, for the look the second tool call asks for\n";
    let from = run.bundles().len();
    run.write("a.txt", work);
    run.tool_call_ends().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        alive(pid) && run.stored() == first + 1,
        "a tool call that ends while a keep runs does not end it: a second after the tool call ended the keep's git still \
         runs and no look has stored a copy; lines:\n{}",
        run.look_lines().join("\n")
    );
    assert!(
        wait_for(LONG, || !alive(pid)).await,
        "the look that the second tool call started ended the keep's git; log:\n{}",
        run.log()
    );
    let _ = std::fs::remove_file(run.dir.join("keep-hold"));
    let second = run
        .wait_copy(from, "a.txt", work, LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the look that the second tool call asked for stores its copy; log:\n{}",
                run.log()
            )
        });
    let line1 = run.line_when_stored(first).await;
    let line2 = run.line_when_stored(second).await;
    a_tool_calls_look_left_the_timer(&line1);
    assert_eq!(
        field(&line1, "keep"),
        Some("ended"),
        "the line of the tool call's look whose keep at once the next look cut: {line1}"
    );
    assert_eq!(
        (
            field(&line2, "by"),
            field(&line2, "engine"),
            field(&line2, "keep"),
            keeps_started(&run.dir)
        ),
        (Some("tool"), Some("same"), Some("0"), 1),
        "the look that ended the keep was the second tool call's and found the same head; the rule of the second look \
         follows no look of a tool call: {line2}"
    );
    let third = run
        .write_and_wait("a.txt", "third, of the timer\n", LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the next look of the timer stores its copy; log:\n{}",
                run.log()
            )
        });
    let line3 = run.line_when_stored(third).await;
    assert!(
        (field(&line3, "by"), field(&line3, "engine")) == (Some("timer"), Some("same"))
            && keep_ran(&line3)
            && names_a_kept_pack(&line3),
        "the next look of the timer of that head is followed by a keep, as in release 5.28, and it keeps a pack: {line3}"
    );
    assert_eq!(
        keeps_started(&run.dir),
        2,
        "the keep at once and the one keep by the rule of the second look"
    );
    ends_as_asked(&mut run).await;
}

/// (ruling 43 (d) of the lead), a pin: a tool call's look that the spacing does not hold past its tick is followed by no
/// keep at once. The timer is ten minutes, so no look of this test leaves it. The tool call's look finds the engine's new head
/// after a burst and is followed by no keep, as in release 5.28.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_tool_calls_look_that_stays_on_the_timer_gets_no_keep_at_once() {
    // Under a timer of ten minutes no look runs before the first tool call ends: the tool call's look is the turn's first.
    let mut run = Run::start("b932-tool-cheap", NO_TICK_MS, Vec::new()).await;
    run.burst(1, 5);
    let from = run.bundles().len();
    let work = "found by the look of a tool call\n";
    run.write("a.txt", work);
    run.tool_call_ends().await;
    let first = run
        .wait_copy(from, "a.txt", work, LONG)
        .await
        .unwrap_or_else(|| panic!("the tool call's look stores its copy; log:\n{}", run.log()));
    let line = run.line_when_stored(first).await;
    assert_eq!(
        (field(&line, "by"), field(&line, "engine")),
        (Some("tool"), Some("moved")),
        "TRIGGER PRESENT is not shown: the look was not a tool call's on the engine's new head: {line}"
    );
    println!(
        "TRIGGER PRESENT: a tool call's look under a timer of ten minutes found a moved head; its git work took {} ms, which \
         holds no look past a tick: {line}",
        git_ms(&line)
    );
    assert_eq!(
        (field(&line, "keep"), keeps_started(&run.dir)),
        (Some("0"), 0),
        "a tool call's look that stays on the timer is followed by no keep at once: {line}"
    );
    ends_as_asked(&mut run).await;
}

/// The line of a later look ends the wait for a keep at
/// once. The first look after a burst is followed by its keep at once: its line waited for that keep. Every look that stores
/// a copy says anew whether its line waits for a keep at once. So when a later look of the timer is followed by a keep by
/// the rule of the second look, and a tool call's wake is pending when that look ends, the wake is taken and that keep is
/// not started, as (`b860_kept_a_wake_that_is_pending_starts_the_next_look_as_before`). A mark "a keep at once
/// waits" that outlived the look it was set for would start that keep in the wait that follows the taken wake.
///
/// That a keep was due then is shown afterwards: the next look of the timer of the same head, with no wake pending, is
/// followed by one.
#[tokio::test(flavor = "multi_thread")]
async fn b932_a_later_looks_line_ends_the_wait_for_a_keep_at_once() {
    let mut run = Run::start("b932-waits-reset", BURST_EVERY_MS, Vec::new()).await;
    run.behind_a_look("a.txt").await;
    run.burst(1, 5);
    let first = run
        .held_look("a.txt", "first after the burst\n", SLOW, |_| {})
        .await;
    let line = run.line_when_stored(first).await;
    left_the_timer(&line);
    assert!(
        keep_ran(&line) && names_a_kept_pack(&line),
        "TRIGGER PRESENT is not shown: no keep followed the first look after the burst at once: {line}"
    );
    println!(
        "TRIGGER PRESENT: the line of the first look after the burst waited for its keep at once: {line}"
    );

    // The head moves under the kept pack. The first look of the new head reads the kept pack and is followed by no keep.
    run.commit(100);
    let second = run
        .write_and_wait("a.txt", "first of the new head\n", LONG)
        .await
        .unwrap();
    let line = run.line_when_stored(second).await;
    assert_eq!(
        (field(&line, "engine"), field(&line, "keep")),
        (Some("moved"), Some("0")),
        "TRIGGER PRESENT is not shown: the look after the commit did not find a new head, or a keep followed it: {line}"
    );

    // The second look of the new head, the timer's: a keep by the rule of the second look is due after it. A tool call ends
    // while its bundle step is held, so its wake is pending when the look ends.
    run.hold_the_bundle();
    let from = run.bundles().len();
    run.write("a.txt", "second of the new head\n");
    assert!(
        wait_for(LONG, || run.dir.join("bundle-running").exists()).await,
        "TRIGGER PRESENT is not shown: no look reached its bundle step after the write; log:\n{}",
        run.log()
    );
    run.tool_call_ends().await;
    let third = "third, written after the tool call ended\n";
    run.write("a.txt", third);
    tokio::time::sleep(SLOW).await;
    run.let_the_bundle_go();
    let held = run
        .wait_copy(from, "a.txt", "second of the new head\n", LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the look whose bundle step was held stores its copy; log:\n{}",
                run.log()
            )
        });
    let found = run
        .wait_copy(held + 1, "a.txt", third, LONG)
        .await
        .unwrap_or_else(|| {
            panic!(
                "the copy that the pending wake asked for is stored; log:\n{}",
                run.log()
            )
        });
    let held_line = run.line_when_stored(held).await;
    let next = run.line_when_stored(found).await;
    assert_eq!(
        (field(&held_line, "by"), field(&held_line, "engine")),
        (Some("timer"), Some("same")),
        "TRIGGER PRESENT is not shown: the held look was not the timer's second look of the new head: {held_line}"
    );
    assert_eq!(
        field(&next, "by"),
        Some("tool"),
        "TRIGGER PRESENT is not shown: the look after the held one was not the tool call's, so the copy task did not take \
         the wake before it turned to the keep: {next}"
    );
    println!(
        "TRIGGER PRESENT: a keep by the rule of the second look was due after a look of the timer, and a tool call's wake \
         was pending when that look ended: {held_line}\nthe look the tool call asked for: {next}"
    );
    assert_eq!(
        (field(&held_line, "keep"), keeps_started(&run.dir)),
        (Some("0"), 1),
        "a keep by the rule of the second look that is not in flight when a wake is taken is not started, also after the \
         line of an earlier look waited for a keep at once: {held_line}"
    );

    // That a keep was due after the held look: the next look of the timer of the same head is followed by one.
    let again = run
        .write_and_wait("a.txt", "fourth, of the timer\n", LONG)
        .await
        .unwrap();
    let line = run.line_when_stored(again).await;
    assert!(
        (field(&line, "by"), field(&line, "engine")) == (Some("timer"), Some("same"))
            && keep_ran(&line)
            && names_a_kept_pack(&line),
        "TRIGGER PRESENT is not shown: no keep follows a look of the timer of this head, so none was due when the wake was \
         pending: {line}"
    );
    assert_eq!(keeps_started(&run.dir), 2);
    ends_as_asked(&mut run).await;
}

// ----: the form in whole turns (tests 11 and 12 of the build brief; the wiring of row d) ----------------------------------------

/// The lines `direct turn copy keep form: ` of a worker's log, each without what stands before those words.
fn form_lines(log: &[String]) -> Vec<String> {
    log.iter()
        .filter_map(|line| {
            line.split_once("direct turn copy keep form: ")
                .map(|(_, rest)| rest.to_string())
        })
        .collect()
}

/// The first lines of the markers of the kept packs in the private repository of a worker's copies.
fn kept_marker_lines(private: &Path) -> Vec<String> {
    let folder = private.join("kept/pack");
    names(&folder)
        .into_iter()
        .filter(|name| name.ends_with(".keep"))
        .map(|name| {
            std::fs::read_to_string(folder.join(name))
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

/// The engine of the in-process turn `d` commits once, and three looks of the timer each find a changed tree: the looks find
/// the engine's head twice, and a keep follows. `session` is the number of the session whose copies are counted. The answer
/// is the engine's head.
async fn a_commit_and_three_looks(d: &Direct, session: u32) -> String {
    let stored = || {
        d.aws
            .versions_of(&keys::snapshot_json_key_t(RUN, session))
            .len()
    };
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
            "a look of the timer stores the copy of {content:?}; log:\n{}",
            d.log.lines().join("\n")
        );
    }
    git(&wt, &["rev-parse", "HEAD"])
}

/// Test 12 of the build brief: the form's line in a whole turn. The engine commits, the looks of the timer find its
/// head twice, and the form keeps the pack: the worker's log has `direct turn copy keep form: made index=<n> filter=<n>`
/// with two numbers of at least 1, before the save line of that look, whose `keep=` is a number, whose `kept=` is the bytes
/// of the kept pack, and which still ends `keep=<what> bytes=<n> kept=<n>`. The marker of the pack reads
/// `agentd kept bundle <head>`.
#[tokio::test(flavor = "multi_thread")]
async fn b936_the_forms_line_stands_before_the_save_line_of_its_look() {
    let dir = scratch("b936-form-line");
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
    let head = a_commit_and_three_looks(&d, 1).await;
    let kept_look = |lines: &[String]| -> Option<usize> {
        lines.iter().position(|l| {
            l.contains("direct turn copy look n=")
                && field(l, "keep").is_some_and(|v| v.parse::<u64>().is_ok_and(|ms| ms > 0))
                && field(l, "kept").is_some_and(|v| v.parse::<u64>().is_ok_and(|bytes| bytes > 0))
        })
    };
    assert!(
        wait_for(LONG, || kept_look(&d.log.lines()).is_some()).await,
        "TRIGGER PRESENT is not shown: no pack was kept in a turn whose engine committed and whose looks found its head \
         twice; log:\n{}",
        d.log.lines().join("\n")
    );
    let log = d.log.lines();
    let private = d.agent_data().join("snapshot.git");
    assert_eq!(
        kept_marker_lines(&private),
        vec![format!("agentd kept bundle {head}")],
        "TRIGGER PRESENT is not shown: the pack was not made by the form"
    );
    let save_at = kept_look(&log).unwrap();
    let form_at = log
        .iter()
        .position(|l| l.contains("direct turn copy keep form: "));
    let forms = form_lines(&log);
    assert_eq!(
        forms.len(),
        1,
        "TRIGGER PRESENT: one keep was due, and the form has one line for it; log:\n{}",
        log.join("\n")
    );
    let numbers: Vec<Option<u64>> = ["index", "filter"]
        .iter()
        .map(|name| field(&forms[0], name).and_then(|v| v.parse().ok()))
        .collect();
    assert!(
        forms[0].starts_with("made index=")
            && forms[0].split_whitespace().count() == 3
            && numbers.iter().all(|n| n.is_some_and(|ms| ms >= 1)),
        "the form's line names the time of its two git calls, each at least 1: {:?}",
        forms[0]
    );
    assert!(
        form_at.is_some_and(|at| at < save_at),
        "the form's line stands before the save line of its look; log:\n{}",
        log.join("\n")
    );
    let save = &log[save_at];
    let pack_bytes: u64 = names(&private.join("kept/pack"))
        .iter()
        .filter(|name| name.ends_with(".pack"))
        .map(|name| {
            std::fs::metadata(private.join("kept/pack").join(name))
                .unwrap()
                .len()
        })
        .sum();
    assert_eq!(
        field(save, "kept").and_then(|v| v.parse::<u64>().ok()),
        Some(pack_bytes),
        "the save line's kept= is the bytes of the kept pack: {save}"
    );
    let last: Vec<&str> = save.split_whitespace().rev().take(3).collect();
    assert!(
        last.len() == 3
            && last[2].starts_with("keep=")
            && last[1].starts_with("bytes=")
            && last[0].starts_with("kept="),
        "the save line still ends keep=<what> bytes=<n> kept=<n>: {save}"
    );
    std::fs::write(&steps, "1000000\n").unwrap();
    let resp = the_turns_result(&d).await;
    assert_eq!(
        resp.outcome,
        harness_protocol::runtime::TurnOutcome::Completed,
        "{resp:?}"
    );
}

/// Row d of "where the form is off" in a whole turn. A
/// second session of a run restores the copy the first session saved. Its engine commits, the looks of the timer find its
/// head twice, and the keep is release 5.28's alone: the worker's log has
/// `direct turn copy keep form: off: the session was restored from a saved copy`, no `made` line and no `gave way` line,
/// and the marker of the kept pack reads `agentd kept <head>`. The copy task tells the snapshotter that the session is
/// restored; before this test only the setter was tested.
#[tokio::test(flavor = "multi_thread")]
async fn b936_a_session_restored_from_a_saved_copy_keeps_by_the_older_keep_alone() {
    use agentd::direct::snapshot::BundleForm;
    let d1 = Direct::start(b860_thin_support::base_form(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    }))
    .await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(
        resp.outcome,
        harness_protocol::runtime::TurnOutcome::Completed,
        "{resp:?}"
    );
    let s = d1.summary();
    let (saved_head, base) = (
        s.git.head_sha.clone().unwrap(),
        s.git.base_sha.clone().unwrap(),
    );
    let own = d1.aws.get(&keys::bundle_key(RUN)).unwrap();

    let dir = scratch("b936-restored-turn");
    let steps = dir.join("engine-steps");
    std::fs::write(&steps, "0\n").unwrap();
    let d2 = b860_thin_support::later_session(
        Opts {
            scenario: "four_edits",
            engine_env: vec![("FAKE_ACP_STEP_FILE", steps.display().to_string())],
            edit_cfg: Some(Box::new(|cfg| {
                cfg.direct.copy_form = BundleForm::AboveFloor;
                cfg.direct.snapshot_interval = Some(Duration::from_millis(500));
                cfg.direct.snapshot_floor = Some(Duration::from_millis(50));
            })),
            ..Opts::default()
        },
        &d1,
        &keys::bundle_key_t(RUN, 1),
        own,
        &saved_head,
        &base,
    )
    .await;
    let (code, text) = d2.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(LONG, || !d2.prompts_received().is_empty()).await,
        "the engine of the restored session got its prompt"
    );
    let head = a_commit_and_three_looks(&d2, 2).await;
    assert!(
        wait_for(LONG, || !form_lines(&d2.log.lines()).is_empty()).await,
        "TRIGGER PRESENT is not shown: no keep was due in the restored session; log:\n{}",
        d2.log.lines().join("\n")
    );
    let log = d2.log.lines();
    assert_eq!(
        form_lines(&log),
        vec!["off: the session was restored from a saved copy".to_string()],
        "TRIGGER PRESENT: in a session restored from a saved copy the form is off; log:\n{}",
        log.join("\n")
    );
    assert!(
        wait_for(LONG, || !kept_marker_lines(
            &d2.agent_data().join("snapshot.git")
        )
        .is_empty())
        .await,
        "release 5.28's keep keeps the pack; log:\n{}",
        d2.log.lines().join("\n")
    );
    assert_eq!(
        kept_marker_lines(&d2.agent_data().join("snapshot.git")),
        vec![format!("agentd kept {head}")],
        "the pack is release 5.28's keep's"
    );
    std::fs::write(&steps, "1000000\n").unwrap();
    let resp = the_turns_result(&d2).await;
    assert_eq!(
        resp.outcome,
        harness_protocol::runtime::TurnOutcome::Completed,
        "{resp:?}"
    );
}

impl Run {
    /// The stand-in holds a git call of the form from now on: `check-hold` holds its `index-pack --stdin` (and writes
    /// `check-running`), `filter-hold` holds its filter call (and writes `filter-running`).
    fn hold_the_form(&self, hold: &str, running: &str) {
        let _ = std::fs::remove_file(self.dir.join(running));
        std::fs::write(self.dir.join(hold), "").unwrap();
    }

    /// Waits for the held git call of the form of the keep that follows the stored copy with index `index`, until
    /// the call's process id stands whole in `running`. The copy of that look was stored before the form's git started. The
    /// answer is the process id of the held call.
    async fn the_held_form(&self, index: usize, running: &str) -> u32 {
        let mut pid: Option<u32> = None;
        assert!(
            wait_for(LONG, || {
                pid = std::fs::read_to_string(self.dir.join(running))
                    .ok()
                    .and_then(|text| text.strip_suffix('\n').and_then(|n| n.parse().ok()));
                pid.is_some()
            })
            .await,
            "TRIGGER PRESENT is not shown: no git call of the form was held ({running}); log:\n{}",
            self.log()
        );
        assert_eq!(
            (self.bundles().len(), self.stored()),
            (index + 1, index + 1),
            "TRIGGER PRESENT: the form's git started before the copy of its look was stored"
        );
        pid.unwrap_or_default()
    }

    /// What the keeps of the turn have started so far, by the stand-in's log of every git call (written before a
    /// call is held): `index-pack --stdin` calls, filter calls and calls of the older keep.
    fn form_calls(&self) -> (usize, usize, usize) {
        let calls = git_calls(&self.dir);
        (
            calls
                .iter()
                .filter(|call| {
                    b860_kept_support::command_of(call) == "index-pack"
                        && call.split_whitespace().any(|word| word == "--stdin")
                })
                .count(),
            b860_kept_support::filters_in(&calls),
            b860_kept_support::older_keeps_in(&calls),
        )
    }

    /// What is left of a keep in the private repository: the names in the form's folder, in the folder a keep writes
    /// into and in the folder of the kept pack.
    fn keep_folders(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let kept = self.private().join("kept");
        (
            names(&kept.join("indexed/pack")),
            names(&kept.join("incoming")),
            names(&kept.join("pack")),
        )
    }

    /// The lines `direct turn copy keep form: ` of the worker's log.
    fn form_lines(&self) -> Vec<String> {
        let lines: Vec<String> = self.log().lines().map(str::to_string).collect();
        form_lines(&lines)
    }
}

/// Test 11 of the build brief in a whole turn (rulings 42 (c)(i) and 44 (f) of the lead): a tool call that ends while
/// a git call of the form is held ends the form. The copy its wake asks for holds what was written then, the held git is
/// gone, nothing is left in the form's folders or in the folder a keep writes into, no pack and no marker stands, the line
/// of the look reads `keep=ended`, and the keep that was cut writes no form line. The head is untried: after the next look
/// of the timer on the same head the form keeps the pack, and the log then has its one `made` line.
async fn a_tool_call_ends_a_form_that_is_held(label: &str, hold: &str, running: &str) {
    let mut run = Run::start(label, NO_TICK_MS, Vec::new()).await;
    run.hold_the_form(hold, running);
    let index = run.to_the_look_a_keep_follows(1, "b.txt").await;
    let head = git(&run.d.worktree(), &["rev-parse", "HEAD"]);
    let pid = run.the_held_form(index, running).await;
    let filters = usize::from(hold == "filter-hold");
    assert_eq!(
        run.form_calls(),
        (1, filters, 0),
        "TRIGGER PRESENT is not shown: {hold}: the held call is the form's own, before any call of the older keep"
    );
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
        "TRIGGER PRESENT: {hold}: no copy holds the work of the moment the tool call ended (the timer is ten minutes): the \
         form held the copy task; log:\n{}",
        run.log()
    );
    assert!(
        gone(pid).await,
        "TRIGGER PRESENT: {hold}: the form's git was not ended when a tool call ended"
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
    assert_eq!(
        run.keep_folders(),
        (vec![], vec![], vec![]),
        "{hold}: a form that was cut leaves nothing in its folders, and no pack and no marker stands"
    );
    assert_eq!(
        (run.form_lines(), run.form_calls()),
        (Vec::<String>::new(), (1, filters, 0)),
        "TRIGGER PRESENT: {hold}: a keep that was cut writes no form line, and release 5.28's keep did not run behind it"
    );
    std::fs::remove_file(run.dir.join(hold)).unwrap();

    // The bucket refuses the copy of the fourth tool call's look; the tick that tries again is the timer's, at the same head.
    let key = keys::snapshot_bundle_key(RUN);
    let attempts = run.d.aws.put_attempts(&key);
    let from = run.bundles().len();
    run.d.aws.fail_puts("snapshot.bundle", 4);
    let again = "in the copy of the tick after the tool call cut the form\n";
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
        "TRIGGER PRESENT: {hold}: the keep whose form a tool call cut was not made after a later look by the timer; \
         lines:\n{}",
        run.look_lines().join("\n")
    );
    let forms = run.form_lines();
    assert!(
        forms.len() == 1 && forms[0].starts_with("made index="),
        "{hold}: the keep that was tried again is the form's, and it has the one form line of the turn: {forms:?}"
    );
    assert_eq!(
        (kept_marker_lines(&run.private()), run.form_calls()),
        (
            vec![format!("agentd kept bundle {head}")],
            (2, 1 + filters, 0)
        ),
        "{hold}: the form made the pack at its second try, and no call of the older keep ran"
    );

    run.d.process.as_ref().unwrap().signal("TERM");
    assert_eq!(
        exit_of(&mut run, LONG).await,
        Some(Some(143)),
        "log:\n{}",
        run.log()
    );
}

/// Test 11: a tool call ends a form that is held in its indexing ([`a_tool_call_ends_a_form_that_is_held`]).
#[tokio::test(flavor = "multi_thread")]
async fn b936_a_tool_call_ends_a_form_that_is_held_in_its_indexing() {
    a_tool_call_ends_a_form_that_is_held("b936-tool-index", "check-hold", "check-running").await;
}

/// Test 11: a tool call ends a form that is held in its filter by the filter's own hold file
/// ([`a_tool_call_ends_a_form_that_is_held`]).
#[tokio::test(flavor = "multi_thread")]
async fn b936_a_tool_call_ends_a_form_that_is_held_in_its_filter() {
    a_tool_call_ends_a_form_that_is_held("b936-tool-filter", "filter-hold", "filter-running").await;
}

/// Test 11 of the build brief in a whole turn: a request to end is not held by the form. While a git call of the
/// form is held without end, the worker is asked to end: the form is ended, the copy of the stop, which holds the file
/// written right before the signal, is stored inside the time a stop has (two minutes here, as in
/// `b860_kept_a_stop_is_not_held_by_the_keep`), the held git is gone, the worker ends as the signal asks, no look failed,
/// nothing of the form is left, and no form line is written.
async fn a_request_to_end_ends_a_form_that_is_held(label: &str, hold: &str, running: &str) {
    let budget = ("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", "120000".to_string());
    let mut run = Run::start(label, NO_TICK_MS, vec![budget]).await;
    run.hold_the_form(hold, running);
    let index = run.to_the_look_a_keep_follows(1, "a.txt").await;
    let pid = run.the_held_form(index, running).await;
    assert_eq!(
        run.form_calls(),
        (1, usize::from(hold == "filter-hold"), 0),
        "TRIGGER PRESENT is not shown: {hold}: the held call is the form's own"
    );
    let late = "written right before the signal, while the form's git ran\n";
    let from = run.bundles().len();
    run.write("late.txt", late);
    let asked = Instant::now();
    run.d.process.as_ref().unwrap().signal("TERM");
    let found = run
        .wait_copy(from, "late.txt", late, Duration::from_secs(100))
        .await;
    assert!(
        found.is_some(),
        "TRIGGER PRESENT: {hold}: no copy with the file written before the signal was stored within 100 s of the request \
         to end (a stop has 120 s here): the form held the stop; log:\n{}",
        run.log()
    );
    println!(
        "{hold}: the copy of the stop was stored {:?} after the signal",
        asked.elapsed()
    );
    assert!(
        gone(pid).await,
        "TRIGGER PRESENT: {hold}: the form's git was not ended by the request to end"
    );
    assert_eq!(
        exit_of(&mut run, LONG).await,
        Some(Some(143)),
        "log:\n{}",
        run.log()
    );
    let line = run.line_of(found.unwrap());
    assert_eq!(
        field(&line, "by"),
        Some("stop"),
        "the look of a stop: {line}"
    );
    // The one look that failed is the one whose copy the bucket refused on purpose, to bring the timer's look about.
    let refused: Vec<String> = refused_or_failed(&run.log())
        .into_iter()
        .filter(|line| !line.contains(" failed why=store "))
        .collect();
    assert!(
        refused.is_empty(),
        "no look fails because of the form:\n{}",
        refused.join("\n")
    );
    assert_eq!(
        (run.keep_folders(), run.form_lines()),
        ((vec![], vec![], vec![]), Vec::<String>::new()),
        "{hold}: a form that a request to end cut leaves nothing and writes no form line"
    );
}

/// Test 11: a request to end ends a form that is held in its indexing
/// ([`a_request_to_end_ends_a_form_that_is_held`]).
#[tokio::test(flavor = "multi_thread")]
async fn b936_a_request_to_end_ends_a_form_that_is_held_in_its_indexing() {
    a_request_to_end_ends_a_form_that_is_held("b936-stop-index", "check-hold", "check-running")
        .await;
}

/// Test 11: a request to end ends a form that is held in its filter by the filter's own hold file
/// ([`a_request_to_end_ends_a_form_that_is_held`]).
#[tokio::test(flavor = "multi_thread")]
async fn b936_a_request_to_end_ends_a_form_that_is_held_in_its_filter() {
    a_request_to_end_ends_a_form_that_is_held("b936-stop-filter", "filter-hold", "filter-running")
        .await;
}

/// Test 11 of the build brief in the cell of [`END_WITHIN`] (ruling 69 (b) of the lead): a request to end ends the
/// wait for the spacing that a keep at once runs through while the form's indexing is held without end
/// ([`a_request_to_end_ends_the_wait_a_keep_runs_through`]). The worker ends within 8 s of the request and before the
/// spacing ends, the copy that the request asked for holds the work written right before it, the form's git is gone, and
/// nothing of the form is left.
#[tokio::test(flavor = "multi_thread")]
async fn b936_a_request_to_end_ends_the_wait_for_the_spacing_with_the_forms_indexing_held() {
    a_request_to_end_ends_the_wait_a_keep_runs_through(
        "b936-stop-tick-index",
        false,
        Some(("check-hold", "check-running")),
    )
    .await;
}

/// Test 11 of the build brief in the cell of [`END_WITHIN`]: a request to end ends the wait for the spacing that a
/// keep at once runs through while the form's filter is held without end by the filter's own hold file
/// ([`a_request_to_end_ends_the_wait_a_keep_runs_through`]).
#[tokio::test(flavor = "multi_thread")]
async fn b936_a_request_to_end_ends_the_wait_for_the_spacing_with_the_forms_filter_held() {
    a_request_to_end_ends_the_wait_a_keep_runs_through(
        "b936-stop-tick-filter",
        false,
        Some(("filter-hold", "filter-running")),
    )
    .await;
}

/// Test 11 of the build brief: a form in flight never moves the start of a look
/// ([`the_keep_never_moves_the_start_of_the_next_look`]). The keep of the second look is the form's, and the stand-in holds
/// its `index-pack --stdin` without end: the look after the held one starts when the spacing says, as in a turn with the
/// keep off (`AGENTD_DIRECT_KEEP_COMMITS=0`; a whole turn cannot switch the form alone off), and its start ends the form.
#[tokio::test(flavor = "multi_thread")]
async fn b936_a_form_held_in_its_indexing_never_moves_the_start_of_a_look() {
    the_keep_never_moves_the_start_of_the_next_look(
        "b936-start-form",
        HeldLook::SecondOfTheTimerWithTheFormHeld,
    )
    .await;
}
