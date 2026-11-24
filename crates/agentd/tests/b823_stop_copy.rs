//! The copies of the work when something stops answering, and when agentd is asked to end.
//!
//! A session that was stopped from outside lived on for about a minute: it posted its events every two seconds and stored no copy, wrote no
//! log line and told the run nothing, although its timer was due. One look of the copy task that does not return (a work tree that stops
//! answering, a store that takes the upload and never answers) ended every later copy, because the task waited for it without a limit.
//!
//! These tests hold five things. A look whose upload gets no answer is said and given up, and the timer goes on. A git that hangs on the
//! work tree is said and ended, and the next look copies. A termination signal saves the newest work at once, also while a look is in
//! flight. The first process of its machine, which is what a session's microVM runs, runs on after that copy: a request to end cuts none
//! of its looks short, and a copy of a stop that failed is tried again soon. The time a copy names is before its tree was read, never
//! the time it was stored.
//!
//! A look that hangs in `git add` is held in two ways. The worker says a look late after the late limit, or after three times
//! what the last look that stored a copy took when that is longer, counted from the start of the look.
//!
//! WHEN a look is said late is held on a clock that the test drives (the tests `b893_...`). From outside the worker a test cannot
//! know when a look began or what the last stored look took, and a crowded machine gives no bound on how late a task that is ready
//! gets a processor: a bound on the wall clock that a test reads from outside is either loose or fails a worker that is right, and a
//! number the worker states itself is no bound, because a worker that states a wrong number passes. So those tests serve the worker
//! inside their own runtime, pause that runtime's clock and move it themselves: the worker reads it for what a stored look took, for
//! the start and the late timer of a look, and for the limits that give a look up. They impose what the last stored look took, hold
//! the next look in `git add`, and ask on that clock: no late line one millisecond before the limit, one line while the clock
//! stands at the limit, which states the imposed and the driven numbers, no second line and no second notice while the look stays
//! held, up to a minute past the limit, each of the two orders of late and given up, and what the worker has told the run one
//! millisecond before the limit (nothing) and at the limit (the late notice). The limit that follows from each imposed duration
//! is written out in the tests (`Imposed`: 100 ms with 400 ms, 200 ms with 600 ms, 137 ms with 411 ms, 1,000 ms with 3,000 ms,
//! 1,500 ms with 4,500 ms; and `Scene` for what else can stand before the look that is held) and is never asked of the worker's
//! own rule, which would move the tests' moment with a rule that is wrong.
//!
//! What those tests hold, and what they do not. Lateness on the runtime's clock is held to the millisecond: a late timer that is
//! armed a millisecond early or late fails them. Real time that passes inside blocking work between the late timer and the late
//! line is not held, because no observer outside the worker can tell it from a slow processor. The worker has one such step: the
//! question to the file system of the work tree, which the product bounds (`TREE_ANSWER_LIMIT`). The worker asks it every
//! period from the start of a look while the look's git works, whatever the late limit and whether or not the look was asked
//! to end, so it can be under way when the late timer fires in any look that is late after more than one period; at a
//! tick that comes with the late limit the late line comes first, by the order of the arms of `watched_look`. Where a test
//! runs the clock to the limit (`DrivenClock::runs_to`), a question that the test does not hold has been answered before the
//! clock reaches the limit. So
//! where a test waits in real time for the late line, with the clock standing at the limit, that wait is the test's patience and
//! no allowance of the rule (`HeldLook::is_said_late_at`). What can be held about the step is held by two tests that ask the
//! worker to end and hold the question themselves (`said_late_when_the_tree_has_answered`, `TreeQuestionHeld`): a late line
//! that is not written when the question waits is not written while it waits, and when the tree has answered the line is there
//! and states the driven numbers exactly, with the clock still standing at the limit. When the worker asks, and the order in
//! which it takes the end of a look, its late line, a request to end and the question, are held by the tests
//! (`tests/b923_tree_question.rs`).
//!
//! THE RULE, CASE BY CASE. When a look is said late, as the worker's code has it (`watched_look`, `start_snapshots` and
//! `Copies::late` in `src/direct/mod.rs`, `late_after` in `src/direct/snapshot.rs`), and the test that drives each case with its
//! numbers written out. The limits below are for a set limit of 400 ms.
//!
//! | Case | What the worker does | Driven by |
//! |---|---|---|
//! | The rule | the set limit, or three times what the last look that stored a copy took when that is longer | stored looks of 100, 200, 137, 1,000 and 1,500 ms: limits of 400, 600, 411, 3,000 and 4,500 ms |
//! | The first look of a worker | no look stored a copy: the set limit, and the line says so | `b893_the_first_look_of_a_worker_...` (two), `b893_the_run_is_told_..._of_the_first_look_of_a_worker` (two) |
//! | After a look that found the tree unchanged | that look does not count: only a look that stored a copy does | `b893_a_look_that_stored_nothing_does_not_count_...`: 200 ms stored, then unchanged: 600 ms, nothing at 400 ms |
//! | After a look that was given up | it does not count and clears nothing; the line is written, and the run is told nothing more | `b893_a_look_after_a_look_that_was_given_up_...`: 600 ms, nothing at 400 ms |
//! | More than one stored look | the last one counts, not the first and not the longest | `b893_after_stored_looks_of_...`: 100 then 200 ms: 600 ms, nothing at 400 ms; 200 then 100 ms: 400 ms |
//! | What a stored look took | from the start of that look to its return, git and store together | the same two tests; every imposed duration is part git and part store |
//! | A stored look that was itself said late | it counts in full as the last stored look | `b893_a_late_look_that_stored_a_copy_...`: the first look is said late at 400 ms and stores its copy at 500 ms; the look after it: 1,500 ms, nothing at 400 ms and at 1,499 ms |
//! | From when a look is late, after the tick of the timer | from its own start; the end of the look before is no part of it | `b893_a_look_that_begins_150_ms_after_...`: late 600 ms after its start, nothing 600 ms after the stored look ended |
//! | From when a look is late, after a wait for the spacing | from its own start; the wait after the tick is no part of it | `b893_a_look_that_waits_for_its_spacing_...`: the tick, then a wait of up to 10 s: no late line 12 s after the tick while the look waits in `git add`, and one line stating 12 s by 22 s after it |
//! | Once per look | no second line and no second notice while the look stays held | the ten tests that run `said_late_at_its_limit_and_given_up_afterwards`: one, two and three limits past the limit, and a minute past it; `b893_a_look_after_a_look_that_was_given_up_...` and `b893_a_look_that_is_late_while_it_stores_...` have no such stops: they look once more, two periods of the flusher (200 ms) past the limit |
//! | A look that returns before its limit | it is never said late | the tests named `..._is_never_said_late` |
//! | What the line states | the time taken, the limit applied, the set limit, the last stored look or that there was none | every test named `..._is_said_late_...` |
//! | The part a look is late in | storing the copy when its store has not ended; else reading the work tree | `b893_a_look_that_is_late_while_it_stores_...`: 200 ms stored, the next look's upload held: nothing at 599 ms, at 600 ms the line of the store and the run told so; every other test that drives the clock: the tree; the real-time test `b823_a_look_whose_upload_gets_no_answer_...` is a look that is late in its store and takes either notice |
//! | What the run is told | the late notice with the line, once until a copy is stored and not after a failure was said | `b893_the_run_is_told_...`; `b893_a_look_after_a_look_that_was_given_up_...`; `b893_a_late_look_that_stored_a_copy_...`: late at 400 ms, stored, late again: told twice; `b893_two_late_looks_with_no_stored_copy_between_them_...`: late at 600 ms, found the tree unchanged, the next look late at 600 ms: told once |
//! | The question to the tree and the line | the tree is asked every period from the start of a look, asked to end or not; a look whose question is under way when its late timer fires waits for the tree's answer before the line | `b893_a_late_line_...` (two): a look that was asked to end; in `tests/b923_tree_question.rs`, `b923_a_look_that_was_not_asked_to_end_asks_its_tree_at_1500_ms_...`: a look that was not: asked at 1,500 ms, no late line at 3,000 ms while the question is held |
//! | The look's end and its late timer at one moment | the end of the look comes first, by the order of the arms of `watched_look`: the look is not said late | in `tests/b923_tree_question.rs`, `b923_a_look_that_ends_at_its_late_limit_and_a_tick_of_5000_ms_...`: a look that fails 5,000 ms after it began, the moment of its late limit: no late line |
//! | The look that follows a look which a request to end overtook | it makes its own limit from the last look that stored a copy, as every look does | in `tests/b923_tree_question.rs`: `b923_the_look_after_a_look_of_100_ms_...`: 200 ms stored, then a look of 100 ms that the request overtook: 400 ms, nothing at 399 ms; `b923_the_look_after_a_first_look_of_200_ms_...`: 600 ms, nothing at 599 ms; `b923_the_look_after_a_look_that_a_request_to_end_overtook_and_its_git_limit_gave_up_...`: the overtaken look failed, 200 ms stored before it: 600 ms, nothing at 599 ms |
//! | What that follow-up look took | it is measured from its own start, and when it stores a copy it is the last stored look | no test: no driven scene asks the worker to end while a look runs and then holds a look after the follow-up look; it belongs with the row above to |
//! | A look that a request to end started | the rule as for any look | in `tests/b923_tree_question.rs`, `b923_a_look_that_ends_at_its_late_limit_and_a_tick_of_5000_ms_...` and `b923_a_look_that_ends_at_a_tick_of_5000_ms_before_its_late_limit_of_6000_ms_...` start such a look while the copy task waits: it fails 5,000 ms after it began and is not said late by then; no test holds such a look past its late limit, so that it is said late there is held by no test |
//!
//! THAT a look that hangs is said, ended and followed by a copy is held on real time, by three tests of a real `agentd serve`
//! (`b823_a_look_that_hangs_in_git_add_...`, `b823_a_look_is_given_up_when_the_file_system_...`). In them the look that hangs is the
//! first look of the copy task (`first_look_hangs`): no look ran before it, so nothing is due to be said about an earlier look however
//! slow the machine is, and the look is late against the limit that was set. So the tests ask for the exact list of what the run was
//! told, in every run. They hold the arm of the rule in which the set limit applies, and that arm only: no copy is stored before
//! the look that hangs, so three times the last stored look never applies in them. That arm is held by the tests that drive the
//! clock. They hold no moment against the wall clock, and so no lower bound on the moment of the late line either: the numbers
//! of the worker's late lines are read as a check that each line agrees with itself (`the_late_lines_hold`: the time a line
//! states is not below the limit it states), never as a bound. That the first look of a worker is said late at the set limit and
//! not a millisecond before is held by the tests that drive the clock (`b893_the_first_look_of_a_worker_...`).
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::snapshot::{
    COPY_GIT_LIMIT, COPY_LATE_AFTER, COPY_STORE_LIMIT, GiveUp, TREE_ANSWER_LIMIT, TreeAlive,
    cut_unix, late_after, store_limit,
};
use agentd::server::{Ending, ends_after_the_copy};
use direct_support::{Direct, Opts, RUN, checkout_of_copy, first_process_possible};
use harness_protocol::direct::s3 as keys;
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::TurnOutcome;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the run is told when a look has run long and is reading the work tree.
const LATE_TREE: &str =
    "A copy of the work is taking long: reading the work tree has not ended. ham keeps trying.";
/// What the run is told when a look has run long and is storing its copy.
const LATE_STORE: &str =
    "A copy of the work is taking long: storing it has not ended. ham keeps trying.";
/// What the run is told when a copy was given up or failed.
const NOT_SAVED: &str = "A copy of the work could not be saved; ham keeps trying.";
/// What the run is told when the copy that a request to end asked for could not be saved.
const STOP_NOT_SAVED: &str =
    "The session was asked to end, and a copy of the work could not be saved.";

/// The late limit of the tests of a look that hangs in `git add`: the `AGENTD_DIRECT_COPY_LATE_MS` of the three that run a real
/// `agentd serve`, and the `copy_late` of the tests that drive the clock.
const HANG_LATE_LIMIT: Duration = Duration::from_millis(400);
/// How often a test asks while it waits ([`wait_for`]).
const ASKS_EVERY: Duration = Duration::from_millis(20);
/// The grain of the numbers of a late line: each is printed in seconds with three places, so each is off by half a millisecond at
/// the most, and three times a number by one and a half.
const LINE_GRAIN: Duration = Duration::from_millis(2);

/// Every version of `snapshot.json` in the bucket, oldest first: the fake store has them at once (the run's events reach the plane in batches).
fn records(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .versions_of(&keys::snapshot_json_key(RUN))
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
        .collect()
}

/// The `savedCopy` members of the run's progress events, in order.
fn saved_copies(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .events()
        .into_iter()
        .filter(|(t, _)| *t == EventType::TurnProgress)
        .filter_map(|(_, v)| v.get("savedCopy").cloned())
        .collect()
}

/// The `copy_not_saved` notices of the run, in order.
fn copy_notices(d: &Direct) -> Vec<String> {
    d.aws
        .progress("notify")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["reason"] == "copy_not_saved")
        .map(|v| v["detail"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A checkout of the snapshot bundle the bucket holds now, on its `snapshot` branch: the copy alone when it holds the whole history (the
/// command the client prints), and on the bundle the run started from when the copy needs it, as every reader does.
fn clone_of_snapshot(d: &Direct, name: &str) -> PathBuf {
    checkout_of_copy(&d.aws, &d.dir, &keys::snapshot_bundle_key(RUN), name)
}

/// Waits until `seen` holds, at most `limit`; `false` when it never did.
async fn wait_for(limit: Duration, mut seen: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while !seen() {
        if began.elapsed() > limit {
            return false;
        }
        tokio::time::sleep(ASKS_EVERY).await;
    }
    true
}

/// Looks into the copy in the bucket each time a new one is there (its record is stored after its bundle, so a new record means both).
#[derive(Default)]
struct CopyWatch {
    seen: usize,
    clones: u32,
}

impl CopyWatch {
    /// Whether a copy that came since the last call holds the file `name`.
    fn new_copy_holds(&mut self, d: &Direct, name: &str) -> bool {
        let now = records(d).len();
        if now == self.seen {
            return false;
        }
        self.seen = now;
        self.clones += 1;
        clone_of_snapshot(d, &format!("copy-{}", self.clones))
            .join(name)
            .is_file()
    }
}

// ---- a look that does not return ------------------------------------------------------------------------------------------------

/// The store takes the upload of one copy's bundle and never answers it. Before, the copy task waited for that one look (up to 494 s in the
/// product: four requests of 120 s and their pauses), no later look ran and the run was told nothing until the look had failed.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_look_whose_upload_gets_no_answer_is_said_and_the_timer_goes_on() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "14".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(300));
            c.direct.snapshot_floor = Some(Duration::from_millis(50));
            c.direct.upload_retry_pause = Duration::from_millis(100);
            c.direct.copy_late = Some(Duration::from_millis(1_500));
            c.direct.copy_store_limit = Some(Duration::from_millis(3_500));
        })),
        ..Opts::default()
    })
    .await;
    // The first upload of a copy's bundle gets no answer for a minute; the store answers every PUT after it at once.
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(60), 1);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let copies = saved_copies(&d);
    let notices = copy_notices(&d);
    assert!(
        copies.iter().any(|c| c.get("final").is_none()),
        "TRIGGER PRESENT: one upload got no answer and no copy was stored while the engine worked on for about 14 s with a 300 ms \
         timer: copies {copies:?}; notices {notices:?}"
    );
    // The run was told when the look was late. (On a build host under load the git work of that look can itself take longer than the late
    // limit of this test: the run is then told that reading the tree is slow.)
    assert!(
        notices
            .first()
            .is_some_and(|n| n == LATE_STORE || n == LATE_TREE),
        "{notices:?}"
    );
    // And it was told that the copy could not be saved when the same look was given up: the late sentence says that ham keeps trying, and
    // without this one the run never learns that the copy did not come.
    assert_eq!(
        notices.iter().filter(|n| *n == NOT_SAVED).count(),
        1,
        "TRIGGER PRESENT: a look was said to be late and was then given up, and the run was not told that its copy could not be saved: \
         {notices:?}"
    );
    assert!(
        notices
            .iter()
            .all(|n| n == LATE_STORE || n == LATE_TREE || n == NOT_SAVED),
        "{notices:?}"
    );
    let s = d.summary();
    assert!(
        s.snapshots.failures >= 1,
        "the look that was given up is counted: {:?}",
        s.snapshots
    );
    // The worker's log names the late look and the one that was given up.
    let log = d.log.lines().join("\n");
    assert!(log.contains("direct turn snapshot late: "), "{log}");
    assert!(
        log.contains("direct turn snapshot failed: not stored within"),
        "{log}"
    );
}

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

/// Writes the script `text` to `path` as a program.
fn program_file(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A stand-in for a work tree that stops answering: a `git` that never returns from `add` while the file `mark` exists, in a folder that
/// leads the search path of `agentd serve`. Every other call, and every call once the mark is gone, is the real git.
fn hanging_git(dir: &Path) -> (PathBuf, PathBuf) {
    let real = program("git");
    let bin = dir.join("slow-tree-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mark = dir.join("the-tree-does-not-answer");
    let script = format!(
        "#!/bin/sh\nif [ -e '{mark}' ]; then for a in \"$@\"; do if [ \"$a\" = add ]; then exec sleep 600; fi; done; fi\nexec '{real}' \"$@\"\n",
        mark = mark.display()
    );
    program_file(&bin.join("git"), &script);
    (bin, mark)
}

/// The file a stand-in writes when it lets its first `git add` through ([`hanging_git_behind_the_first_add`], [`waiting_git`]).
fn first_add_file(dir: &Path) -> PathBuf {
    dir.join("the-first-git-add-went-through")
}

/// [`hanging_git`] for a mark that is set before the engine works. The first `add` the stand-in sees while the mark exists goes
/// through, and the stand-in writes [`first_add_file`]: the worker reads the tree once before it starts the engine
/// (`Snapshotter::new`), and that reading is no look of the copy task. Every later `add` never returns while the mark exists. So
/// the mark holds the first look of the copy task and nothing before it.
fn hanging_git_behind_the_first_add(dir: &Path) -> (PathBuf, PathBuf) {
    let real = program("git");
    let bin = dir.join("slow-tree-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mark = dir.join("the-tree-does-not-answer");
    let script = format!(
        "#!/bin/sh\nif [ -e '{mark}' ]; then for a in \"$@\"; do if [ \"$a\" = add ]; then if [ -e '{first}' ]; then exec sleep 600; fi; : > '{first}'; fi; done; fi\nexec '{real}' \"$@\"\n",
        mark = mark.display(),
        first = first_add_file(dir).display()
    );
    program_file(&bin.join("git"), &script);
    (bin, mark)
}

/// A stand-in like [`hanging_git`] for a work tree that answers again: its `git add` waits for as long as the file `mark` exists and
/// then goes on as the real git, so the look it held up ends by itself. While it waits, the process agentd started as git is a script
/// named `sleep`, so [`stand_in_sleeps`] sees it as it sees the `sleep` of [`hanging_git`]. It asks for the mark every tenth of a
/// second and a thousand times at the most, so a stand-in that outlives its test goes on by itself as well.
/// Its first `add` while the mark exists goes through, as that of [`hanging_git_behind_the_first_add`] does: the mark is set
/// before the engine works and holds the first look of the copy task.
fn waiting_git(dir: &Path) -> (PathBuf, PathBuf) {
    let real = program("git");
    let sleep = program("sleep");
    let bin = dir.join("slow-tree-bin");
    let wait = dir.join("slow-tree-wait");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&wait).unwrap();
    let mark = dir.join("the-tree-does-not-answer");
    let waiter = wait.join("sleep");
    let ten = "0 1 2 3 4 5 6 7 8 9";
    let script = format!(
        "#!/bin/sh\nfor a in {ten}; do for b in {ten}; do for c in {ten}; do\nif [ -e '{mark}' ]; then '{sleep}' 0.1; else exec '{real}' \"$@\"; fi\ndone; done; done\nexec '{real}' \"$@\"\n",
        mark = mark.display()
    );
    program_file(&waiter, &script);
    let script = format!(
        "#!/bin/sh\nif [ -e '{mark}' ]; then for a in \"$@\"; do if [ \"$a\" = add ]; then if [ -e '{first}' ]; then exec '{waiter}' \"$@\"; fi; : > '{first}'; fi; done; fi\nexec '{real}' \"$@\"\n",
        mark = mark.display(),
        first = first_add_file(dir).display(),
        waiter = waiter.display()
    );
    program_file(&bin.join("git"), &script);
    (bin, mark)
}

/// The process ids of the stand-ins of [`hanging_git`] or [`waiting_git`] that hang below the worker with the process id `worker`
/// now, as `pgrep` prints them; empty when none does. The stand-in becomes its `sleep` in the process agentd started as git, so
/// that `sleep` is a child of the worker for as long as the call hangs.
fn stand_ins_left(worker: u32) -> String {
    let found = std::process::Command::new("pgrep")
        .args(["-P", &worker.to_string(), "sleep"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&found.stdout).trim().to_string()
}

/// Whether a `git add` of the worker with the process id `worker` hangs in a stand-in now.
fn stand_in_sleeps(worker: u32) -> bool {
    !stand_ins_left(worker).is_empty()
}

/// The `copy_not_saved` notices of the run that stand before and behind the `savedCopy` event of the copy with the number `copy`,
/// each in order. The plane has the events of a run in one list, in the order the worker made them, and the worker makes the event
/// of a copy when it has stored the copy. So the notices behind that event were made after the copy was stored, and a notice of a
/// look before that copy is never among them, however late the batch that carried it arrived.
fn notices_around(d: &Direct, copy: u64) -> (Vec<String>, Vec<String>) {
    let mut before = Vec::new();
    let mut behind = Vec::new();
    let mut found = false;
    for (kind, v) in d.aws.events() {
        if kind != EventType::TurnProgress {
            continue;
        }
        if v["savedCopy"]["count"].as_u64() == Some(copy) {
            found = true;
        } else if v["kind"] == "notify" && v["reason"] == "copy_not_saved" {
            let said = v["detail"].as_str().unwrap_or_default().to_string();
            if found {
                behind.push(said);
            } else {
                before.push(said);
            }
        }
    }
    assert!(found, "the run has the event of copy {copy}");
    (before, behind)
}

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

/// The worker's own numbers are consistent, for every late line in `since`, a part of the worker's log. It is a check of
/// the lines that exist and never the bound of the rule: a worker that states wrong numbers passes it, and the moment a look is
/// said late is held on a clock the test drives (the tests `b893_...`). For each line:
///
/// - the limit the worker applied is the limit these tests set ([`HANG_LATE_LIMIT`]), or three times what the last look that
///   stored a copy took when that is longer (within [`LINE_GRAIN`]). The rule is written out here and is not taken from the
///   worker's own function: a worker whose rule is wrong would move the expected limit with it;
/// - the time the line states is not below the limit the same line states. Both are the worker's own numbers: this holds that
///   the line agrees with itself, and nothing about when the worker spoke. A worker that speaks early and states a time at its
///   limit passes it. No test of a real worker holds a lower bound on the moment of the late line; the tests that drive the
///   clock do, for the first look of a worker too;
/// - the limit that was set is the one these tests set.
///
/// How far above its limit a line may state its time is not checked. The worker reads its clock when its late timer has fired and
/// the copy task has a processor again, and on a crowded machine that wait has no constant: a worker that is right stated 744 ms
/// and 706 ms against a limit of 400 ms beside 32 busy loops on eight processors. A look that is said late too late is held where the
/// time is the test's own (the tests `b893_...`).
///
/// A line that does not state its numbers fails the test. What the lines state is printed (`STATED`) and returned, in order.
fn the_late_lines_hold(since: &str, log: &str) -> Vec<SaidLate> {
    let mut all = Vec::new();
    for line in since
        .lines()
        .filter(|l| l.contains("direct turn snapshot late: "))
    {
        let Some(said) = said_late(line.trim_end()) else {
            panic!(
                "the worker's late line does not state how long the look had taken and the limit it applied: {line:?}; log:\n{log}"
            );
        };
        println!(
            "STATED: the worker said a look late after {} ms; its limit was {} ms, the set limit {} ms; the last stored copy took {}",
            said.taken.as_millis(),
            said.limit.as_millis(),
            said.set.as_millis(),
            said.last.map_or_else(
                || "none".to_string(),
                |took| format!("{} ms", took.as_millis())
            )
        );
        let limit = said
            .last
            .map_or(HANG_LATE_LIMIT, |took| HANG_LATE_LIMIT.max(took * 3));
        assert!(
            said.limit <= limit + LINE_GRAIN && limit <= said.limit + LINE_GRAIN,
            "the worker applied a late limit of {} ms. The limit is {} ms: the {} ms this test set, or three times what the last \
             look that stored a copy took ({:?}) when that is longer; {line:?}; log:\n{log}",
            said.limit.as_millis(),
            limit.as_millis(),
            HANG_LATE_LIMIT.as_millis(),
            said.last
        );
        assert!(
            said.taken >= said.limit,
            "the worker's late line states a time below the limit it states: {} ms taken, and a limit of {} ms. The line does not \
             agree with itself (this is no check of when the worker spoke); {line:?}; log:\n{log}",
            said.taken.as_millis(),
            said.limit.as_millis()
        );
        assert_eq!(
            said.set, HANG_LATE_LIMIT,
            "the worker states a set late limit that this test did not set; {line:?}; log:\n{log}"
        );
        all.push(said);
    }
    all
}

/// How long the tests of a look that hangs wait for the first look of the copy task to hang in `git add`: as long as the other
/// tests of this file wait for the first copy of the engine's first tool call (`working_process`).
const FIRST_LOOK_WITHIN: Duration = Duration::from_secs(30);

/// The files the engine of the scenario `writes_files` writes, below the work tree: one in each of its two tool calls.
const SCENARIO_FILES: [&str; 2] = ["src/calc.py", "tests/test_calc.py"];

/// Whether the engine has written both files of its scenario ([`SCENARIO_FILES`]): each is in the tree, and the engine's log names
/// its edit as written, a line the engine writes when the file is written.
fn the_scenario_is_in_the_tree(d: &Direct) -> bool {
    let engine = d.fake_engine_log();
    SCENARIO_FILES.iter().all(|file| {
        d.worktree().join(file).is_file()
            && engine
                .lines()
                .any(|l| l.ends_with(&format!("/{file} written")))
    })
}

/// Every file and folder below `path`, sorted, as paths below it, 60 at the most; one line that says so when nothing is there.
fn what_stands_at(path: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let at = entry.path();
            let name = at.strip_prefix(root).unwrap_or(&at).display().to_string();
            if at.is_dir() && !at.is_symlink() {
                found.push(format!("{name}/"));
                walk(root, &at, found);
            } else {
                found.push(name);
            }
        }
    }
    if !path.exists() {
        return vec![format!("nothing: {} does not exist", path.display())];
    }
    let mut found = Vec::new();
    walk(path, path, &mut found);
    found.sort();
    found.truncate(60);
    found
}

/// A real `agentd serve` whose first look hangs in `git add`. The mark of the stand-in
/// ([`hanging_git_behind_the_first_add`] or [`waiting_git`], first on the search path that `more` gives the process) is set
/// before the engine works. The stand-in lets the first `git add` through, which is the reading of the tree the worker makes
/// before it starts the engine, and holds the second: the first look of the copy task, the one the engine's first tool call asks
/// for. Returns when that look hangs; the tree then holds `late.txt`, a file that no copy has.
///
/// So no look ran before the look that hangs, and no copy was stored before it. Nothing is due to be said about an earlier look,
/// however slow the machine is; and the look that hangs is late against the late limit that was set, not against three times an
/// earlier look, so it is said late before anything else can be said about it. That holds in every run, and it is why these tests
/// can ask for the exact list of what the run was told.
async fn first_look_hangs(
    mark: &Path,
    silence_s: &str,
    more: Vec<(&'static str, String)>,
) -> Direct {
    let mut agentd_env = vec![
        ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", "300".to_string()),
        ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
    ];
    agentd_env.extend(more);
    let d = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", silence_s.into())],
        agentd_env,
        ..Opts::default()
    })
    .await;
    // From the start the tree does not answer a look.
    std::fs::write(mark, "set\n").unwrap();
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    let worker = d.process.as_ref().unwrap().pid();
    let hangs = wait_for(FIRST_LOOK_WITHIN, || stand_in_sleeps(worker)).await;
    let log = d.process.as_ref().unwrap().log_text();
    assert!(
        hangs,
        "TRIGGER PRESENT: the first look of the copy task did not hang in `git add` within {} s; log:\n{log}",
        FIRST_LOOK_WITHIN.as_secs()
    );
    assert!(
        log.contains("direct turn copies: the first reading of the tree took "),
        "TRIGGER PRESENT: the `git add` the stand-in let through was the worker's reading of the tree before the engine started; \
         log:\n{log}"
    );
    assert!(
        records(&d).is_empty(),
        "TRIGGER PRESENT: no copy was stored before the look that hangs: {:?}",
        records(&d)
    );
    // The engine's two edits are two tool calls, and it makes the second while the look that its first one asked for hangs. Both
    // files are in the tree before a test goes on: the engine makes the folder of a file before it writes the file, so an edit
    // that came after a test had moved the tree away would be written at the tree's path, where the tree could then not go back.
    let written = wait_for(FIRST_LOOK_WITHIN, || the_scenario_is_in_the_tree(&d)).await;
    assert!(
        written,
        "TRIGGER PRESENT: the engine had not written both files of its scenario ({SCENARIO_FILES:?}) into the tree within {} s; \
         the tree holds {:?}; the engine's log:\n{}\nlog:\n{}",
        FIRST_LOOK_WITHIN.as_secs(),
        what_stands_at(&d.worktree()),
        d.fake_engine_log(),
        d.process.as_ref().unwrap().log_text()
    );
    std::fs::write(
        d.worktree().join("late.txt"),
        "written after the look began to hang\n",
    )
    .unwrap();
    d
}

/// What the run was told about its copies before the event of its first stored copy, in order: everything it was told since it
/// began. The events of a run reach the plane in batches, so the test waits for that event first (the worker makes it when it has
/// stored the copy, after every notice of a look before that copy).
async fn told_before_the_first_copy(d: &Direct) -> Vec<String> {
    let first = records(d)
        .first()
        .and_then(|r| r["count"].as_u64())
        .expect("a copy is stored");
    let seen = wait_for(Duration::from_secs(8), || {
        saved_copies(d)
            .iter()
            .any(|c| c["count"].as_u64() == Some(first))
    })
    .await;
    assert!(
        seen,
        "the event of copy {first} did not reach the run: {:?}",
        saved_copies(d)
    );
    notices_around(d, first).0
}

/// The work tree stops answering for a while: the `git add` of a look does not return until the tree answers again. The look has a
/// minute for its git work, so it is not given up, and the run is told that its copy is late. When the tree answers again the look
/// ends by itself and stores its copy.
///
/// The look that hangs is the first look of the copy task ([`first_look_hangs`]), so the run is told exactly one thing, in every
/// run. When the worker says a look late is held on a clock the test drives (the tests `b893_...` below); this test holds the whole
/// of it on real time, with the waits of the test before it.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_look_that_hangs_in_git_add_is_said_late_and_copies_when_the_tree_answers_again() {
    let scratch = common::scratch("late-tree");
    let (bin, mark) = waiting_git(&scratch);
    let d = first_look_hangs(
        &mark,
        "60",
        vec![
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
            ("AGENTD_DIRECT_COPY_LATE_MS", "400".into()),
            // The git work of a look has a minute, as in the test of a tree that no longer answers: this look is not given up.
            ("AGENTD_DIRECT_COPY_GIT_LIMIT_MS", "60000".into()),
        ],
    )
    .await;
    let worker = d.process.as_ref().unwrap().pid();
    let log = || d.process.as_ref().unwrap().log_text();
    // The run is told when that look is late.
    let told = wait_for(Duration::from_secs(6), || !copy_notices(&d).is_empty()).await;
    // What the run was told and what the worker logged while the look still hangs: nothing else can be said before the tree
    // answers again, because the copy task runs one look at a time.
    let said = copy_notices(&d);
    let logged = log();
    let still_hangs = stand_in_sleeps(worker);
    // The tree answers again: the stand-in goes on as the real git.
    std::fs::remove_file(&mark).unwrap();
    assert!(
        told,
        "TRIGGER PRESENT: a look hung on the work tree for 6 s and the run was told nothing; copies {:?}; log:\n{}",
        records(&d),
        log()
    );
    assert_eq!(said, vec![LATE_TREE.to_string()]);
    assert!(
        logged.contains("direct turn snapshot late: reading the work tree"),
        "the log names the look that is late:\n{logged}"
    );
    // The worker's own numbers of that line are consistent (`the_late_lines_hold`), and it is the one late line.
    let stated = the_late_lines_hold(&logged, &logged);
    assert_eq!(
        stated.len(),
        1,
        "one look was said late, once, while it hung; log:\n{logged}"
    );
    assert_eq!(
        stated[0].last, None,
        "TRIGGER PRESENT: no copy was stored before the look that hung, so its late limit is the one that was set; log:\n{logged}"
    );
    assert!(
        still_hangs,
        "the look that hung had ended when the run was told that it is late; log:\n{}",
        log()
    );
    // The look ends by itself and stores its copy. Its `git add` went on after the file was written, so that copy holds the file.
    let mut watch = CopyWatch::default();
    let copied = wait_for(Duration::from_secs(8), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        copied,
        "no copy held the file after the tree answered again: {:?}; log:\n{}",
        records(&d),
        log()
    );
    // The look was not given up: up to the event of its copy the run was told that one thing, and no look failed before it.
    assert_eq!(
        told_before_the_first_copy(&d).await,
        [LATE_TREE.to_string()],
        "what the run was told before the look that hung stored its copy"
    );
    assert!(
        !log().contains("direct turn snapshot failed:"),
        "a look failed after a look hung and the tree answered again:\n{}",
        log()
    );
    let left = stand_ins_left(worker);
    assert!(
        left.is_empty(),
        "a stand-in still waits after the tree answered again: {left}; log:\n{}",
        log()
    );
}

/// The work tree stops answering: the `git add` of a look never returns. Before, the look was waited for without a limit, so no
/// later look ran, nothing was logged and the run was told nothing for as long as the process lived.
///
/// The look is said late, it is given up at the limit of its git work, its git is ended, and a later look copies. The look that
/// hangs is the first look of the copy task ([`first_look_hangs`]): its late limit is the 400 ms that were set and its git work has
/// 6 s, so the run is told that the copy is late first and that it could not be saved second, and nothing else before the copy
/// of the later look: in every run.
///
/// The limit of 6 s is for the git work of every look of this worker. With 1.5 s, the real git of the looks after the one that
/// hangs was cut at that limit on a crowded machine, three looks in a row, and no look stored the copy. The test waits 22 s for
/// that copy: the look that hangs is given up 6 s after it began, and two later looks then have their full 6 s each, after the
/// spacing that follows a look that was given up (five periods of the 300 ms timer at the most). The engine is silent for 28 s,
/// the 6 s of the first wait and those 22 s, so the later look copies while the engine still works.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_look_that_hangs_in_git_add_is_given_up_its_git_is_ended_and_a_later_look_copies() {
    let scratch = common::scratch("slow-tree");
    let (bin, mark) = hanging_git_behind_the_first_add(&scratch);
    let d = first_look_hangs(
        &mark,
        "28",
        vec![
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
            ("AGENTD_DIRECT_COPY_LATE_MS", "400".into()),
            ("AGENTD_DIRECT_COPY_GIT_LIMIT_MS", "6000".into()),
        ],
    )
    .await;
    let worker = d.process.as_ref().unwrap().pid();
    let log = || d.process.as_ref().unwrap().log_text();
    // The run is told when that look is late.
    let told = wait_for(Duration::from_secs(6), || !copy_notices(&d).is_empty()).await;
    // The tree answers again (also for the end commit of a build that never gives a look up).
    std::fs::remove_file(&mark).unwrap();
    assert!(
        told,
        "TRIGGER PRESENT: a look hung on the work tree for 6 s and the run was told nothing; copies {:?}; log:\n{}",
        records(&d),
        log()
    );
    // The look is given up, its git is ended, and a later look stores the file while the engine still works. The file was written
    // after the look hung and the looks run one after another, so a copy that holds it comes from a look after the one that hung.
    let mut watch = CopyWatch::default();
    let copied = wait_for(Duration::from_secs(22), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        copied,
        "no copy held the file after the tree answered again: {:?}; log:\n{}",
        records(&d),
        log()
    );
    // What the run was told before that copy, all of it and in its order. The list is read at the event of the copy and not at the
    // moment of the first notice: the look is given up 5.6 s after it is late, so a test that looked a moment late would find both.
    assert_eq!(
        told_before_the_first_copy(&d).await,
        [LATE_TREE.to_string(), NOT_SAVED.to_string()],
        "what the run was told before the look after the hang stored its copy; log:\n{}",
        log()
    );
    let logged = log();
    assert!(
        logged.contains("direct turn snapshot late: reading the work tree"),
        "{logged}"
    );
    assert!(logged.contains("direct turn snapshot failed:"), "{logged}");
    // The worker's own numbers of every look it said late are consistent (`the_late_lines_hold`); the first is the look that hung.
    let stated = the_late_lines_hold(&logged, &logged);
    assert_eq!(
        stated.first().map(|s| s.last),
        Some(None),
        "TRIGGER PRESENT: no copy was stored before the look that hung, so its late limit is the one that was set; log:\n{logged}"
    );
    // No `sleep` of the stand-in is left behind: the git that was given up was ended.
    let left = stand_ins_left(worker);
    assert!(
        left.is_empty(),
        "a git that was given up still runs: {left}; log:\n{}",
        log()
    );
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(d.summary().snapshots.failures >= 1);
}

/// The file system of the work tree stops answering while a look reads the tree: here the tree is moved away, so the question agentd asks
/// the file system fails. The look is given up at that question, not at the long limit of its git work (a minute here, an hour in the
/// product), so a tree that is gone costs seconds and a repository whose copies take long keeps its copies.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_look_is_given_up_when_the_file_system_of_the_tree_no_longer_answers() {
    let scratch = common::scratch("tree-gone");
    let (bin, mark) = hanging_git_behind_the_first_add(&scratch);
    let d = first_look_hangs(
        &mark,
        "40",
        vec![
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
            ("AGENTD_DIRECT_COPY_LATE_MS", "400".into()),
            // The git work of a look has a minute: only the question to the file system can end this look sooner.
            ("AGENTD_DIRECT_COPY_GIT_LIMIT_MS", "60000".into()),
        ],
    )
    .await;
    let worker = d.process.as_ref().unwrap().pid();
    let log = || d.process.as_ref().unwrap().log_text();
    // The look hangs in `git add`. The file system still answers, so the run is told that the copy is taking long.
    let told = wait_for(Duration::from_secs(6), || !copy_notices(&d).is_empty()).await;
    // What the run was told and what the worker logged while the look still hangs and the tree is still there.
    let said = copy_notices(&d);
    let logged = log();
    assert!(
        told,
        "the look that hangs is said: {:?}; log:\n{logged}",
        records(&d)
    );
    assert_eq!(said, vec![LATE_TREE.to_string()]);
    // The worker's own numbers of that line are consistent (`the_late_lines_hold`), and it is the one late line.
    let stated = the_late_lines_hold(&logged, &logged);
    assert_eq!(
        stated.len(),
        1,
        "one look was said late, once, while it hung; log:\n{logged}"
    );
    assert_eq!(
        stated[0].last, None,
        "TRIGGER PRESENT: no copy was stored before the look that hung, so its late limit is the one that was set; log:\n{logged}"
    );
    // Now the tree is gone, while the look that hung still hangs on it.
    assert!(
        stand_in_sleeps(worker),
        "TRIGGER PRESENT: the look that hung had ended before the tree went away; log:\n{}",
        log()
    );
    let away = d.worktree().with_file_name("repo-away");
    std::fs::rename(d.worktree(), &away).unwrap();
    let given_up = wait_for(Duration::from_secs(5), || {
        log().contains("direct turn snapshot failed:")
    })
    .await;
    // The tree is back and answers again. The engine had written both of its files before the tree went away
    // (`first_look_hangs`); a failure here names what stands at the tree's path, so that it names its writer.
    if let Err(e) = std::fs::rename(&away, d.worktree()) {
        panic!(
            "the tree could not be moved back to its path ({e}): something was written at that path while the tree was away. \
             What stands at the path now: {:?}; the engine's log:\n{}\nlog:\n{}",
            what_stands_at(&d.worktree()),
            d.fake_engine_log(),
            log()
        );
    }
    std::fs::remove_file(&mark).unwrap();
    assert!(
        given_up,
        "TRIGGER PRESENT: the tree was gone for 5 s and the look that hung on it was not given up; log:\n{}",
        log()
    );
    assert!(
        log().contains("direct turn snapshot: the work tree"),
        "the log names the file system that did not answer:\n{}",
        log()
    );
    // A later look stores the file.
    let mut watch = CopyWatch::default();
    let copied = wait_for(Duration::from_secs(8), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        copied,
        "no copy held the file after the tree was back: {:?}; log:\n{}",
        records(&d),
        log()
    );
    // No `sleep` of the stand-in is left behind: the git of the look that was given up at the question to the file system was ended.
    let left = stand_ins_left(worker);
    assert!(
        left.is_empty(),
        "a git that was given up still runs: {left}; log:\n{}",
        log()
    );
}

// ---- the late limit of a look, on a clock the test drives ---------------------------------------------------------------------
//
// From outside the worker no test knows when a look began or what the last stored look took, and a crowded machine gives no bound on
// how late a task that is ready gets a processor. So the moment a look is said late is held here, where the time is the test's own.
// The worker is served inside the test's runtime (`Direct::start`), and the clock of that runtime is paused: the copy task reads it
// for the three things its late rule rests on (what a look that stored a copy took, the start and the one late timer of
// `watched_look`, and the timers that give a look up). The test holds that clock while real work runs (git, the store, the engine),
// and moves it itself.

/// The variable that names the scratch folder of a test that drives the clock, in the process that test started for itself
/// ([`on_a_driven_clock`]).
const DRIVEN_DIR: &str = "B893_DRIVEN_DIR";

/// The timer of the copy task in the tests that drive the clock. The spacing of the looks is capped at five of its periods
/// (`snapshot::COPY_SPACING_CAP`), and it is shorter than every duration these tests impose, so the tick of the look after a stored
/// look is due when that look ends.
const DRIVEN_TIMER: Duration = Duration::from_millis(10);

/// The flusher of the worker posts the events of the run this often in these tests (the `events_every` of the test rig).
const DRIVEN_POSTS_EVERY: Duration = Duration::from_millis(100);

/// How long a test that drives the clock waits, in real time and while the clock stands, for real work it knows to be under way: a
/// git, a request to the store, a post of the flusher. No moment of the late rule is measured against it. It is the patience the
/// shared rig has for the start of a real `agentd serve` (60 s).
const REAL_WORK_WITHIN: Duration = direct_support::REAL_WORK_WITHIN;

/// How long past its late limit the tests of a look that is said late keep the look held, on the driven clock: the git work of
/// the look has this much more than the late limit, so the look is given up a minute and a half after it was said late. Driven
/// time costs no real time.
const HELD_PAST_THE_LIMIT: Duration = Duration::from_secs(90);
/// The last moment past its late limit at which those tests stop and look, before the look is given up.
const A_MINUTE: Duration = Duration::from_secs(60);

/// The threads for blocking work in the runtime of a test that holds the worker's question to the tree
/// ([`on_a_driven_clock_with`], [`TreeQuestionHeld`]): few, so that the test can take every one of them.
const HELD_QUESTION_THREADS: usize = 4;
/// How long, in real time, such a test keeps the question held while it looks for a late line that must not come.
const QUESTION_HELD_FOR: Duration = Duration::from_millis(300);

/// What a test that drives the clock imposes on the look that stores the last copy, and the late limit that follows from it for
/// the look after it. Both numbers are written out, each pair in its own constant below, and the limit is never asked of the
/// worker's own rule (`snapshot::late_after`): a worker whose rule is wrong would move the moment the test looks at with it, and
/// pass. The rule the pairs follow: the late limit that is set for the worker, or three times what the last look that stored a
/// copy took when that is longer.
#[derive(Clone, Copy)]
struct Imposed {
    /// What the look that stored the last copy took.
    last: Duration,
    /// The late limit of the look after it.
    limit: Duration,
    /// The late limit that is set for the worker of the test: the 400 ms of every test of a look that hangs
    /// ([`HANG_LATE_LIMIT`]), but for the stored look of a second. The look that stores the last copy is the first look of its
    /// worker, so its own late limit is the one that is set, and a look that takes longer than that is rightly said late itself.
    set: Duration,
}

/// The set limit applies: three times 100 ms is less than the 400 ms these tests set.
const LAST_100_MS: Imposed = Imposed {
    last: Duration::from_millis(100),
    limit: Duration::from_millis(400),
    set: HANG_LATE_LIMIT,
};
/// Three times the last stored look applies: 600 ms, and not the 400 ms these tests set.
const LAST_200_MS: Imposed = Imposed {
    last: Duration::from_millis(200),
    limit: Duration::from_millis(600),
    set: HANG_LATE_LIMIT,
};
/// Three times the last stored look applies, with a number that is not round: 411 ms. A rule that counts in tenths of a second or
/// in hundredths gives 400 ms, 390 ms or 420 ms.
const LAST_137_MS: Imposed = Imposed {
    last: Duration::from_millis(137),
    limit: Duration::from_millis(411),
    set: HANG_LATE_LIMIT,
};
/// Three times the last stored look applies, with a look of a whole second: 3,000 ms. The limit that is set for this worker is
/// 1,500 ms, so that the stored look of a second is not late itself. A rule that reads only the part of a duration below a
/// second gives those 1,500 ms.
const LAST_1000_MS: Imposed = Imposed {
    last: Duration::from_millis(1_000),
    limit: Duration::from_millis(3_000),
    set: Duration::from_millis(1_500),
};
/// Three times the last stored look applies, with a look of a second and a half: 4,500 ms. The limit that is set for this worker
/// is 2,000 ms, so that the stored look is not late itself. A rule that takes twice a look of more than a second gives 3,000 ms.
const LAST_1500_MS: Imposed = Imposed {
    last: Duration::from_millis(1_500),
    limit: Duration::from_millis(4_500),
    set: Duration::from_millis(2_000),
};

/// What stands before the look that a test holds, on the driven clock (the rows of the table in the header of this file).
#[derive(Clone, Copy)]
enum Before {
    /// Nothing: the held look is the first look of its worker.
    Nothing,
    /// One look, which stored a copy and took this long. The held look begins at the moment it ends.
    Stored(Duration),
    /// Two looks, each of which stored a copy; what they took, in order. Each look begins at the moment the look before it ends.
    StoredTwice(Duration, Duration),
    /// A look that stored a copy and took the first time; then a look that found the tree unchanged and stored nothing. The held
    /// look begins the second time after the stored look ended.
    StoredThenUnchanged(Duration, Duration),
    /// A look that stored a copy and took the first time; then no look for the second time, and the held look begins.
    StoredThenNoLook(Duration, Duration),
}

/// A look that is held and what a test expects of it. Every number is written out in the constant that makes the scene; none is
/// asked of the worker's own rule.
#[derive(Clone, Copy)]
struct Scene {
    before: Before,
    /// The late limit of the held look, counted from its start.
    limit: Duration,
    /// The late limit that is set for the worker.
    set: Duration,
    /// What the last look that stored a copy took, as the worker's late line must state it; `None` when no look stored one.
    last: Option<Duration>,
    /// A moment before the limit, counted from the start of the held look, at which the test also stops and requires that no
    /// late line was written: the limit that a rule other than the worker's would give.
    nothing_at: Option<Duration>,
}

impl From<Imposed> for Scene {
    fn from(imposed: Imposed) -> Self {
        Self {
            before: Before::Stored(imposed.last),
            limit: imposed.limit,
            set: imposed.set,
            last: Some(imposed.last),
            nothing_at: None,
        }
    }
}

impl Scene {
    /// What stands before the held look, in words for a message.
    fn story(&self) -> String {
        let ms = |d: Duration| d.as_millis();
        match self.before {
            Before::Nothing => {
                "no look ran before it: it is the first look of its worker".to_string()
            }
            Before::Stored(last) => format!("the last stored look took {} ms", ms(last)),
            Before::StoredTwice(first, second) => format!(
                "two looks stored a copy before it; they took {} ms and then {} ms",
                ms(first),
                ms(second)
            ),
            Before::StoredThenUnchanged(stored, later) => format!(
                "a look stored a copy and took {} ms, the look after it found the tree unchanged and stored nothing, and this \
                 look began {} ms after the stored look had ended",
                ms(stored),
                ms(later)
            ),
            Before::StoredThenNoLook(stored, pause) => format!(
                "a look stored a copy and took {} ms, and this look began {} ms after that look had ended, with no look between",
                ms(stored),
                ms(pause)
            ),
        }
    }

    /// Builds the scene: returns when the look is held in `git add`. The git work of a look has `git_limit`.
    async fn held(&self, git_limit: Duration, dir: &Path) -> HeldLook {
        match self.before {
            Before::Nothing => the_first_look_held(self.set, git_limit, dir).await,
            Before::Stored(last) => {
                a_look_held_after_a_stored_look(last, self.set, git_limit, dir).await
            }
            Before::StoredTwice(first, second) => {
                a_look_held_after_two_stored_looks(first, second, self.set, git_limit, dir).await
            }
            Before::StoredThenUnchanged(stored, later) => {
                a_look_held_after_a_stored_and_an_unchanged_look(
                    stored, later, self.set, git_limit, dir,
                )
                .await
            }
            Before::StoredThenNoLook(stored, pause) => {
                a_look_held_after_a_stored_look_and_no_look_for(
                    stored, pause, self.set, git_limit, dir,
                )
                .await
            }
        }
    }
}

/// The first look of a worker is held: no look stored a copy, so the limit that is set applies, 400 ms.
const THE_FIRST_LOOK: Scene = Scene {
    before: Before::Nothing,
    limit: Duration::from_millis(400),
    set: HANG_LATE_LIMIT,
    last: None,
    nothing_at: None,
};
/// Two stored looks of 100 ms and then 200 ms: the last one counts, so the limit is 600 ms. A worker that keeps the limit of
/// the first stored look says the held look late at 400 ms; one that measures the second look from the start of the first
/// (300 ms) says it late at 900 ms.
const STORED_100_THEN_200_MS: Scene = Scene {
    before: Before::StoredTwice(Duration::from_millis(100), Duration::from_millis(200)),
    limit: Duration::from_millis(600),
    set: HANG_LATE_LIMIT,
    last: Some(Duration::from_millis(200)),
    nothing_at: Some(Duration::from_millis(400)),
};
/// Two stored looks of 200 ms and then 100 ms: the last one counts, so the limit is the 400 ms that are set. A worker that
/// keeps the limit of the first stored look, or takes the longest stored look, says the held look late at 600 ms.
const STORED_200_THEN_100_MS: Scene = Scene {
    before: Before::StoredTwice(Duration::from_millis(200), Duration::from_millis(100)),
    limit: Duration::from_millis(400),
    set: HANG_LATE_LIMIT,
    last: Some(Duration::from_millis(100)),
    nothing_at: None,
};
/// A stored look of 200 ms, then a look that found the tree unchanged, and the held look begins 50 ms after the stored look
/// ended: only a look that stored a copy counts, so the limit is 600 ms. A worker that takes every look that returned (the
/// unchanged look took 50 ms at the most) says the held look late at 400 ms.
const STORED_200_MS_THEN_UNCHANGED: Scene = Scene {
    before: Before::StoredThenUnchanged(Duration::from_millis(200), Duration::from_millis(50)),
    limit: Duration::from_millis(600),
    set: HANG_LATE_LIMIT,
    last: Some(Duration::from_millis(200)),
    nothing_at: Some(Duration::from_millis(400)),
};
/// A stored look of 200 ms, then no look for 150 ms, then the held look: its limit is 600 ms and is counted from its own
/// start. A worker that counts from the end of the look before says it late 450 ms after it began.
const STORED_200_MS_THEN_NO_LOOK_FOR_150_MS: Scene = Scene {
    before: Before::StoredThenNoLook(Duration::from_millis(200), Duration::from_millis(150)),
    limit: Duration::from_millis(600),
    set: HANG_LATE_LIMIT,
    last: Some(Duration::from_millis(200)),
    nothing_at: Some(Duration::from_millis(450)),
};

/// The timer of the worker in a scene in which a look follows a look that stored nothing ([`a_look_held_on_the_timer`]). After
/// such a look the worker waits for its spacing, which it counts on the machine's clock and sleeps on the runtime's: under
/// [`DRIVEN_TIMER`] the next look would begin some milliseconds later on the driven clock, as many as the machine was fast. The
/// spacing is capped at five periods of a timer that a test set (`snapshot::COPY_SPACING_CAP`), and five of these are less than
/// the worker's code between two readings of the machine's clock takes: so no spacing holds a look back, and the tick of the
/// next look is due one grain of the driven clock (a millisecond) after a look began. A look therefore begins at the moment a
/// look before it ends, when that look took a millisecond or more; else at the moment the clock next stands at.
const ALWAYS_DUE: Duration = Duration::from_nanos(1);
/// The grain of the driven clock: the runtime's timers fire on whole milliseconds, so a timer of [`ALWAYS_DUE`] is due this long
/// after it was armed.
const CLOCK_GRAIN: Duration = Duration::from_millis(1);

/// What a test sets up for the look after a look that stores a copy, while the upload of that copy is held
/// ([`HeldLook::stores_a_copy`]). The tree stops answering in every case: the mark of the stand-in is set again.
#[derive(Clone, Copy)]
struct Next {
    /// The tree gets a file that no copy has: the look after the stored look has something to store.
    changed: bool,
    /// The stand-in lets one more `git add` through ([`pass_files`]): the look after the stored look reads the tree.
    one_add_goes_through: bool,
}

impl Next {
    /// The look after the stored look waits in `git add`, and the tree holds a file that no copy has.
    const HELD: Self = Self {
        changed: true,
        one_add_goes_through: false,
    };
    /// The look after the stored look reads a tree that is as the stored copy has it, and the look after that waits in `git add`.
    const UNCHANGED_THEN_HELD: Self = Self {
        changed: false,
        one_add_goes_through: true,
    };
    /// The look after the stored look waits in `git add`, and the tree is as the stored copy has it.
    const UNCHANGED_AND_HELD: Self = Self {
        changed: false,
        one_add_goes_through: false,
    };
    /// The look after the stored look reads the tree, which holds a file that no copy has, and goes on to store it.
    const CHANGED_AND_READ: Self = Self {
        changed: true,
        one_add_goes_through: true,
    };
}

/// The files of [`held_git`] in the scratch folder `dir`: the mark that makes `git add` wait, and the file the stand-in writes when
/// it waits.
fn held_git_files(dir: &Path) -> (PathBuf, PathBuf) {
    (
        dir.join("the-tree-does-not-answer"),
        dir.join("a-look-waits-in-git-add"),
    )
}

/// The files with which a test lets one `git add` of [`held_git`] through while the mark exists: the test writes the first, and
/// the stand-in gives it the second name when a `git add` took it.
fn pass_files(dir: &Path) -> (PathBuf, PathBuf) {
    (
        dir.join("one-git-add-goes-through"),
        dir.join("a-git-add-went-through"),
    )
}

/// The files with which a test lets a `git add` of [`held_git`] that waits go on while the mark stays: the test writes the first,
/// and the call that waits gives it the second name and goes on as the real git. The next `git add` waits again.
fn release_files(dir: &Path) -> (PathBuf, PathBuf) {
    (
        dir.join("a-waiting-git-add-goes-on"),
        dir.join("a-waiting-git-add-went-on"),
    )
}

/// A stand-in like [`waiting_git`] that also says when it waits: while the mark exists its first `git add` goes through (the
/// worker's reading of the tree before it starts the engine, [`first_add_file`]), and every later `git add` writes the file `held`
/// ([`held_git_files`]) and then waits until the mark is gone, for about a quarter of an hour at the most. A test can let one
/// of those later calls through ([`pass_files`]): the call that finds the first of the two files renames it and is the real git.
/// And it can let a call that waits go on while the mark stays ([`release_files`]). Returns the folder that must lead the search
/// path.
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
    let (release, released) = release_files(dir);
    let waiter = wait.join("sleep");
    let ten = "0 1 2 3 4 5 6 7 8 9";
    let script = format!(
        "#!/bin/sh\nfor a in {ten}; do for b in {ten}; do for c in {ten}; do for d in {ten}; do\nif [ -e '{release}' ]; then '{rename}' '{release}' '{released}'; exec '{real}' \"$@\"; fi\nif [ -e '{mark}' ]; then '{sleep}' 0.1; else exec '{real}' \"$@\"; fi\ndone; done; done; done\nexec '{real}' \"$@\"\n",
        mark = mark.display(),
        release = release.display(),
        released = released.display()
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

/// Keeps the paused clock of the test's runtime where it is. A paused clock moves by itself to the next timer when the runtime has
/// nothing to do, unless a blocking task is outstanding (tokio's rule for a paused clock): this is one that waits until it is let go.
struct ClockHold {
    go: std::sync::mpsc::Sender<()>,
    parked: tokio::task::JoinHandle<()>,
}

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

/// The clock of a test that drives it. It stands while the test waits for real work, and it moves only when the test says so.
struct DrivenClock {
    hold: Option<ClockHold>,
}

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

/// The late lines of the worker's log, in order.
fn late_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct turn snapshot late: "))
        .collect()
}

/// The lines of the worker's log about a look that failed, in order.
fn failed_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct turn snapshot failed: "))
        .collect()
}

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

impl HeldLook {
    fn log(&self) -> String {
        self.d.log.lines().join("\n")
    }

    /// How long the look has run on the driven clock.
    fn has_run(&self) -> Duration {
        DrivenClock::now() - self.began
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
    /// ([`TreeQuestionHeld`]): [`said_late_when_the_tree_has_answered`] builds the path in which a held question comes first,
    /// and holds it. `true` when the log has a late line; the clock still stands at `late_at` then, so the line was written at
    /// the limit on that clock.
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

    /// Everything the worker had made for the run when the clock stood at this moment is with the run when this returns. A
    /// notice is an event of the run: it waits in the worker until the flusher posts it, and a post is real work, so the clock
    /// can pass a moment while a post is under way. The flusher posts once every [`DRIVEN_POSTS_EVERY`], one post after the
    /// other, also when it holds nothing. So two more of its periods pass on the clock, and the test waits, while the clock
    /// stands, until two more posts have arrived: the second of them began after this moment and carried what was left.
    async fn the_run_has_what_was_made(&mut self) {
        let posts = self.d.aws.batches().len();
        let two_periods_on = DrivenClock::now() + DRIVEN_POSTS_EVERY * 2;
        self.clock.runs_to(two_periods_on).await;
        assert!(
            self.clock
                .stands_until(REAL_WORK_WITHIN, || {
                    self.d.aws.batches().len() >= posts + 2
                })
                .await,
            "TRIGGER PRESENT: the flusher did not post twice in two of its periods, so it is not known what the run has; log:\n{}",
            self.log()
        );
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
    /// The look has ended when this returns, so a scene may move the clock next. A look ends when the worker has read the
    /// store's answer to the record of its copy, and that answer can come late in real time; the record alone, which the
    /// store has before it answers, does not show it. The worker writes its save line of the look before it takes the look's
    /// result, and holds the line back when a keep of the run's commits follows, so the line does not show it either. The
    /// store does (`FakeAws::put_answers`): the test waits, while the clock stands, until the store has given one more
    /// answer to a PUT of the record and the worker has closed the connection of every such answer, which it does when
    /// its store has returned. The worker gives no request up meanwhile: its limits run on the clock that stands.
    async fn stores_a_copy(&mut self, took: Duration, next: Next) {
        assert_eq!(
            DrivenClock::now(),
            self.began,
            "the clock stands at the start of the look that stores a copy"
        );
        self.stores_a_copy_at(self.began + took, next).await;
    }

    /// [`Self::stores_a_copy`] for a look that waits in `git add` and may have run for a while on the driven clock already: the
    /// look stores a copy and ends at `end`, and [`Self::began`] is then that moment. Of the time from now to `end`, two fifths
    /// pass while its git works and the rest while it stores.
    async fn stores_a_copy_at(&mut self, end: tokio::time::Instant, next: Next) {
        let from = DrivenClock::now();
        let rest = end - from;
        let in_git = rest * 2 / 5;
        let in_store = rest - in_git;
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
            from + in_git,
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
        self.began = end;
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

/// [`a_look_held_after_a_stored_look`] with two looks that store a copy before the look that waits: they took `first` and then
/// `second` on the driven clock, each began at the moment the look before it ended, and the look that waits began at the moment
/// the second ended. The second look stores the file the test wrote while the first stored its copy.
async fn a_look_held_after_two_stored_looks(
    first: Duration,
    second: Duration,
    set: Duration,
    git_limit: Duration,
    dir: &Path,
) -> HeldLook {
    let mut look = a_look_held_after_a_stored_look(first, set, git_limit, dir).await;
    look.stores_a_copy(second, Next::HELD).await;
    look.a_look_waits_in_git_add("the look after the second stored look")
        .await;
    look.has_stored(2);
    look
}

/// Starts a worker whose looks only its timer starts, and returns when its first look waits in `git add`. The engine of the
/// scenario makes no tool call and then is silent, so no tool call asks for a look, and the tree is the test's own: it holds a
/// file that no copy has. The timer of the worker is `timer`, and `first_tick` is when its first tick is due, counted from the
/// start: `timer` on the grain of the driven clock.
///
/// How the moment is known. The copy task is started before the worker sends the prompt to the engine, so it exists, and has
/// armed its timer at the moment the test began, when the engine has answered the prompt; the clock stands until then. The
/// test then runs the clock to the first tick, and the look begins there: no look ran before it, so nothing holds it back.
async fn a_look_held_on_the_timer(
    timer: Duration,
    first_tick: Duration,
    set: Duration,
    git_limit: Duration,
    dir: &Path,
) -> HeldLook {
    let clock = DrivenClock::stands();
    let first = DrivenClock::now();
    let (mark, held) = held_git_files(dir);
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "600".into())],
        edit_cfg: Some(Box::new(move |c| {
            c.direct.snapshot_interval = Some(timer);
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
    std::fs::write(&mark, "set\n").unwrap();
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    let mut look = HeldLook {
        d,
        clock,
        began: first,
        mark,
        held,
        spacing: timer * 5 + Duration::from_millis(10),
        written: 0,
    };
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || {
                look.d.fake_engine_log().contains("agent_message_chunk")
            })
            .await,
        "TRIGGER PRESENT: the engine did not answer the prompt; the engine's log:\n{}\nlog:\n{}",
        look.d.fake_engine_log(),
        look.log()
    );
    // The tasks of the worker that were ready are polled once more before the clock moves.
    look.clock.stands_for(Duration::from_millis(20)).await;
    assert_eq!(
        DrivenClock::now(),
        first,
        "the clock stood until the engine had answered"
    );
    assert!(
        !look.held.exists() && records(&look.d).is_empty(),
        "TRIGGER PRESENT: no look began before the tick of the timer; copies {:?}; log:\n{}",
        records(&look.d),
        look.log()
    );
    // The worker has read the tree for the first time (it does so before it starts the engine): what the test writes now is in
    // no copy, so the first look has something to store.
    std::fs::write(
        look.d.worktree().join("work.txt"),
        "written before the first look\n",
    )
    .unwrap();
    look.began = first + first_tick;
    look.clock.runs_to(look.began).await;
    look.a_look_waits_in_git_add("the first look, at the first tick of the timer")
        .await;
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

/// A look that stored a copy and took `stored`, then a look that found the tree unchanged and stored nothing, and then the
/// look that waits in `git add`, which began `later` after the stored look ended.
///
/// The timer of this worker is [`ALWAYS_DUE`], so each look begins at the moment the look before it ends, or at the moment the
/// clock next stands at. The look after the stored look begins when that look ends; the stand-in lets its `git add` through
/// ([`pass_files`]) and says so, and the tree is as the stored copy has it. The clock stands until the stand-in has said so, and
/// then jumps once, by `later`. Whether the unchanged look ended before that jump or after it, the look after it begins at the
/// moment the clock then stands at: its tick was due a millisecond after the unchanged look began, and it can begin only when
/// that look has ended. So the unchanged look took `later` at the most, on the worker's clock.
async fn a_look_held_after_a_stored_and_an_unchanged_look(
    stored: Duration,
    later: Duration,
    set: Duration,
    git_limit: Duration,
    dir: &Path,
) -> HeldLook {
    let mut look = a_look_held_on_the_timer(ALWAYS_DUE, CLOCK_GRAIN, set, git_limit, dir).await;
    look.stores_a_copy(stored, Next::UNCHANGED_THEN_HELD).await;
    let (_, went_through) = pass_files(dir);
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || went_through.exists())
            .await,
        "TRIGGER PRESENT: the look after the stored look did not read the tree; copies {:?}; log:\n{}",
        records(&look.d),
        look.log()
    );
    assert_eq!(
        DrivenClock::now(),
        look.began,
        "the clock stood from the end of the stored look until the look after it read the tree"
    );
    assert!(
        !look.held.exists(),
        "TRIGGER PRESENT: no look waits in `git add` before the clock has moved; log:\n{}",
        look.log()
    );
    look.clock.jumps_and_settles(later).await;
    look.began += later;
    look.a_look_waits_in_git_add("the look after the look that found the tree unchanged")
        .await;
    // The look between stored nothing and did not fail: it found the tree unchanged.
    look.has_stored(1);
    look
}

/// A look that stored a copy and took `stored`, then no look for `pause`, and then the look that waits in `git add`.
///
/// The timer of this worker is `stored` and `pause` together, and only the timer starts its looks
/// ([`a_look_held_on_the_timer`]): the first look begins at the first tick, and the tick after it is due `pause` after that
/// look has ended. The spacing of the looks has ended in real time before the stored look ends ([`HeldLook::stores_a_copy`]), so
/// nothing but the tick decides when the next look begins.
async fn a_look_held_after_a_stored_look_and_no_look_for(
    stored: Duration,
    pause: Duration,
    set: Duration,
    git_limit: Duration,
    dir: &Path,
) -> HeldLook {
    let timer = stored + pause;
    let mut look = a_look_held_on_the_timer(timer, timer, set, git_limit, dir).await;
    look.stores_a_copy(stored, Next::HELD).await;
    // One millisecond before the tick no look has begun.
    look.clock.runs_to(look.began + pause - CLOCK_GRAIN).await;
    assert!(
        !look.held.exists(),
        "TRIGGER PRESENT: no look began between the stored look and the tick of the timer; log:\n{}",
        look.log()
    );
    look.began += pause;
    look.clock.runs_to(look.began).await;
    look.a_look_waits_in_git_add("the look at the tick after the stored look")
        .await;
    look.has_stored(1);
    look
}

/// A look that is still held at its late limit H is said late at H: not when everything that is ready has run one millisecond
/// before H, once while the clock stands at H, and not again while it stays held. The worker's line states the numbers the test
/// imposed and drove, and the notice reaches the run through the flusher with its sentence.
///
/// "Not again" is held far past the limit. The git work of the look has [`HELD_PAST_THE_LIMIT`] more than the late limit, and the
/// test stops the clock one, two and three limits past H and a minute past it: at each stop the log has the one late line, no
/// look has failed, and what the run was told about its copies is exactly the late notice ([`HeldLook::the_run_has_what_was_made`]
/// makes that list complete). A worker that says the look late again at every limit, or tells the run a second time seconds
/// later, fails at a stop.
///
/// The look is given up afterwards, at the limit of its git work: the run is told that the copy could not be saved, second.
/// `scene` is what stands before the look, with the limit that follows from it. When the scene names a moment at which a rule
/// other than the worker's would say the look late, the test stops there first: the log has no late line.
async fn said_late_at_its_limit_and_given_up_afterwards(scene: Scene, dir: PathBuf) {
    let Scene {
        limit, set, last, ..
    } = scene;
    let story = scene.story();
    let git_limit = limit + HELD_PAST_THE_LIMIT;
    let mut look = scene.held(git_limit, &dir).await;
    let late_at = look.began + limit;
    let given_up_at = look.began + git_limit;
    let ms = Duration::from_millis(1);

    if let Some(at) = scene.nothing_at {
        // The moment another rule would give: everything that was ready has run, and the look was not said late.
        look.clock.runs_to(look.began + at).await;
        assert!(
            late_lines(&look.d).is_empty(),
            "a look was said late {} ms after it began; its limit is {} ms ({story}; the set limit is {} ms); log:\n{}",
            at.as_millis(),
            limit.as_millis(),
            set.as_millis(),
            look.log()
        );
        assert!(failed_lines(&look.d).is_empty(), "{}", look.log());
    }

    // One millisecond before the limit: everything that was ready has run, and the look was not said late.
    look.clock.runs_to(late_at - ms).await;
    assert!(
        late_lines(&look.d).is_empty(),
        "a look was said late before its limit: it began {} ms ago and its limit is {} ms (the set limit of {} ms, or three times \
         what the last look that stored a copy took when that is longer; {story}); log:\n{}",
        look.has_run().as_millis(),
        limit.as_millis(),
        set.as_millis(),
        look.log()
    );
    assert!(failed_lines(&look.d).is_empty(), "{}", look.log());

    // At the limit, while the clock stands there: the look is said late, once, and the line states the time of the limit.
    assert!(
        look.is_said_late_at(late_at).await,
        "a look that is still held at its late limit of {} ms was not said late at that moment ({story}): the clock stands at \
         the limit and the log has no late line; log:\n{}",
        limit.as_millis(),
        look.log()
    );
    let said_after = look.has_run();
    let said = late_lines(&look.d);
    assert_eq!(
        said.len(),
        1,
        "a look that is held at its late limit is said late once; log:\n{}",
        look.log()
    );
    assert!(
        said[0].contains("direct turn snapshot late: reading the work tree has taken "),
        "{said:?}"
    );
    let Some(stated) = said_late(said[0].trim_end()) else {
        panic!("the worker's late line does not state its numbers: {said:?}");
    };
    println!(
        "DRIVEN: {story}; the look was held; the test saw its late line {} ms after it began, with the clock standing; the line \
         states {} ms taken, a limit of {} ms, a set limit of {} ms and a last stored copy of {:?}",
        said_after.as_millis(),
        stated.taken.as_millis(),
        stated.limit.as_millis(),
        stated.set.as_millis(),
        stated.last
    );
    assert_eq!(
        (stated.taken, stated.limit, stated.set, stated.last),
        (limit, limit, set, last),
        "the worker's late line states the time the look had taken when it was said late, the limit that applied, the set limit and \
         what the last stored look took, or that none was stored (taken, limit, set, last); the test imposed and drove the second \
         of each pair ({story}): {said:?}"
    );

    // While the look stays held it is not said late again and the run is told nothing else: one, two and three limits past the
    // limit, and a minute past it. The notice reaches the run through the flusher, with its sentence, and at every stop it is
    // the one notice about a copy the run has.
    for past in [limit, limit * 2, limit * 3, A_MINUTE] {
        look.clock.runs_to(late_at + past).await;
        look.the_run_has_what_was_made().await;
        let lines = late_lines(&look.d);
        assert_eq!(
            lines.len(),
            1,
            "a look that stays held is said late once: {} ms past its late limit of {} ms the log has {} late lines; log:\n{}",
            past.as_millis(),
            limit.as_millis(),
            lines.len(),
            look.log()
        );
        assert_eq!(
            copy_notices(&look.d),
            [LATE_TREE.to_string()],
            "what the run was told about its copies {} ms past the late limit of {} ms of a look that stays held: the late notice, \
             once; log:\n{}",
            past.as_millis(),
            limit.as_millis(),
            look.log()
        );
        assert!(
            failed_lines(&look.d).is_empty(),
            "TRIGGER PRESENT: no look has failed {} ms past the late limit; log:\n{}",
            past.as_millis(),
            look.log()
        );
        assert!(
            stand_in_sleeps(std::process::id()),
            "TRIGGER PRESENT: the look is still held in `git add` {} ms past its late limit; log:\n{}",
            past.as_millis(),
            look.log()
        );
    }
    println!(
        "DRIVEN: the look stayed held; {} ms past its late limit of {} ms the log had one late line and the run had been told \
         {:?} about its copies",
        (look.has_run() - limit).as_millis(),
        limit.as_millis(),
        copy_notices(&look.d)
    );

    // Up to the millisecond before it is given up the look is held, said late once, and has not failed.
    look.clock.runs_to(given_up_at - ms).await;
    assert_eq!(late_lines(&look.d).len(), 1, "{}", look.log());
    assert!(failed_lines(&look.d).is_empty(), "{}", look.log());
    assert!(
        stand_in_sleeps(std::process::id()),
        "TRIGGER PRESENT: the look is still held in `git add`; log:\n{}",
        look.log()
    );
    assert_eq!(copy_notices(&look.d), [LATE_TREE.to_string()]);

    // Afterwards the look is given up, at the limit of its git work.
    look.clock.runs_to(given_up_at).await;
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || !failed_lines(&look.d).is_empty())
            .await,
        "the look was not given up at the limit of its git work; log:\n{}",
        look.log()
    );
    let failed = failed_lines(&look.d);
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0].contains("git add: given up before it had ended"),
        "{failed:?}"
    );
    look.clock.runs_to(given_up_at + DRIVEN_POSTS_EVERY).await;
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || copy_notices(&look.d).len() > 1)
            .await,
        "the run was not told that the copy could not be saved; log:\n{}",
        look.log()
    );
    assert_eq!(
        copy_notices(&look.d),
        [LATE_TREE.to_string(), NOT_SAVED.to_string()]
    );
    // And the look that was given up left one late line, the one of its limit.
    assert_eq!(late_lines(&look.d).len(), 1, "{}", look.log());
    look.ends();
}

/// A look that is given up before its late limit H is not said late: it fails as a look whose git was given up, the run is told
/// that the copy could not be saved and nothing else, and no late line is written when H passes, up to one period of the flusher
/// after the look was given up. `scene` is what stands before the look, with the limit that follows from it; the git work of a
/// look has 20 ms less than that limit.
async fn given_up_before_its_late_limit_and_never_said_late(scene: Scene, dir: PathBuf) {
    let limit = scene.limit;
    let story = scene.story();
    let git_limit = limit - Duration::from_millis(20);
    let mut look = scene.held(git_limit, &dir).await;
    let given_up_at = look.began + git_limit;
    let ms = Duration::from_millis(1);

    // One millisecond before the limit of its git work the look is neither late nor given up.
    look.clock.runs_to(given_up_at - ms).await;
    assert!(
        late_lines(&look.d).is_empty(),
        "a look was said late {} ms before its limit of {} ms ({story}); log:\n{}",
        (limit - look.has_run()).as_millis(),
        limit.as_millis(),
        look.log()
    );
    assert!(failed_lines(&look.d).is_empty(), "{}", look.log());

    // At that limit it is given up.
    look.clock.runs_to(given_up_at).await;
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || !failed_lines(&look.d).is_empty())
            .await,
        "the look was not given up at the limit of its git work; log:\n{}",
        look.log()
    );
    let failed = failed_lines(&look.d);
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0].contains("git add: given up before it had ended"),
        "{failed:?}"
    );
    assert!(late_lines(&look.d).is_empty(), "{}", look.log());

    // The late limit passes, and one period of the flusher after the give-up: the look that was given up is not said late.
    look.clock.runs_to(given_up_at + DRIVEN_POSTS_EVERY).await;
    assert!(
        late_lines(&look.d).is_empty(),
        "a look that was given up {} ms before its late limit was said late all the same; log:\n{}",
        (limit - git_limit).as_millis(),
        look.log()
    );
    println!(
        "DRIVEN: {story}; the look was held; it was given up {} ms after it began, {} ms before its late limit, and the log has \
         no late line {} ms after it began",
        git_limit.as_millis(),
        (limit - git_limit).as_millis(),
        look.has_run().as_millis()
    );
    // The run was told one thing about its copies: that the copy could not be saved.
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || !copy_notices(&look.d).is_empty())
            .await,
        "the run was not told that the copy could not be saved; log:\n{}",
        look.log()
    );
    assert_eq!(copy_notices(&look.d), [NOT_SAVED.to_string()]);
    look.ends();
}

/// What can stand between the late timer of a look and its late line, held by the test: the worker's question to the file system
/// of the work tree ([`TreeQuestionHeld`]).
///
/// When the worker asks. Every look asks the tree once before its git starts. While the look's git works the copy task asks
/// again, once every late limit that was set (the two are one setting in a test), counted from the start of the look, from the
/// first period on and whether or not the look was asked to end.
/// So a question can be under way when the late
/// timer fires, and the copy task waits for its answer before it goes on to the line. That is the one path of the worker in
/// which blocking work stands between the timer and the line, and this test builds it in a look that was asked to end: it asks
/// the worker to end one millisecond before the last question at or before the late limit H is due, takes the threads, moves
/// the clock to that question and on to H, and looks while the clock stands at H. `imposed` is what the look that stored the
/// last copy took, with the late limit that follows from it.
///
/// - When the question was asked before H (three times the last stored look applies), the copy task waits for the answer when
///   its late timer fires: there is no late line, in every run, for as long as the question is held.
/// - When the question is due at H itself (the set limit applies), the worker writes the line first: the late line comes before
///   the question in the order of the arms of `watched_look`, so the line is there at once and the question holds
///   nothing back. This test still takes either order, as it did when the worker took the two in either order: had the
///   worker asked first, there would be no late line while the question is held. The order itself is held by
///   `b923_a_look_late_at_400_ms_with_a_question_every_400_ms_...` and `b923_twelve_looks_in_a_row_...` in
///   `tests/b923_tree_question.rs`.
/// - When the test gives the threads back, the tree is asked and answers, the line is there (the wait for it is the test's
///   patience, [`HeldLook::is_said_late_at`]), it is the one late line, and it states the driven numbers exactly: the clock still
///   stands at H, so the time the worker stood behind the question is no part of the time the line states.
///
/// What this holds: whatever real time the answer of the tree takes, the line is written at the limit on the worker's clock and
/// states that limit, and no line is written while the answer is outstanding. What it does not hold, and no test of this file
/// does: how much real time the worker spends between its late timer and its late line.
async fn said_late_when_the_tree_has_answered(imposed: Imposed, dir: PathBuf) {
    let Imposed { last, limit, set } = imposed;
    let mut look =
        a_look_held_after_a_stored_look(last, set, limit + HELD_PAST_THE_LIMIT, &dir).await;
    let late_at = look.began + limit;
    let ms = Duration::from_millis(1);
    // The last question to the tree at or before the late limit.
    let questions = u32::try_from(limit.as_millis() / set.as_millis()).unwrap();
    let asked_after = set * questions;
    let asked_at = look.began + asked_after;
    let asked_before_the_limit = asked_at < late_at;

    // One millisecond before that question everything that was ready has run, no look is late, and no question is outstanding.
    look.clock.runs_to(asked_at - ms).await;
    assert!(
        late_lines(&look.d).is_empty() && failed_lines(&look.d).is_empty(),
        "a look was said late or failed before its late limit of {} ms; log:\n{}",
        limit.as_millis(),
        look.log()
    );
    // The worker is asked to end, and the copy task takes the request up. It asks the tree while the look reads it with or
    // without the request; the scene keeps the request, so the held look is one that was asked to end. The look in
    // flight keeps the time a request to end gives it, which is far longer than these tests run.
    assert!(
        !look.log().contains("direct turn session ending: "),
        "TRIGGER PRESENT: the worker was not asked to end before the test asks; log:\n{}",
        look.log()
    );
    let _ = look.d.app.ending.ask();
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || {
                look.log()
                    .contains("direct turn session ending: a copy of the work is saved at once")
            })
            .await,
        "TRIGGER PRESENT: the copy task did not take the request to end up while the look was held; log:\n{}",
        look.log()
    );
    assert_eq!(
        DrivenClock::now(),
        asked_at - ms,
        "the clock stood while the copy task took the request up"
    );
    // From here a question to the tree waits: the test has every thread it could run on.
    let held = TreeQuestionHeld::from_now().await;
    look.clock.jumps_and_settles(ms).await;
    if asked_before_the_limit {
        // The question was asked and is held; the clock goes on to the late limit, where the late timer fires.
        look.clock.jumps_and_settles(late_at - asked_at).await;
    }
    assert_eq!(
        DrivenClock::now(),
        late_at,
        "the clock stands at the late limit"
    );
    let at_once = late_lines(&look.d).len();
    assert!(
        at_once <= 1,
        "a look was said late more than once at its late limit; log:\n{}",
        look.log()
    );
    if asked_before_the_limit {
        assert_eq!(
            at_once,
            0,
            "the worker asked the tree {} ms before the late limit of {} ms and the question is held: its late timer has fired and \
             the copy task waits for the answer, so it writes no late line before the tree has answered; log:\n{}",
            (limit - asked_after).as_millis(),
            limit.as_millis(),
            look.log()
        );
    }
    let asked_first = at_once == 0;
    if asked_first {
        // While the question is held the line does not come, however long the test looks.
        let came = held
            .held_for(QUESTION_HELD_FOR, || !late_lines(&look.d).is_empty())
            .await;
        assert!(
            !came,
            "a late line was written while the worker's question to the tree was held: the copy task waits for the answer before it \
             goes on; log:\n{}",
            look.log()
        );
    }
    assert!(
        held.holds(),
        "TRIGGER PRESENT: a thread for blocking work was free while the test held them, so the question was not held; log:\n{}",
        look.log()
    );
    assert_eq!(
        DrivenClock::now(),
        late_at,
        "the clock stood at the late limit while the question was held"
    );

    // The tree is asked and answers: the line is there, once, and states the limit as the time the look had taken.
    held.released().await;
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || !late_lines(&look.d).is_empty())
            .await,
        "a look that is still held at its late limit of {} ms was not said late when the tree had answered; log:\n{}",
        limit.as_millis(),
        look.log()
    );
    assert_eq!(
        DrivenClock::now(),
        late_at,
        "the clock stood at the late limit until the line came"
    );
    let said = late_lines(&look.d);
    assert_eq!(
        said.len(),
        1,
        "a look that is held at its late limit is said late once; log:\n{}",
        look.log()
    );
    let Some(stated) = said_late(said[0].trim_end()) else {
        panic!("the worker's late line does not state its numbers: {said:?}");
    };
    println!(
        "DRIVEN: the last stored look took {} ms and the next look was held and asked to end; the question the worker asks the \
         tree {} ms after the look began was held by the test; with the clock standing at the late limit of {} ms the worker had \
         {}; when the tree had answered, the line stated {} ms taken, a limit of {} ms, a set limit of {} ms and a last stored copy \
         of {:?}",
        last.as_millis(),
        asked_after.as_millis(),
        limit.as_millis(),
        if asked_first {
            "written no late line, and wrote none while the question was held"
        } else {
            "written its late line before it asked"
        },
        stated.taken.as_millis(),
        stated.limit.as_millis(),
        stated.set.as_millis(),
        stated.last
    );
    assert_eq!(
        (stated.taken, stated.limit, stated.set, stated.last),
        (limit, limit, set, Some(last)),
        "the worker's late line states the time the look had taken when it was said late, the limit that applied, the set limit and \
         what the last stored look took (taken, limit, set, last); the clock stood at the limit while the tree was asked: {said:?}"
    );
    look.ends();
}

/// What the run has been told about its copies at a moment of a look that is held: nothing one millisecond before the late limit
/// (`at_the_limit` false), the late notice and nothing else at the limit (`at_the_limit` true). A notice is an event of the run,
/// which waits in the worker until the flusher posts it, so the log alone does not show when a notice was made. With the clock
/// standing at the moment, the test asks the worker to end: the flusher then posts what it holds and gives the service notice of
/// the end after it. When that notice has arrived, every event the worker had made before the request is with the run. `scene`
/// is what stands before the look, with the limit that follows from it.
async fn told_at_a_moment_of_a_held_look(scene: Scene, at_the_limit: bool, dir: PathBuf) {
    let limit = scene.limit;
    let story = scene.story();
    let git_limit = limit + Duration::from_millis(300);
    let mut look = scene.held(git_limit, &dir).await;
    let late_at = look.began + limit;
    if at_the_limit {
        assert!(
            look.is_said_late_at(late_at).await,
            "a look that is still held at its late limit of {} ms was not said late at that moment; log:\n{}",
            limit.as_millis(),
            look.log()
        );
    } else {
        look.clock.runs_to(late_at - Duration::from_millis(1)).await;
        assert!(
            late_lines(&look.d).is_empty(),
            "a look was said late before its limit of {} ms; log:\n{}",
            limit.as_millis(),
            look.log()
        );
    }
    let asked_after = look.has_run();
    assert!(
        look.d.aws.ending_notices().is_empty(),
        "TRIGGER PRESENT: the service has no notice of an end before the test asks for one"
    );
    let _ = look.d.app.ending.ask();
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || !look.d.aws.ending_notices().is_empty())
            .await,
        "TRIGGER PRESENT: the notice of the end did not arrive, so it is not known what the worker had made before it; log:\n{}",
        look.log()
    );
    assert_eq!(
        look.has_run(),
        asked_after,
        "the clock stood while the flusher posted what it held"
    );
    let told = copy_notices(&look.d);
    println!(
        "DRIVEN: {story}; the look was held; {} ms after it began (its late limit is {} ms) the run had been told {told:?} about \
         its copies",
        asked_after.as_millis(),
        limit.as_millis()
    );
    if at_the_limit {
        assert_eq!(
            told,
            [LATE_TREE.to_string()],
            "at the late limit of {} ms of a look that is held the worker has made the late notice, and no other notice about a copy \
             ({story})",
            limit.as_millis()
        );
    } else {
        assert_eq!(
            told,
            Vec::<String>::new(),
            "the run was told about a copy before the late limit of {} ms of the look that is held ({story})",
            limit.as_millis()
        );
    }
    look.ends();
}

/// A look after a look that was given up is late against the last look that STORED a copy, and the run is told nothing more.
///
/// The scene, with every number written out. The late limit that is set is 400 ms and the git work of a look has 1,000 ms. A
/// look stores a copy and takes 200 ms: the late limit of the looks after it is 600 ms. The look after it waits in `git add`: it
/// is said late 600 ms after it began (the first late line, and the run is told) and given up 1,000 ms after it began (the run
/// is told that the copy could not be saved). The timer of this worker is [`ALWAYS_DUE`], so the next look begins at the moment
/// that look is given up; it waits in `git add` too.
///
/// What is held about that look. Its limit is still 600 ms, three times the 200 ms of the last look that stored a copy: the
/// look that was given up stored none, so it does not count, and it does not clear what the stored look took. So the log has
/// no second late line 400 ms after the look began (the limit that is set, which a worker that forgot the stored look would
/// apply) and none one millisecond before 600 ms; with the clock standing at 600 ms it has the second late line, which states
/// 600 ms taken, a limit of 600 ms, the set limit of 400 ms and a last stored copy of 200 ms. A worker that counted the look
/// that was given up (1,000 ms) would write that line at 3,000 ms. And the run is told nothing about it: a failure was said
/// since the last copy was stored, so what the run has been told about its copies is still the late notice of the look before
/// and that its copy could not be saved, each once.
async fn late_after_a_look_that_was_given_up(dir: PathBuf) {
    let stored = Duration::from_millis(200);
    let set = HANG_LATE_LIMIT;
    let limit = Duration::from_millis(600);
    let git_limit = Duration::from_millis(1_000);
    let ms = Duration::from_millis(1);
    let mut look = a_look_held_on_the_timer(ALWAYS_DUE, CLOCK_GRAIN, set, git_limit, &dir).await;
    look.stores_a_copy(stored, Next::HELD).await;
    look.a_look_waits_in_git_add("the look after the stored look")
        .await;
    look.has_stored(1);

    // The look after the stored look: said late at its limit, and given up at the limit of its git work.
    let given_up_at = look.began + git_limit;
    look.clock.runs_to(look.began + limit - ms).await;
    assert!(late_lines(&look.d).is_empty(), "{}", look.log());
    assert!(
        look.is_said_late_at(look.began + limit).await,
        "TRIGGER PRESENT: the look after the stored look was not said late at its limit of {} ms; log:\n{}",
        limit.as_millis(),
        look.log()
    );
    // The stand-in says again when the next look waits.
    std::fs::remove_file(&look.held).unwrap();
    look.clock.runs_to(given_up_at - ms).await;
    assert!(
        failed_lines(&look.d).is_empty() && late_lines(&look.d).len() == 1,
        "TRIGGER PRESENT: one millisecond before the limit of its git work the look was said late once and had not failed; \
         log:\n{}",
        look.log()
    );
    look.clock.runs_to(given_up_at).await;
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || !failed_lines(&look.d).is_empty())
            .await,
        "TRIGGER PRESENT: the look was not given up at the limit of its git work; log:\n{}",
        look.log()
    );
    let failed = failed_lines(&look.d);
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0].contains("git add: given up before it had ended"),
        "TRIGGER PRESENT: the look before the look that waits was given up: {failed:?}"
    );

    // The look after the look that was given up begins at that moment and waits in `git add`.
    look.began = given_up_at;
    look.a_look_waits_in_git_add("the look after the look that was given up")
        .await;
    assert_eq!(
        records(&look.d).len(),
        1,
        "TRIGGER PRESENT: one look stored a copy; log:\n{}",
        look.log()
    );
    let late_at = look.began + limit;

    // At the limit that is set, 400 ms: no second late line.
    look.clock.runs_to(look.began + set).await;
    assert_eq!(
        late_lines(&look.d).len(),
        1,
        "a look after a look that was given up was said late {} ms after it began; its limit is {} ms, three times the {} ms \
         the last look that stored a copy took; log:\n{}",
        set.as_millis(),
        limit.as_millis(),
        stored.as_millis(),
        look.log()
    );
    // One millisecond before its limit: none.
    look.clock.runs_to(late_at - ms).await;
    assert_eq!(
        late_lines(&look.d).len(),
        1,
        "a look after a look that was given up was said late before its limit of {} ms; log:\n{}",
        limit.as_millis(),
        look.log()
    );
    // At its limit, while the clock stands there: the second late line of the log.
    assert!(
        look.has_late_lines_at(late_at, 2).await,
        "a look after a look that was given up was not said late at its limit of {} ms (three times the {} ms the last look \
         that stored a copy took): the clock stands at the limit and the log has no second late line; log:\n{}",
        limit.as_millis(),
        stored.as_millis(),
        look.log()
    );
    let said = late_lines(&look.d);
    assert_eq!(said.len(), 2, "{}", look.log());
    let Some(stated) = said_late(said[1].trim_end()) else {
        panic!("the worker's late line does not state its numbers: {said:?}");
    };
    println!(
        "DRIVEN: a look stored a copy and took {} ms, the look after it was given up after {} ms, and the look after that was \
         held; the test saw its late line {} ms after it began, with the clock standing; the line states {} ms taken, a limit \
         of {} ms, a set limit of {} ms and a last stored copy of {:?}",
        stored.as_millis(),
        git_limit.as_millis(),
        look.has_run().as_millis(),
        stated.taken.as_millis(),
        stated.limit.as_millis(),
        stated.set.as_millis(),
        stated.last
    );
    assert_eq!(
        (stated.taken, stated.limit, stated.set, stated.last),
        (limit, limit, set, Some(stored)),
        "the late line of a look after a look that was given up states the time the look had taken, the limit that applied, \
         the set limit and what the last look that stored a copy took (taken, limit, set, last): {said:?}"
    );

    // The run is told nothing about it: a failure was said since the last copy was stored.
    look.the_run_has_what_was_made().await;
    assert_eq!(
        copy_notices(&look.d),
        [LATE_TREE.to_string(), NOT_SAVED.to_string()],
        "what the run was told about its copies when a look after a look that was given up was said late: the late notice of \
         the look before and that its copy could not be saved, each once; log:\n{}",
        look.log()
    );
    assert_eq!(late_lines(&look.d).len(), 2, "{}", look.log());
    assert_eq!(failed_lines(&look.d).len(), 1, "{}", look.log());
    look.ends();
}

/// A look that was said late and then stored its copy counts in full as the last stored look, and its copy lets the run be told
/// again: the late notice is made once until a copy is stored.
///
/// The first look of the worker is held: no look stored a copy before it, so it is said late at the set limit of 400 ms and the
/// run is told. Then it stores its copy and ends at 500 ms. The look after it begins at that moment and is held: its limit is
/// three times those 500 ms, 1,500 ms. The log has no second late line 400 ms after it began (a worker that does not count a look
/// that was said late applies the set limit) and none at 1,499 ms; the second line comes at 1,500 ms and states 1,500, 1,500, 400
/// and 500 ms. The run has then been told twice that a copy is late; a worker that keeps the flag of the late notice after a
/// stored copy tells it once.
async fn told_again_after_a_late_look_that_stored_a_copy(dir: PathBuf) {
    let set = HANG_LATE_LIMIT;
    let stored = Duration::from_millis(500);
    let limit = Duration::from_millis(1_500);
    let git_limit = limit + HELD_PAST_THE_LIMIT;
    let ms = Duration::from_millis(1);
    let mut look = the_first_look_held(set, git_limit, &dir).await;

    // The first look: said late at the set limit, and the run is told.
    look.clock.runs_to(look.began + set - ms).await;
    assert!(late_lines(&look.d).is_empty(), "{}", look.log());
    assert!(
        look.is_said_late_at(look.began + set).await,
        "TRIGGER PRESENT: the first look was not said late at the set limit of {} ms; log:\n{}",
        set.as_millis(),
        look.log()
    );
    let said = late_lines(&look.d);
    let Some(first) = said_late(said[0].trim_end()) else {
        panic!("the worker's late line does not state its numbers: {said:?}");
    };
    assert_eq!(
        (first.taken, first.limit, first.set, first.last),
        (set, set, set, None),
        "TRIGGER PRESENT: the first look of the worker was said late at the set limit, with no stored copy before it: {said:?}"
    );

    // It stores its copy and ends at 500 ms; the look after it begins then and waits in `git add`.
    let end = look.began + stored;
    look.stores_a_copy_at(end, Next::HELD).await;
    look.a_look_waits_in_git_add("the look after the look that was said late and stored a copy")
        .await;
    assert!(
        records(&look.d).len() == 1
            && late_lines(&look.d).len() == 1
            && failed_lines(&look.d).is_empty(),
        "TRIGGER PRESENT: one look stored a copy, it was said late once, and no look failed; copies {:?}; log:\n{}",
        records(&look.d),
        look.log()
    );
    let late_at = look.began + limit;

    // At the set limit, 400 ms: no second late line.
    look.clock.runs_to(look.began + set).await;
    assert_eq!(
        late_lines(&look.d).len(),
        1,
        "a look was said late {} ms after it began; the look before it was said late and then stored a copy that took {} ms, so \
         its limit is three times that, {} ms; log:\n{}",
        set.as_millis(),
        stored.as_millis(),
        limit.as_millis(),
        look.log()
    );
    // One millisecond before its limit: none.
    look.clock.runs_to(late_at - ms).await;
    assert_eq!(
        late_lines(&look.d).len(),
        1,
        "a look was said late before its limit of {} ms; log:\n{}",
        limit.as_millis(),
        look.log()
    );
    // At its limit, while the clock stands there: the second late line of the log.
    assert!(
        look.has_late_lines_at(late_at, 2).await,
        "a look after a stored look of {} ms that had been said late was not said late at its limit of {} ms: the clock \
         stands at the limit and the log has no second late line; log:\n{}",
        stored.as_millis(),
        limit.as_millis(),
        look.log()
    );
    let said = late_lines(&look.d);
    assert_eq!(said.len(), 2, "{}", look.log());
    let Some(stated) = said_late(said[1].trim_end()) else {
        panic!("the worker's late line does not state its numbers: {said:?}");
    };
    println!(
        "DRIVEN: the first look was said late at {} ms and stored a copy at {} ms; the look after it was held; the test saw its \
         late line {} ms after it began, with the clock standing; the line states {} ms taken, a limit of {} ms, a set limit of \
         {} ms and a last stored copy of {:?}",
        set.as_millis(),
        stored.as_millis(),
        look.has_run().as_millis(),
        stated.taken.as_millis(),
        stated.limit.as_millis(),
        stated.set.as_millis(),
        stated.last
    );
    assert_eq!(
        (stated.taken, stated.limit, stated.set, stated.last),
        (limit, limit, set, Some(stored)),
        "the late line of a look after a stored look that had been said late states the time the look had taken, the limit \
         that applied, the set limit and what the last look that stored a copy took (taken, limit, set, last): {said:?}"
    );

    // The run was told twice: a copy was stored between the two late looks.
    look.the_run_has_what_was_made().await;
    assert_eq!(
        copy_notices(&look.d),
        [LATE_TREE.to_string(), LATE_TREE.to_string()],
        "what the run was told about its copies: the late notice of the first look, and after its stored copy the late notice \
         of the look after it; log:\n{}",
        look.log()
    );
    assert_eq!(late_lines(&look.d).len(), 2, "{}", look.log());
    assert!(failed_lines(&look.d).is_empty(), "{}", look.log());
    look.ends();
}

/// The late notice is made once until a copy is stored, not once per look. A look stored a copy and took 200 ms, and the tree
/// stays as that copy has it. The look after it is held: it is said late at 600 ms, and the run is told. Then it goes on, finds
/// the tree unchanged and stores nothing, and the look after it begins at that moment and is held while the tree still does not
/// answer: its limit is 600 ms as well, and its late line states 600, 600, 400 and 200 ms. The run has been told once; a worker
/// that clears the flag of the late notice when a look begins tells it twice.
///
/// The timer of this worker is [`ALWAYS_DUE`]: after the look that found the tree unchanged, no spacing holds the next look back,
/// and its tick is due, so it begins at the moment the clock stands at when that look ends ([`a_look_held_on_the_timer`]).
async fn told_once_over_two_late_looks_with_no_stored_copy_between(dir: PathBuf) {
    let set = HANG_LATE_LIMIT;
    let stored = Duration::from_millis(200);
    let limit = Duration::from_millis(600);
    let git_limit = limit + HELD_PAST_THE_LIMIT;
    let ms = Duration::from_millis(1);
    let mut look = a_look_held_on_the_timer(ALWAYS_DUE, CLOCK_GRAIN, set, git_limit, &dir).await;
    look.stores_a_copy(stored, Next::UNCHANGED_AND_HELD).await;
    look.a_look_waits_in_git_add("the look after the stored look")
        .await;
    look.has_stored(1);

    // The look after the stored look: said late at 600 ms, and the run is told.
    look.clock.runs_to(look.began + limit - ms).await;
    assert!(late_lines(&look.d).is_empty(), "{}", look.log());
    assert!(
        look.is_said_late_at(look.began + limit).await,
        "TRIGGER PRESENT: the look after the stored look was not said late at its limit of {} ms; log:\n{}",
        limit.as_millis(),
        look.log()
    );
    look.the_run_has_what_was_made().await;
    assert_eq!(
        copy_notices(&look.d),
        [LATE_TREE.to_string()],
        "TRIGGER PRESENT: the run was told once that the copy of the look after the stored look is late; log:\n{}",
        look.log()
    );

    // That look goes on while the mark stays: it reads a tree that is as the stored copy has it and stores nothing, and the look
    // after it begins at the moment the clock stands at and waits in `git add`.
    let (release, released) = release_files(&dir);
    std::fs::remove_file(&look.held).unwrap();
    std::fs::write(&release, "go on\n").unwrap();
    look.began = DrivenClock::now();
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || released.exists())
            .await,
        "TRIGGER PRESENT: the look that was said late did not go on; log:\n{}",
        look.log()
    );
    look.a_look_waits_in_git_add("the look after the late look that stored nothing")
        .await;
    assert!(
        records(&look.d).len() == 1
            && late_lines(&look.d).len() == 1
            && failed_lines(&look.d).is_empty(),
        "TRIGGER PRESENT: the late look stored nothing and did not fail: it found the tree unchanged; copies {:?}; log:\n{}",
        records(&look.d),
        look.log()
    );
    let late_at = look.began + limit;

    // One millisecond before its limit: no second late line.
    look.clock.runs_to(late_at - ms).await;
    assert_eq!(
        late_lines(&look.d).len(),
        1,
        "a look after a late look that stored nothing was said late before its limit of {} ms; log:\n{}",
        limit.as_millis(),
        look.log()
    );
    // At its limit, while the clock stands there: the second late line of the log.
    assert!(
        look.has_late_lines_at(late_at, 2).await,
        "a look after a late look that stored nothing was not said late at its limit of {} ms (three times the {} ms the last \
         look that stored a copy took): the clock stands at the limit and the log has no second late line; log:\n{}",
        limit.as_millis(),
        stored.as_millis(),
        look.log()
    );
    let said = late_lines(&look.d);
    assert_eq!(said.len(), 2, "{}", look.log());
    let Some(stated) = said_late(said[1].trim_end()) else {
        panic!("the worker's late line does not state its numbers: {said:?}");
    };
    println!(
        "DRIVEN: a look stored a copy and took {} ms, the look after it was said late and found the tree unchanged, and the look \
         after that was held; the test saw its late line {} ms after it began, with the clock standing; the line states {} ms \
         taken, a limit of {} ms, a set limit of {} ms and a last stored copy of {:?}",
        stored.as_millis(),
        look.has_run().as_millis(),
        stated.taken.as_millis(),
        stated.limit.as_millis(),
        stated.set.as_millis(),
        stated.last
    );
    assert_eq!(
        (stated.taken, stated.limit, stated.set, stated.last),
        (limit, limit, set, Some(stored)),
        "the late line of a look after a late look that stored nothing states the time the look had taken, the limit that \
         applied, the set limit and what the last look that stored a copy took (taken, limit, set, last): {said:?}"
    );

    // The run is told nothing more: no copy was stored since its late notice.
    look.the_run_has_what_was_made().await;
    assert_eq!(
        copy_notices(&look.d),
        [LATE_TREE.to_string()],
        "what the run was told about its copies over two late looks with no stored copy between them: the late notice, once; \
         log:\n{}",
        look.log()
    );
    assert_eq!(late_lines(&look.d).len(), 2, "{}", look.log());
    assert!(failed_lines(&look.d).is_empty(), "{}", look.log());
    look.ends();
}

/// A look that is late while it stores its copy is said late as storing the copy. A look stored a copy and took 200 ms. The look
/// after it begins at the next tick of the timer, reads the tree, which holds a file that no copy has, and stores its bundle; the
/// store holds that upload. Its limit is 600 ms, counted from its start: the log has no late line at 599 ms; at 600 ms it has one,
/// which begins "direct turn snapshot late: storing the copy has taken" and states 600, 600, 400 and 200 ms, and the run is told
/// once that storing the copy has not ended. A worker that names the part of the tree for a look in its store fails.
///
/// The store holds one upload at a time, and its hold is armed again only after the upload of the stored look has gone on. So the
/// look after it must not begin before that: only the timer starts the looks of this worker ([`a_look_held_on_the_timer`]), its
/// timer is the stored look and a pause of 100 ms together, and the look begins at that tick.
async fn said_late_while_it_stores(dir: PathBuf) {
    let set = HANG_LATE_LIMIT;
    let stored = Duration::from_millis(200);
    let pause = Duration::from_millis(100);
    let limit = Duration::from_millis(600);
    let git_limit = limit + HELD_PAST_THE_LIMIT;
    let ms = Duration::from_millis(1);
    let timer = stored + pause;
    let mut look = a_look_held_on_the_timer(timer, timer, set, git_limit, &dir).await;
    look.stores_a_copy(stored, Next::CHANGED_AND_READ).await;
    look.has_stored(1);
    // The stored look has ended: its upload went on, and the worker has read the store's answer to its record, so what it took
    // is taken before the clock moves again ([`HeldLook::stores_a_copy`]). The store holds the next upload of a bundle.
    assert!(
        !look.d.aws.put_body_held(),
        "TRIGGER PRESENT: the upload of the stored look went on; log:\n{}",
        look.log()
    );
    look.d.aws.hold_put_body("snapshot.bundle", 0);
    // One millisecond before the tick no look has begun.
    let (_, went_through) = pass_files(&dir);
    look.clock.runs_to(look.began + pause - ms).await;
    assert!(
        !went_through.exists() && !look.d.aws.put_body_held(),
        "TRIGGER PRESENT: no look began between the stored look and the tick of the timer; log:\n{}",
        look.log()
    );
    // At the tick the look begins, reads the tree and stores its bundle, and the store holds the upload.
    look.began += pause;
    look.clock.runs_to(look.began).await;
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || look.d.aws.put_body_held())
            .await,
        "TRIGGER PRESENT: the look at the tick did not reach the store while the clock stood; copies {:?}; log:\n{}",
        records(&look.d),
        look.log()
    );
    assert_eq!(
        DrivenClock::now(),
        look.began,
        "the clock stood until the look at the tick was in its store"
    );
    assert!(
        went_through.exists()
            && records(&look.d).len() == 1
            && late_lines(&look.d).is_empty()
            && failed_lines(&look.d).is_empty(),
        "TRIGGER PRESENT: the look at the tick read the tree, and before it one look stored a copy and no look was late or \
         failed; copies {:?}; log:\n{}",
        records(&look.d),
        look.log()
    );
    let late_at = look.began + limit;

    // One millisecond before its limit: no late line.
    look.clock.runs_to(late_at - ms).await;
    assert!(
        late_lines(&look.d).is_empty(),
        "a look that stores its copy was said late before its limit of {} ms (three times the {} ms the last stored look took); \
         log:\n{}",
        limit.as_millis(),
        stored.as_millis(),
        look.log()
    );
    // At its limit, while the clock stands there: one late line, of the store.
    assert!(
        look.is_said_late_at(late_at).await,
        "a look that stores its copy was not said late at its limit of {} ms: the clock stands at the limit and the log has no \
         late line; log:\n{}",
        limit.as_millis(),
        look.log()
    );
    let said = late_lines(&look.d);
    assert_eq!(said.len(), 1, "{}", look.log());
    assert!(
        said[0].contains("direct turn snapshot late: storing the copy has taken "),
        "a look that is late while its store has not ended is said late as storing the copy: {said:?}"
    );
    let Some(stated) = said_late(said[0].trim_end()) else {
        panic!("the worker's late line does not state its numbers: {said:?}");
    };
    println!(
        "DRIVEN: a look stored a copy and took {} ms, the look after it began {} ms later at the tick and its upload was held; the \
         test saw its late line {} ms after it began, with the clock standing; the line states {} ms taken, a limit of {} ms, a \
         set limit of {} ms and a last stored copy of {:?}",
        stored.as_millis(),
        pause.as_millis(),
        look.has_run().as_millis(),
        stated.taken.as_millis(),
        stated.limit.as_millis(),
        stated.set.as_millis(),
        stated.last
    );
    assert_eq!(
        (stated.taken, stated.limit, stated.set, stated.last),
        (limit, limit, set, Some(stored)),
        "the late line of a look in its store states the time the look had taken, the limit that applied, the set limit and \
         what the last look that stored a copy took (taken, limit, set, last): {said:?}"
    );

    // The run is told once that storing the copy has not ended.
    look.the_run_has_what_was_made().await;
    assert_eq!(
        copy_notices(&look.d),
        [LATE_STORE.to_string()],
        "what the run was told about its copies when a look was late in its store; log:\n{}",
        look.log()
    );
    assert_eq!(late_lines(&look.d).len(), 1, "{}", look.log());
    assert!(failed_lines(&look.d).is_empty(), "{}", look.log());
    // The store lets the upload go.
    look.d.aws.release_put_body();
    look.ends();
}

/// The wait for the spacing is no part of a look's late limit: the limit counts from the start of the look, after that wait.
///
/// The copy task wakes at the tick of its timer and then waits, before the look, until the spacing has passed: nine times what
/// the git work of the look before took, on the machine's clock, at most five periods of a timer that a test set
/// (`snapshot::COPY_SPACING`, `snapshot::COPY_SPACING_CAP`). It sleeps that wait on the runtime's clock, so the moment the look
/// begins is some milliseconds or seconds after the tick, as fast as the machine was. The test does not need that moment. The
/// timer of this worker is 2 s, so the spacing is 10 s at the most, and its set limit is 12 s, longer than any such wait. The
/// first look waits in `git add` for 2 s of real time, so its spacing is the whole 10 s (nine times 2 s is more); it then
/// stores its copy, having taken 200 ms on the driven clock, and the test runs the clock to the tick 2 s after that look began.
/// What the test reads of the machine's clock bounds the wait from below: the git work of that look ended after the test let
/// it go, and the task woke at the tick before the test read the clock again, so the wait is at least 10 s less the real time
/// between those two readings, which is required to be less than 10 s (TRIGGER PRESENT). So the look after it begins after the
/// tick and within 10 s of it.
///
/// 12 s after the tick that look waits in `git add` and the log has no late line: its limit runs from its own start. A worker
/// that arms the late timer when it woke at the tick says it late at that moment. 22 s after the tick, the latest moment at
/// which its limit can end, the log has one late line, stating 12, 12, 12 and 0.2 s.
async fn said_late_after_a_wait_for_the_spacing(dir: PathBuf) {
    let timer = Duration::from_secs(2);
    let spacing_cap = Duration::from_secs(10);
    let set = Duration::from_secs(12);
    let stored = Duration::from_millis(200);
    let git_held = Duration::from_secs(2);
    let git_limit = set + HELD_PAST_THE_LIMIT;
    let mut look = a_look_held_on_the_timer(timer, timer, set, git_limit, &dir).await;
    let first = look.began;
    // The first look waits in `git add` for 2 s of real time, while the clock stands; its git work began before this reading.
    let held_from = Instant::now();
    look.clock.stands_for(git_held).await;
    // Nothing more is waited for after its git work: the spacing is not to pass before the tick.
    look.spacing = Duration::ZERO;
    let let_go = Instant::now();
    look.stores_a_copy(stored, Next::HELD).await;
    look.has_stored(1);

    // The tick, 2 s after the first look began.
    let tick = first + timer;
    look.clock.runs_to(tick).await;
    let at_the_tick = Instant::now();
    let between = at_the_tick - let_go;
    assert!(
        let_go - held_from >= git_held && between < spacing_cap,
        "TRIGGER PRESENT: the git work of the stored look was held {} ms of real time, and {} ms of real time passed from the \
         moment it was let go to the tick, so the spacing still holds the next look back at the tick; log:\n{}",
        (let_go - held_from).as_millis(),
        between.as_millis(),
        look.log()
    );
    assert!(
        !look.held.exists(),
        "TRIGGER PRESENT: no look began at the tick, while the clock stands there; log:\n{}",
        look.log()
    );

    // One late limit after the tick: the look waits in `git add`, and it is not late.
    look.clock.runs_to(tick + set).await;
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || look.held.exists())
            .await,
        "TRIGGER PRESENT: the look after the stored look did not wait in `git add` within the {} s of the spacing after the \
         tick; log:\n{}",
        spacing_cap.as_secs(),
        look.log()
    );
    assert!(
        late_lines(&look.d).is_empty(),
        "a look that waited for its spacing after the tick was said late {} s after the tick: its limit of {} s counts from its \
         own start, after that wait; log:\n{}",
        set.as_secs(),
        set.as_secs(),
        look.log()
    );

    // By the latest moment at which its limit can end: one late line, with the numbers of this look.
    let latest = tick + spacing_cap + set;
    look.clock.runs_to(latest).await;
    assert!(
        look.clock
            .stands_until(REAL_WORK_WITHIN, || !late_lines(&look.d).is_empty())
            .await,
        "a look that waited for its spacing after the tick was not said late {} s after the tick, the latest moment at which \
         its limit ends; log:\n{}",
        (spacing_cap + set).as_secs(),
        look.log()
    );
    let said = late_lines(&look.d);
    assert_eq!(said.len(), 1, "{}", look.log());
    let Some(stated) = said_late(said[0].trim_end()) else {
        panic!("the worker's late line does not state its numbers: {said:?}");
    };
    println!(
        "DRIVEN: a look stored a copy and took {} ms after its git work was held {} ms of real time; {} ms of real time passed \
         from then to the tick; 12 s after the tick the look after it waited in `git add` with no late line; its line states \
         {} ms taken, a limit of {} ms, a set limit of {} ms and a last stored copy of {:?}",
        stored.as_millis(),
        (let_go - held_from).as_millis(),
        between.as_millis(),
        stated.taken.as_millis(),
        stated.limit.as_millis(),
        stated.set.as_millis(),
        stated.last
    );
    assert_eq!(
        (stated.taken, stated.limit, stated.set, stated.last),
        (set, set, set, Some(stored)),
        "the late line of a look that waited for its spacing states the time the look had taken from its own start, the limit \
         that applied, the set limit and what the last look that stored a copy took (taken, limit, set, last): {said:?}"
    );
    assert!(failed_lines(&look.d).is_empty(), "{}", look.log());
    look.ends();
}

/// The set limit applies: the last stored look took 100 ms, and three times that is less than the 400 ms these tests set.
#[test]
fn b893_a_look_is_said_late_at_the_set_limit_and_not_a_millisecond_before() {
    on_a_driven_clock(
        "b893_a_look_is_said_late_at_the_set_limit_and_not_a_millisecond_before",
        |dir| said_late_at_its_limit_and_given_up_afterwards(LAST_100_MS.into(), dir),
    );
}

/// Three times the last stored look applies: that look took 200 ms, so the limit is 600 ms and not the 400 ms these tests set.
#[test]
fn b893_a_look_is_said_late_at_three_times_the_last_stored_look_and_not_a_millisecond_before() {
    on_a_driven_clock(
        "b893_a_look_is_said_late_at_three_times_the_last_stored_look_and_not_a_millisecond_before",
        |dir| said_late_at_its_limit_and_given_up_afterwards(LAST_200_MS.into(), dir),
    );
}

/// Three times the last stored look applies, with a number that is not round: that look took 137 ms, so the limit is 411 ms.
#[test]
fn b893_a_look_is_said_late_411_ms_after_a_stored_look_of_137_ms_and_not_a_millisecond_before() {
    on_a_driven_clock(
        "b893_a_look_is_said_late_411_ms_after_a_stored_look_of_137_ms_and_not_a_millisecond_before",
        |dir| said_late_at_its_limit_and_given_up_afterwards(LAST_137_MS.into(), dir),
    );
}

/// Three times the last stored look applies, with a look of a whole second: the limit is 3,000 ms.
#[test]
fn b893_a_look_is_said_late_3_s_after_a_stored_look_of_1_s_and_not_a_millisecond_before() {
    on_a_driven_clock(
        "b893_a_look_is_said_late_3_s_after_a_stored_look_of_1_s_and_not_a_millisecond_before",
        |dir| said_late_at_its_limit_and_given_up_afterwards(LAST_1000_MS.into(), dir),
    );
}

/// The set limit applies (400 ms), and the look is given up after 380 ms.
#[test]
fn b893_a_look_that_is_given_up_before_the_set_limit_is_never_said_late() {
    on_a_driven_clock(
        "b893_a_look_that_is_given_up_before_the_set_limit_is_never_said_late",
        |dir| given_up_before_its_late_limit_and_never_said_late(LAST_100_MS.into(), dir),
    );
}

/// Three times the last stored look applies (600 ms), and the look is given up after 580 ms: a worker that took the set limit of
/// 400 ms for it would say it late first.
#[test]
fn b893_a_look_that_is_given_up_before_three_times_the_last_stored_look_is_never_said_late() {
    on_a_driven_clock(
        "b893_a_look_that_is_given_up_before_three_times_the_last_stored_look_is_never_said_late",
        |dir| given_up_before_its_late_limit_and_never_said_late(LAST_200_MS.into(), dir),
    );
}

/// The last stored look took 137 ms (a limit of 411 ms), and the look is given up after 391 ms.
#[test]
fn b893_a_look_that_is_given_up_before_411_ms_after_a_stored_look_of_137_ms_is_never_said_late() {
    on_a_driven_clock(
        "b893_a_look_that_is_given_up_before_411_ms_after_a_stored_look_of_137_ms_is_never_said_late",
        |dir| given_up_before_its_late_limit_and_never_said_late(LAST_137_MS.into(), dir),
    );
}

/// The last stored look took a second (a limit of 3,000 ms), and the look is given up after 2,980 ms: a worker that took the set
/// limit of 400 ms for it would say it late first.
#[test]
fn b893_a_look_that_is_given_up_before_3_s_after_a_stored_look_of_1_s_is_never_said_late() {
    on_a_driven_clock(
        "b893_a_look_that_is_given_up_before_3_s_after_a_stored_look_of_1_s_is_never_said_late",
        |dir| given_up_before_its_late_limit_and_never_said_late(LAST_1000_MS.into(), dir),
    );
}

/// Three times the last stored look applies (600 ms). The worker asks the tree every 400 ms, so 200 ms before the limit; the
/// look was asked to end one millisecond before that question. The test holds the question, and no late line is written
/// before the tree has answered.
#[test]
fn b893_a_late_line_waits_for_the_answer_of_a_tree_that_was_asked_before_the_limit() {
    on_a_driven_clock_with(
        "b893_a_late_line_waits_for_the_answer_of_a_tree_that_was_asked_before_the_limit",
        Some(HELD_QUESTION_THREADS),
        |dir| said_late_when_the_tree_has_answered(LAST_200_MS, dir),
    );
}

/// The set limit applies (400 ms). The worker's question to the tree is due at the moment of the limit; the look was asked to
/// end one millisecond before. The test holds the question. The worker writes the late line before it asks; the test
/// takes either order, and the line states the limit whether the worker wrote it before it asked or after the answer.
#[test]
fn b893_a_late_line_at_the_set_limit_states_the_limit_whenever_the_tree_answers() {
    on_a_driven_clock_with(
        "b893_a_late_line_at_the_set_limit_states_the_limit_whenever_the_tree_answers",
        Some(HELD_QUESTION_THREADS),
        |dir| said_late_when_the_tree_has_answered(LAST_100_MS, dir),
    );
}

/// The set limit applies (400 ms): after 399 ms the run has been told nothing about its copies.
#[test]
fn b893_the_run_is_told_nothing_before_the_set_limit() {
    on_a_driven_clock("b893_the_run_is_told_nothing_before_the_set_limit", |dir| {
        told_at_a_moment_of_a_held_look(LAST_100_MS.into(), false, dir)
    });
}

/// Three times the last stored look applies (600 ms): after 599 ms the run has been told nothing about its copies.
#[test]
fn b893_the_run_is_told_nothing_before_three_times_the_last_stored_look() {
    on_a_driven_clock(
        "b893_the_run_is_told_nothing_before_three_times_the_last_stored_look",
        |dir| told_at_a_moment_of_a_held_look(LAST_200_MS.into(), false, dir),
    );
}

/// The set limit applies (400 ms): with the clock standing at 400 ms the worker has made the late notice.
#[test]
fn b893_the_run_is_told_at_the_set_limit() {
    on_a_driven_clock("b893_the_run_is_told_at_the_set_limit", |dir| {
        told_at_a_moment_of_a_held_look(LAST_100_MS.into(), true, dir)
    });
}

/// Three times the last stored look applies (600 ms): with the clock standing at 600 ms the worker has made the late notice.
#[test]
fn b893_the_run_is_told_at_three_times_the_last_stored_look() {
    on_a_driven_clock(
        "b893_the_run_is_told_at_three_times_the_last_stored_look",
        |dir| told_at_a_moment_of_a_held_look(LAST_200_MS.into(), true, dir),
    );
}

/// Three times the last stored look applies, with a look of a second and a half: the limit is 4,500 ms. A rule that takes
/// twice a look of more than a second gives 3,000 ms.
#[test]
fn b893_a_look_is_said_late_4500_ms_after_a_stored_look_of_1500_ms_and_not_a_millisecond_before() {
    on_a_driven_clock(
        "b893_a_look_is_said_late_4500_ms_after_a_stored_look_of_1500_ms_and_not_a_millisecond_before",
        |dir| said_late_at_its_limit_and_given_up_afterwards(LAST_1500_MS.into(), dir),
    );
}

/// The first look of a worker: no look stored a copy before it, so the set limit applies. It is said late 400 ms after it
/// began and not a millisecond before, once, and its line states that no copy was stored before.
#[test]
fn b893_the_first_look_of_a_worker_is_said_late_at_the_set_limit_and_not_a_millisecond_before() {
    on_a_driven_clock(
        "b893_the_first_look_of_a_worker_is_said_late_at_the_set_limit_and_not_a_millisecond_before",
        |dir| said_late_at_its_limit_and_given_up_afterwards(THE_FIRST_LOOK, dir),
    );
}

/// The first look of a worker is given up after 380 ms, 20 ms before the set limit: it is never said late.
#[test]
fn b893_the_first_look_of_a_worker_that_is_given_up_before_the_set_limit_is_never_said_late() {
    on_a_driven_clock(
        "b893_the_first_look_of_a_worker_that_is_given_up_before_the_set_limit_is_never_said_late",
        |dir| given_up_before_its_late_limit_and_never_said_late(THE_FIRST_LOOK, dir),
    );
}

/// The first look of a worker: after 399 ms the run has been told nothing about its copies.
#[test]
fn b893_the_run_is_told_nothing_before_the_set_limit_of_the_first_look_of_a_worker() {
    on_a_driven_clock(
        "b893_the_run_is_told_nothing_before_the_set_limit_of_the_first_look_of_a_worker",
        |dir| told_at_a_moment_of_a_held_look(THE_FIRST_LOOK, false, dir),
    );
}

/// The first look of a worker: with the clock standing at 400 ms the worker has made the late notice.
#[test]
fn b893_the_run_is_told_at_the_set_limit_of_the_first_look_of_a_worker() {
    on_a_driven_clock(
        "b893_the_run_is_told_at_the_set_limit_of_the_first_look_of_a_worker",
        |dir| told_at_a_moment_of_a_held_look(THE_FIRST_LOOK, true, dir),
    );
}

/// Two looks stored a copy and took 100 ms and then 200 ms: the last one counts. The look after them is said late at 600 ms,
/// and the log has no late line at 400 ms.
#[test]
fn b893_after_stored_looks_of_100_ms_and_200_ms_a_look_is_said_late_at_600_ms_and_not_at_400_ms() {
    on_a_driven_clock(
        "b893_after_stored_looks_of_100_ms_and_200_ms_a_look_is_said_late_at_600_ms_and_not_at_400_ms",
        |dir| said_late_at_its_limit_and_given_up_afterwards(STORED_100_THEN_200_MS, dir),
    );
}

/// Two looks stored a copy and took 200 ms and then 100 ms: the last one counts. The look after them is said late at the
/// 400 ms that are set.
#[test]
fn b893_after_stored_looks_of_200_ms_and_100_ms_a_look_is_said_late_at_400_ms() {
    on_a_driven_clock(
        "b893_after_stored_looks_of_200_ms_and_100_ms_a_look_is_said_late_at_400_ms",
        |dir| said_late_at_its_limit_and_given_up_afterwards(STORED_200_THEN_100_MS, dir),
    );
}

/// A look stored a copy and took 200 ms, and the look after it found the tree unchanged and stored nothing: it does not count.
/// The look after that is said late at 600 ms, and the log has no late line at 400 ms.
#[test]
fn b893_a_look_that_stored_nothing_does_not_count_the_next_look_is_said_late_at_600_ms() {
    on_a_driven_clock(
        "b893_a_look_that_stored_nothing_does_not_count_the_next_look_is_said_late_at_600_ms",
        |dir| said_late_at_its_limit_and_given_up_afterwards(STORED_200_MS_THEN_UNCHANGED, dir),
    );
}

/// A look stored a copy and took 200 ms, and the look after it was given up: it does not count either. The look after that
/// is said late at 600 ms, with no late line at 400 ms, and the run is told nothing more.
#[test]
fn b893_a_look_after_a_look_that_was_given_up_is_said_late_at_600_ms_and_the_run_is_told_nothing_more()
 {
    on_a_driven_clock(
        "b893_a_look_after_a_look_that_was_given_up_is_said_late_at_600_ms_and_the_run_is_told_nothing_more",
        late_after_a_look_that_was_given_up,
    );
}

/// A look stored a copy and took 200 ms, and the next look began 150 ms after it had ended. That look is late 600 ms after
/// its own start, and the log has no late line 600 ms after the stored look ended.
#[test]
fn b893_a_look_that_begins_150_ms_after_the_stored_look_ended_is_late_600_ms_after_its_own_start() {
    on_a_driven_clock(
        "b893_a_look_that_begins_150_ms_after_the_stored_look_ended_is_late_600_ms_after_its_own_start",
        |dir| {
            said_late_at_its_limit_and_given_up_afterwards(
                STORED_200_MS_THEN_NO_LOOK_FOR_150_MS,
                dir,
            )
        },
    );
}

/// The first look of a worker is said late at 400 ms and then stores a copy at 500 ms. It counts in full: the look after it is
/// said late at 1,500 ms and not at 400 ms, and the run is told a second time that a copy is late.
#[test]
fn b893_a_late_look_that_stored_a_copy_counts_and_the_next_late_look_tells_the_run_again() {
    on_a_driven_clock(
        "b893_a_late_look_that_stored_a_copy_counts_and_the_next_late_look_tells_the_run_again",
        told_again_after_a_late_look_that_stored_a_copy,
    );
}

/// A look is said late at 600 ms and then finds the tree unchanged; the look after it is said late at 600 ms as well. No copy
/// was stored between them, so the run is told once.
#[test]
fn b893_two_late_looks_with_no_stored_copy_between_them_tell_the_run_once() {
    on_a_driven_clock(
        "b893_two_late_looks_with_no_stored_copy_between_them_tell_the_run_once",
        told_once_over_two_late_looks_with_no_stored_copy_between,
    );
}

/// A look stored a copy and took 200 ms; the look after it reads the tree and its upload is held. It is said late at 600 ms as
/// storing the copy, and the run is told so, once.
#[test]
fn b893_a_look_that_is_late_while_it_stores_is_said_late_at_600_ms_as_storing_the_copy() {
    on_a_driven_clock(
        "b893_a_look_that_is_late_while_it_stores_is_said_late_at_600_ms_as_storing_the_copy",
        said_late_while_it_stores,
    );
}

/// A look stored a copy, and the look after it waits for its spacing after the tick of the timer, up to 10 s. It is not said
/// late 12 s after the tick, its limit: the limit counts from its own start, after that wait.
#[test]
fn b893_a_look_that_waits_for_its_spacing_after_the_tick_is_late_from_its_own_start() {
    on_a_driven_clock(
        "b893_a_look_that_waits_for_its_spacing_after_the_tick_is_late_from_its_own_start",
        said_late_after_a_wait_for_the_spacing,
    );
}

// ---- agentd is asked to end ---------------------------------------------------------------------------------------------------------

/// A real `agentd serve` whose engine has edited its files and is silent for `silence_s` seconds; the copies its tool calls asked for are
/// in the bucket. The timer is `every_ms`; `more` is added to the environment of the process.
async fn working_process(
    every_ms: &str,
    silence_s: &str,
    more: Vec<(&'static str, String)>,
) -> Direct {
    let mut agentd_env = vec![
        ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", every_ms.to_string()),
        ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
    ];
    agentd_env.extend(more);
    let d = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", silence_s.into())],
        agentd_env,
        ..Opts::default()
    })
    .await;
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(Duration::from_secs(30), || !records(&d).is_empty()).await,
        "the copy after the engine's first tool call"
    );
    // The engine's edits are two tool calls: the copy the second one asks for is stored before the test goes on.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    d
}

/// Waits until the process has ended and returns its exit code; `None` when it still runs after `limit`.
async fn ended_within(d: &mut Direct, limit: Duration) -> Option<Option<i32>> {
    let began = Instant::now();
    loop {
        if let Some(code) = d.process.as_mut().unwrap().ended() {
            return Some(code);
        }
        if began.elapsed() > limit {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// agentd is asked to end while the engine works (the timer is an hour away and no tool call ends, so nothing else asks for a copy). Before,
/// agentd had no handler: the signal ended it at once where it is not the first process of its machine, and was ignored where it is, and in
/// both cases the work written since the last copy was in no copy.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_termination_signal_saves_the_newest_work_at_once() {
    let mut d = working_process("3600000", "40", Vec::new()).await;
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    std::fs::write(
        d.worktree().join("late.txt"),
        "written after the last copy and before the signal\n",
    )
    .unwrap();
    let asked = Instant::now();
    d.process.as_ref().unwrap().signal("TERM");
    let stored = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    let took = asked.elapsed();
    assert!(
        stored,
        "TRIGGER PRESENT: no copy after the termination signal holds the file written before it; the process ended with {:?}; log:\n{}",
        d.process.as_mut().unwrap().ended(),
        d.process.as_ref().unwrap().log_text()
    );
    assert!(
        took < Duration::from_secs(20),
        "the copy came {took:?} after the signal"
    );
    let last = records(&d).pop().unwrap();
    assert!(
        last.get("final").is_none(),
        "the engine had not stopped: this is no copy of the end: {last}"
    );
    // The process is not the first one of its machine here, so it ends as the signal asks, after the copy: 128 + 15.
    assert_eq!(
        ended_within(&mut d, Duration::from_secs(20)).await,
        Some(Some(143)),
        "log:\n{}",
        d.process.as_ref().unwrap().log_text()
    );
    let log = d.process.as_ref().unwrap().log_text();
    assert!(
        log.contains("termination signal SIGTERM: a copy of the work is saved first"),
        "{log}"
    );
    assert!(
        log.contains("direct turn session ending: a copy of the work was stored"),
        "{log}"
    );
}

/// The signal comes while a look is in flight: its tree was read before the last file was written, and its upload takes three seconds.
/// The copy that follows the signal must hold that last file all the same: a look in flight is no reason to skip it.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_termination_signal_during_a_look_in_flight_still_saves_the_newest_work() {
    let mut d = working_process("300", "40", Vec::new()).await;
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    // The next upload of a copy's bundle is answered after 3 s: the look that reads `mid.txt` is in flight for that long.
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(3), 1);
    std::fs::write(d.worktree().join("mid.txt"), "read by the look in flight\n").unwrap();
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    std::fs::write(
        d.worktree().join("late.txt"),
        "written while that look stored its copy\n",
    )
    .unwrap();
    d.process.as_ref().unwrap().signal("TERM");
    let stored = wait_for(Duration::from_secs(25), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        stored,
        "TRIGGER PRESENT: the copy in the bucket does not hold the file written before the signal; records {:?}; the process ended \
         with {:?}; log:\n{}",
        records(&d),
        d.process.as_mut().unwrap().ended(),
        d.process.as_ref().unwrap().log_text()
    );
    assert_eq!(
        ended_within(&mut d, Duration::from_secs(20)).await,
        Some(Some(143))
    );
}

/// agentd is asked to end while the git of the stop copy hangs on the work tree: no copy can be made in the time the copy has. The run
/// is told so in a sentence of its own, which names the request and promises no further try: this process ends after the copy. Before,
/// the run read "ham keeps trying" from a worker that ended two event periods later.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_stop_copy_that_fails_tells_the_run_that_the_session_was_asked_to_end() {
    let scratch = common::scratch("stop-slow-tree");
    let (bin, mark) = hanging_git(&scratch);
    let mut d = working_process(
        "3600000",
        "40",
        vec![
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
            ("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", "3000".into()),
        ],
    )
    .await;
    let before = records(&d).len();
    std::fs::write(&mark, "set\n").unwrap();
    std::fs::write(d.worktree().join("late.txt"), "written before the signal\n").unwrap();
    d.process.as_ref().unwrap().signal("TERM");
    let told = wait_for(Duration::from_secs(12), || !copy_notices(&d).is_empty()).await;
    let code = ended_within(&mut d, Duration::from_secs(20)).await;
    std::fs::remove_file(&mark).unwrap();
    let log = d.process.as_ref().unwrap().log_text();
    assert!(
        told,
        "the run is told that the copy of the stop failed; log:\n{log}"
    );
    assert_eq!(
        copy_notices(&d),
        vec![STOP_NOT_SAVED.to_string()],
        "TRIGGER PRESENT: the copy of a stop failed and the run was not told that the session was asked to end; log:\n{log}"
    );
    assert!(
        log.contains("direct turn session ending: no copy of the work could be saved"),
        "{log}"
    );
    assert_eq!(code, Some(Some(143)), "log:\n{log}");
    assert_eq!(records(&d).len(), before, "no copy could be made");
}

/// A `git` that writes where it was started, the private repository it was given and its arguments to `calls`, and then is the real git.
fn recording_git(dir: &Path) -> (PathBuf, PathBuf) {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "git on the search path: {real:?}");
    let bin = dir.join("recording-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = dir.join("git-calls.tsv");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\t%s\\t%s\\n' \"$(pwd -P)\" \"${{GIT_DIR:-}}\" \"$*\" >> '{calls}'\nexec '{real}' \"$@\"\n",
        calls = calls.display()
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, calls)
}

/// The git calls of a look that work on the tree are started outside it, and git changes to the tree itself (`-C`). Starting a process in
/// a directory waits until the new process is there; in a tree that stopped answering that wait cannot be ended, and it held the copy
/// task inside its own step, where no limit of the look and no late notice could run. Before, `git add -A`, `git write-tree` and the read
/// of the engine's head were started in the work tree.
#[tokio::test(flavor = "multi_thread")]
async fn b823_the_git_of_a_look_is_started_outside_the_work_tree() {
    let scratch = common::scratch("git-outside");
    let (bin, calls) = recording_git(&scratch);
    let d = working_process(
        "300",
        "6",
        vec![("PATH", format!("{}:/usr/bin:/bin", bin.display()))],
    )
    .await;
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    // The tree as agentd names it, and where it is (the folder a process started in it reports).
    let tree = d.worktree().display().to_string();
    let tree_is = std::fs::canonicalize(d.worktree())
        .unwrap()
        .display()
        .to_string();
    let text = std::fs::read_to_string(&calls).unwrap();
    // (where it was started, its private repository, its arguments) of every git call of agentd.
    let rows: Vec<(&str, &str, &str)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            Some((f.next()?, f.next()?, f.next()?))
        })
        .collect();
    // The calls of the copies: the ones that name the private repository of the copies.
    let of_copies = |verb: &str| -> Vec<&(&str, &str, &str)> {
        rows.iter()
            .filter(|(_, git_dir, args)| git_dir.ends_with("/snapshot.git") && args.ends_with(verb))
            .collect()
    };
    for verb in [" add -A", " write-tree"] {
        let found = of_copies(verb);
        assert!(!found.is_empty(), "the copies ran git{verb}: {text}");
        for (started_in, _, args) in found {
            assert!(
                !started_in.starts_with(&tree_is),
                "TRIGGER PRESENT: git{verb} of a copy was started in the work tree ({started_in}): {args}"
            );
            assert!(
                args.starts_with(&format!("-C {tree} ")),
                "git{verb} of a copy changes to the work tree itself: {args}"
            );
        }
    }
    // The read of the engine's head, which a look makes in the engine's repository, is started outside the tree too.
    let heads: Vec<_> = rows
        .iter()
        .filter(|(started_in, _, args)| {
            *started_in == "/"
                && args.starts_with(&format!("-C {tree} "))
                && args.ends_with(" rev-parse HEAD")
        })
        .collect();
    assert!(
        !heads.is_empty(),
        "TRIGGER PRESENT: no read of the engine's head was started outside the work tree: {text}"
    );
}

/// When the session's bundle file of the private repository was written last; `None` while there is none.
fn bundle_written(d: &Direct) -> Option<std::time::SystemTime> {
    std::fs::metadata(d.agent_data().join("snapshot.git/snapshot.bundle"))
        .and_then(|m| m.modified())
        .ok()
}

/// A termination signal while a turn is in its end sequence (here: while the copy of the end is stored). No copy task works then, so the
/// signal asks for no copy; the log says that a turn runs, not that none works. A process that is not the first of its machine ends at
/// once, as it did before agentd had a handler.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_signal_in_the_end_sequence_is_logged_as_a_turn_outside_its_prompt_phase() {
    let mut d = working_process("3600000", "8", Vec::new()).await;
    // The copies the engine's tool calls asked for are stored by now; the engine is silent for some seconds more.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    // The copy of the end is the next copy: its bundle goes up for 6 s.
    let before = bundle_written(&d);
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(6), 1);
    let storing = wait_for(Duration::from_secs(30), || {
        let now = bundle_written(&d);
        now.is_some() && now != before
    })
    .await;
    assert!(
        storing,
        "the engine stopped and the copy of the end is on its way"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    d.process.as_ref().unwrap().signal("TERM");
    let code = ended_within(&mut d, Duration::from_secs(10)).await;
    let log = d.process.as_ref().unwrap().log_text();
    assert_eq!(code, Some(Some(143)), "log:\n{log}");
    assert!(
        log.contains("termination signal SIGTERM: a turn runs outside its prompt phase"),
        "TRIGGER PRESENT: a signal in the end sequence of a turn was logged as if no turn worked; log:\n{log}"
    );
    assert!(
        !log.contains("no turn is working"),
        "a turn was working; log:\n{log}"
    );
}

// ---- agentd is the first process of its machine ---------------------------------------------------------------------------------------

/// [`working_process`] with `agentd serve` as process 1 of a process-id namespace of its own, which is what a session's microVM runs:
/// the kernel gives such a process no signal it has no handler for, and agentd runs on after the copy of a stop. `None` on a machine that
/// cannot make such a namespace; the test then says so and holds nothing.
async fn working_first_process(
    every_ms: &str,
    silence_s: &str,
    more: Vec<(&'static str, String)>,
) -> Option<Direct> {
    if !first_process_possible() {
        eprintln!(
            "SKIPPED: this machine cannot start a process as the first one of a process-id namespace (unshare --user --pid --fork)"
        );
        return None;
    }
    let mut agentd_env = vec![
        ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", every_ms.to_string()),
        ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
    ];
    agentd_env.extend(more);
    let d = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", silence_s.into())],
        agentd_env,
        first_process: true,
        ..Opts::default()
    })
    .await;
    assert_eq!(
        d.process.as_ref().unwrap().own_pid(),
        Some(1),
        "TRIGGER PRESENT: agentd must be process 1 of its own namespace for this test"
    );
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(Duration::from_secs(60), || !records(&d).is_empty()).await,
        "the copy after the engine's first tool call; log:\n{}",
        d.process.as_ref().unwrap().log_text()
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    Some(d)
}

/// A look reads `mid.txt`, writes its bundle and starts the upload, which the store answers after `put_s` seconds. Returns once that look
/// is storing (its bundle file is new), at most `limit` later.
async fn look_in_flight_storing(d: &Direct, put_s: u64, limit: Duration) {
    let before = bundle_written(d);
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(put_s), 1);
    std::fs::write(d.worktree().join("mid.txt"), "read by the look in flight\n").unwrap();
    let storing = wait_for(limit, || {
        let now = bundle_written(d);
        now.is_some() && now != before
    })
    .await;
    assert!(storing, "the look that reads mid.txt wrote its bundle");
    // The bundle file is in place before the upload starts: the look gets a moment to reach the upload.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Process 1 is asked to end while the engine works. The copy is saved at once, agentd runs on, the timer saves a later change, and a
/// second request saves a copy again. (A session's microVM runs agentd as its first process; before this test only the rule
/// `ends_after_the_copy(1)` was held.)
#[tokio::test(flavor = "multi_thread")]
async fn b823_the_first_process_saves_a_copy_when_asked_to_end_and_runs_on_and_the_timer_goes_on() {
    let Some(d) = working_first_process("4000", "90", Vec::new()).await else {
        return;
    };
    let agentd = d.process.as_ref().unwrap();
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    std::fs::write(d.worktree().join("late.txt"), "written before the signal\n").unwrap();
    agentd.signal("TERM");
    let stored = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        stored,
        "TRIGGER PRESENT: process 1 saved no copy when it was asked to end; log:\n{}",
        agentd.log_text()
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        agentd.alive(),
        "process 1 runs on after the copy; log:\n{}",
        agentd.log_text()
    );
    // A change after the stop copy: only the timer can save it.
    std::fs::write(
        d.worktree().join("third.txt"),
        "written after the stop copy\n",
    )
    .unwrap();
    let by_timer = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "third.txt")
    })
    .await;
    assert!(
        by_timer,
        "the timer keeps saving copies after the stop copy; log:\n{}",
        agentd.log_text()
    );
    // A second request, with newer work.
    std::fs::write(
        d.worktree().join("fourth.txt"),
        "written before the second signal\n",
    )
    .unwrap();
    agentd.signal("TERM");
    let again = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "fourth.txt")
    })
    .await;
    let log = agentd.log_text();
    assert!(again, "a second request saves a copy again; log:\n{log}");
    assert!(
        log.contains("direct turn session ending: a copy of the work was stored"),
        "{log}"
    );
    assert!(
        log.contains(
            "agentd works behind the first process of its machine and runs on until the machine ends"
        ),
        "{log}"
    );
    assert!(copy_notices(&d).is_empty(), "{:?}", copy_notices(&d));
}

/// Process 1 is asked to end while a look is in flight whose upload is answered about 5 s after it began. The time who asked waits for
/// the copy is 1.5 s here (30 s in the product); the timer is 20 s. Process 1 runs on, so nothing ends at that time: the look in flight
/// keeps its own limits and stores its copy, and one more look follows it at once for what was written after it had read the tree.
/// Before, the request cut the look in flight at that time, no look followed, and the next copy waited for the next tick of the timer: a
/// copy that needed longer than the budget was thrown away although the machine lived on.
#[tokio::test(flavor = "multi_thread")]
async fn b823_the_first_process_lets_the_look_in_flight_store_its_copy_and_copies_once_more() {
    let Some(d) = working_first_process(
        "20000",
        "120",
        vec![("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", "1500".into())],
    )
    .await
    else {
        return;
    };
    let agentd = d.process.as_ref().unwrap();
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    // The look of the next tick reads `mid.txt`; its upload is answered 5 s after it began.
    look_in_flight_storing(&d, 5, Duration::from_secs(40)).await;
    std::fs::write(
        d.worktree().join("late.txt"),
        "written while that look stored its copy\n",
    )
    .unwrap();
    let asked = Instant::now();
    agentd.signal("TERM");
    // The upload in flight is answered about 5 s after it began: its copy is in the bucket well before the next tick, 20 s after the look
    // began.
    let mid = wait_for(Duration::from_secs(11), || {
        watch.new_copy_holds(&d, "mid.txt")
    })
    .await;
    let mid_took = asked.elapsed();
    assert!(
        mid,
        "TRIGGER PRESENT: process 1 runs on, and the copy that was in flight when it was asked to end was not stored {mid_took:?} after \
         the request; log:\n{}",
        agentd.log_text()
    );
    // The look that follows it stores what was written before the request.
    let late = clone_of_snapshot(&d, "after-the-look-in-flight")
        .join("late.txt")
        .is_file()
        || wait_for(Duration::from_secs(8), || {
            watch.new_copy_holds(&d, "late.txt")
        })
        .await;
    let late_took = asked.elapsed();
    let log = agentd.log_text();
    assert!(
        late,
        "TRIGGER PRESENT: no look followed the one in flight; the file written before the request was in no copy {late_took:?} after \
         it; log:\n{log}"
    );
    assert!(
        late_took < Duration::from_secs(16),
        "the copy came from the look that follows, not from the next tick: {late_took:?}"
    );
    assert!(agentd.alive(), "process 1 runs on; log:\n{log}");
    assert!(
        !log.contains("not stored in the time agentd had left"),
        "the look in flight kept its own limits; log:\n{log}"
    );
    assert!(
        log.contains("direct turn session ending: a copy of the work was stored"),
        "{log}"
    );
    assert!(copy_notices(&d).is_empty(), "{:?}", copy_notices(&d));
}

/// Process 1 is asked to end and the copy of the stop cannot be stored (the store takes its upload and does not answer). The timer is an
/// hour away. The run is told that the session was asked to end and that the copy could not be saved, and the copy task looks again a few
/// seconds later, because the process runs on: the store answers then, and the copy holds the work. Before, the next try was the next
/// tick of the timer, which a request to end had moved a whole period away.
#[tokio::test(flavor = "multi_thread")]
async fn b823_the_first_process_tries_a_stop_copy_that_failed_again_soon() {
    let Some(d) = working_first_process(
        "3600000",
        "120",
        vec![("AGENTD_DIRECT_COPY_STORE_LIMIT_MS", "1500".into())],
    )
    .await
    else {
        return;
    };
    let agentd = d.process.as_ref().unwrap();
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    // The next upload of a copy's bundle gets no answer for a minute; the store answers every PUT after it at once.
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(60), 1);
    std::fs::write(d.worktree().join("late.txt"), "written before the signal\n").unwrap();
    let asked = Instant::now();
    agentd.signal("TERM");
    let stored = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    let took = asked.elapsed();
    let log = agentd.log_text();
    assert!(
        log.contains("direct turn session ending: no copy of the work could be saved"),
        "the copy of the stop failed first; log:\n{log}"
    );
    assert!(
        stored,
        "TRIGGER PRESENT: the copy of a stop failed, process 1 ran on, and no copy held the work {took:?} after the request (the timer \
         is an hour away); log:\n{log}"
    );
    assert_eq!(copy_notices(&d), vec![STOP_NOT_SAVED.to_string()], "{log}");
    assert!(agentd.alive(), "process 1 runs on; log:\n{log}");
}

/// A process that is not the first of its machine ends after the copy of a stop, so nothing of a look runs after the time that copy
/// has: a look in flight that needs longer is given up, and what it had read is lost with the process, as it was before agentd had a
/// handler. (The first process keeps such a look: `b823_the_first_process_lets_the_look_in_flight_store_its_copy_and_copies_once_more`.)
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_process_that_ends_after_the_copy_gives_a_look_in_flight_up_at_the_stop_budget() {
    let mut d = working_process(
        "300",
        "60",
        vec![("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", "1500".into())],
    )
    .await;
    look_in_flight_storing(&d, 8, Duration::from_secs(30)).await;
    d.process.as_ref().unwrap().signal("TERM");
    let code = ended_within(&mut d, Duration::from_secs(30)).await;
    let log = d.process.as_ref().unwrap().log_text();
    assert_eq!(code, Some(Some(143)), "log:\n{log}");
    assert!(
        log.contains("not stored in the time agentd had left"),
        "the look in flight was given up at the stop budget; log:\n{log}"
    );
    assert!(
        log.contains("direct turn session ending: no copy of the work could be saved"),
        "log:\n{log}"
    );
}

// ---- the time a copy names ------------------------------------------------------------------------------------------------------------

/// The time in a copy's record is what the client prints as "changes after <time> are not in it". Before, it was the second in which the
/// bundle had been stored: a file written after the tree was read, in that second or before it, was not in the copy although the line
/// said so (seen live: a file of 08:20:41 missing from the copy "saved 08:20:41").
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_copy_names_a_time_before_its_tree_was_read_not_the_time_it_was_stored() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "6".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(300));
            c.direct.snapshot_floor = Some(Duration::from_millis(50));
        })),
        ..Opts::default()
    })
    .await;
    // The first upload of a copy's bundle takes 2 s: the tree of that copy was read at least 2 s before its record is in the bucket.
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(2), 1);
    let watcher = async {
        let seen = wait_for(Duration::from_secs(60), || !records(&d).is_empty()).await;
        (seen, agentd::time::now_unix())
    };
    let ((_, resp), (seen, seen_unix)) = tokio::join!(d.invoke(), watcher);
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(seen, "a first copy");
    let first = records(&d).remove(0);
    let at = agentd::time::parse_rfc3339(first["at"].as_str().unwrap()).unwrap();
    assert!(
        seen_unix - at >= 2,
        "TRIGGER PRESENT: the copy names {} and its record was in the bucket at {seen_unix}, {} s later, but its bundle alone took 2 s to \
         store: the time it names is not from before its tree was read",
        first["at"],
        seen_unix - at
    );
    // The event of that copy carries the same time.
    assert_eq!(saved_copies(&d)[0]["at"], first["at"]);
}

// ---- the small parts ----------------------------------------------------------------------------------------------------------------

/// Whether `took` is `secs` seconds: the paused clock of a test ends a sleep at its time, rounded up to the next millisecond.
fn took_secs(took: Duration, secs: u64) -> bool {
    took >= Duration::from_secs(secs)
        && took < Duration::from_secs(secs) + Duration::from_millis(50)
}

#[test]
fn b823_a_copy_names_the_second_before_its_tree_was_read() {
    // A look that began to read the tree in second 100 names second 99: all of second 99 was over before the reading began.
    assert_eq!(cut_unix(100), 99);
}

#[test]
fn b823_the_limits_of_a_look_follow_the_last_copy_and_the_size_of_the_copy() {
    // The git work of a look never has more than an hour (`b823_copy_rules` holds the rule that gives it less).
    assert_eq!(COPY_GIT_LIMIT, Duration::from_secs(3_600));
    // A look is late after twenty seconds, and for a repository whose copies take long after three times its last stored copy.
    assert_eq!(late_after(COPY_LATE_AFTER, None), Duration::from_secs(20));
    assert_eq!(
        late_after(COPY_LATE_AFTER, Some(Duration::from_secs(1))),
        Duration::from_secs(20)
    );
    assert_eq!(
        late_after(COPY_LATE_AFTER, Some(Duration::from_secs(40))),
        Duration::from_secs(120)
    );
    // Thirty seconds to store a copy, and a second more for each mebibyte of its bundle.
    assert_eq!(store_limit(0), COPY_STORE_LIMIT);
    assert_eq!(store_limit(4_709), Duration::from_secs(30));
    assert_eq!(store_limit(8 * 1024 * 1024), Duration::from_secs(38));
    assert_eq!(store_limit(1024 * 1024 * 1024), Duration::from_secs(1_054));
}

#[tokio::test(start_paused = true)]
async fn b823_work_is_given_up_at_the_time_named_and_the_time_only_moves_forward() {
    let began = tokio::time::Instant::now();
    // No time: nothing is given up.
    let never = GiveUp::at(None);
    assert_eq!(
        never
            .run(tokio::time::sleep(Duration::from_secs(3_600)))
            .await,
        Some(())
    );
    // A time: work that ends before it answers, work that does not is given up at that time.
    let at = tokio::time::Instant::now() + Duration::from_secs(10);
    let give_up = GiveUp::at(Some(at));
    assert_eq!(give_up.run(async { 7 }).await, Some(7));
    assert_eq!(
        give_up
            .run(tokio::time::sleep(Duration::from_secs(60)))
            .await,
        None
    );
    assert!(took_secs(began.elapsed(), 3_610), "{:?}", began.elapsed());
    // The time is brought forward while the work runs (agentd was asked to end), never back.
    let give_up = GiveUp::at(Some(tokio::time::Instant::now() + Duration::from_secs(300)));
    let sooner = tokio::time::Instant::now() + Duration::from_secs(30);
    let work = give_up.run(tokio::time::sleep(Duration::from_secs(200)));
    let moved = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        give_up.no_later_than(sooner);
        give_up.no_later_than(sooner + Duration::from_secs(100));
    };
    let (ended, ()) = tokio::join!(work, moved);
    assert_eq!(ended, None);
    assert!(
        took_secs(began.elapsed(), 3_640),
        "given up 30 s after the look began, not 300 s: {:?}",
        began.elapsed()
    );
    // A time named for a look that had none.
    let give_up = GiveUp::at(None);
    give_up.no_later_than(tokio::time::Instant::now() + Duration::from_secs(1));
    assert_eq!(
        give_up
            .run(tokio::time::sleep(Duration::from_secs(2)))
            .await,
        None
    );
}

#[tokio::test(start_paused = true)]
async fn b823_who_asks_agentd_to_end_waits_for_the_copy_and_for_nothing_when_no_turn_works() {
    let ending = std::sync::Arc::new(Ending::default());
    // No copy task watches: there is no work to copy and nothing to wait for.
    assert!(!ending.watched());
    let n = ending.ask();
    assert_eq!(n, 1);
    let began = tokio::time::Instant::now();
    assert!(ending.saved(n, Duration::from_secs(30)).await);
    assert_eq!(began.elapsed(), Duration::ZERO);

    // A copy task watches: it is told, and who asked waits until the copy is saved.
    let mut watch = ending.watch();
    assert!(ending.watched());
    let n = ending.ask();
    assert_eq!(watch.asked().await, n);
    let waiting = ending.saved(n, Duration::from_secs(30));
    let copying = async {
        tokio::time::sleep(Duration::from_secs(4)).await;
        watch.saved(n);
    };
    let began = tokio::time::Instant::now();
    let (saved, ()) = tokio::join!(waiting, copying);
    assert!(saved);
    assert!(took_secs(began.elapsed(), 4), "{:?}", began.elapsed());

    // A copy that does not come in time: who asked goes on at the limit.
    let n = ending.ask();
    let began = tokio::time::Instant::now();
    assert!(!ending.saved(n, Duration::from_secs(30)).await);
    assert!(took_secs(began.elapsed(), 30), "{:?}", began.elapsed());

    // The prompt phase ends while someone waits (the copy task is dropped): the wait is over at once.
    let n = ending.ask();
    let waiting = ending.saved(n, Duration::from_secs(30));
    let ended = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(watch);
    };
    let began = tokio::time::Instant::now();
    let (released, ()) = tokio::join!(waiting, ended);
    assert!(released);
    assert!(took_secs(began.elapsed(), 2), "{:?}", began.elapsed());
    assert!(!ending.watched());
}

#[test]
fn b823_only_the_first_process_of_its_machine_runs_on_after_the_copy() {
    // Process 1 ignored the signal while agentd had no handler; it still runs on, and the timer keeps saving copies.
    assert!(!ends_after_the_copy(1, false));
    // In a session's microVM process 1 starts the worker and passes the signal on to it; the worker runs on in its place.
    assert!(!ends_after_the_copy(2, true));
    assert!(!ends_after_the_copy(1, true));
    // Any other process was ended by the signal; it still ends, after the copy.
    assert!(ends_after_the_copy(2, false));
    assert!(ends_after_the_copy(std::process::id(), false));
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_a_tree_that_is_there_answers_and_one_that_is_gone_is_named() {
    let dir = common::scratch("tree-alive");
    let tree = dir.join("repo");
    std::fs::create_dir_all(&tree).unwrap();
    let alive = TreeAlive::new(&tree);
    assert_eq!(alive.answers(TREE_ANSWER_LIMIT).await, Ok(()));
    // Asked again: no earlier question is outstanding.
    assert_eq!(alive.answers(TREE_ANSWER_LIMIT).await, Ok(()));
    std::fs::rename(&tree, dir.join("repo-away")).unwrap();
    let gone = alive.answers(TREE_ANSWER_LIMIT).await.unwrap_err();
    assert!(gone.starts_with("the work tree: "), "{gone}");
    std::fs::rename(dir.join("repo-away"), &tree).unwrap();
    assert_eq!(alive.answers(TREE_ANSWER_LIMIT).await, Ok(()));
}

#[test]
fn b823_the_settings_that_only_tests_set_are_read_in_milliseconds() {
    use agentd::config::DirectConfig;
    let from = |pairs: &[(&str, &str)]| {
        DirectConfig::from_vars(&|k: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_string())
        })
    };
    // Unset, as in the product: the constants hold.
    let d = from(&[]);
    assert_eq!(d.copy_late, None);
    assert_eq!(d.copy_git_limit, None);
    assert_eq!(d.copy_store_limit, None);
    assert_eq!(d.stop_copy_budget, None);
    let d = from(&[
        ("AGENTD_DIRECT_COPY_LATE_MS", "400"),
        ("AGENTD_DIRECT_COPY_GIT_LIMIT_MS", "1500"),
        ("AGENTD_DIRECT_COPY_STORE_LIMIT_MS", " 2500 "),
        ("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", "5000"),
    ]);
    assert_eq!(d.copy_late, Some(Duration::from_millis(400)));
    assert_eq!(d.copy_git_limit, Some(Duration::from_millis(1_500)));
    assert_eq!(d.copy_store_limit, Some(Duration::from_millis(2_500)));
    assert_eq!(d.stop_copy_budget, Some(Duration::from_secs(5)));
    // Zero and a value that is no number leave the constant: a limit of nothing would give every look up.
    let d = from(&[
        ("AGENTD_DIRECT_COPY_LATE_MS", "0"),
        ("AGENTD_DIRECT_COPY_GIT_LIMIT_MS", "soon"),
        ("AGENTD_DIRECT_COPY_STORE_LIMIT_MS", "-1"),
        ("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", ""),
    ]);
    assert_eq!(d.copy_late, None);
    assert_eq!(d.copy_git_limit, None);
    assert_eq!(d.copy_store_limit, None);
    assert_eq!(d.stop_copy_budget, None);
}
