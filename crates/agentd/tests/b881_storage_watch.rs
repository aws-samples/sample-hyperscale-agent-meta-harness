//! The watch of the session's storage: the rule, with scripted writes and a scripted size.
//!
//! The storage of a session was seen to refuse writes with "No space left on device" while its files were at 16 percent of its size.
//! The watch makes a test write of its own at three moments and says what it sees: one notice per stretch of refusals, the first of a
//! session with the size of the files, and never a notice for an error that is not errno 28 or for files that nearly fill the storage.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::direct::disk::{DiskProbe, Limits, Reading};
use agentd::direct::snapshot::TREE_ANSWER_LIMIT;
use agentd::direct::storage_watch::{
    CopyRef, Marked, Seen, StorageWatch, StorageWrite, StorageWrites, WatchSetup, WatchTask,
    WriteError,
};
use agentd::events::EventRecorder;
use agentd::logging::MemoryLog;
use harness_protocol::envelope::{Envelope, EventType, Payload};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const RUN: &str = "01JRUN0000000000000000000A";

/// What a scripted write answers.
#[derive(Clone, Debug)]
enum Ans {
    Took,
    /// The write step fails with this errno.
    Errno(i32),
    /// The write worked and the remove of the test file failed with errno 28.
    RemoveFailed,
    /// The write waits until the gate opens, then is taken.
    Hold(Arc<(Mutex<bool>, Condvar)>),
}

/// Writes that answer from a list (then `Took`), count their calls and keep the order the calls began in.
#[derive(Debug, Default)]
struct Script {
    answers: Mutex<VecDeque<Ans>>,
    tests: AtomicU32,
    markers: AtomicU32,
    /// The most test writes that were in flight at once.
    most_at_once: AtomicU32,
    in_flight: AtomicU32,
    order: Mutex<Vec<&'static str>>,
}

impl Script {
    fn new(answers: Vec<Ans>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            ..Self::default()
        })
    }

    fn push(&self, a: Ans) {
        self.answers.lock().unwrap().push_back(a);
    }

    fn tests(&self) -> u32 {
        self.tests.load(Ordering::SeqCst)
    }

    fn answer(&self, what: &'static str) -> Result<(), WriteError> {
        self.order.lock().unwrap().push(what);
        let next = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ans::Took);
        let err = |step: &'static str, errno: i32| WriteError {
            step,
            errno: Some(errno),
            text: std::io::Error::from_raw_os_error(errno).to_string(),
            test_switch: false,
        };
        match next {
            Ans::Took => Ok(()),
            Ans::Errno(n) => Err(err("write", n)),
            Ans::RemoveFailed => Err(err("remove", 28)),
            Ans::Hold(gate) => {
                let (open, cv) = &*gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = cv.wait(open).unwrap();
                }
                Ok(())
            }
        }
    }
}

impl StorageWrites for Script {
    fn test(&self, _run_dir: &Path) -> Result<(), WriteError> {
        self.tests.fetch_add(1, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_at_once.fetch_max(now, Ordering::SeqCst);
        let done = self.answer("test");
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        done
    }

    fn marker(&self, _run_dir: &Path, _bytes: &[u8]) -> Result<(), WriteError> {
        self.markers.fetch_add(1, Ordering::SeqCst);
        self.answer("marker")
    }
}

/// A size the test gives, and how often it was asked for.
#[derive(Debug)]
struct Sized {
    used: Mutex<(Option<u64>, bool)>,
    walks: AtomicU32,
}

impl Sized {
    fn new(used: Option<u64>, partial: bool) -> Arc<Self> {
        Arc::new(Self {
            used: Mutex::new((used, partial)),
            walks: AtomicU32::new(0),
        })
    }

    fn walks(&self) -> u32 {
        self.walks.load(Ordering::SeqCst)
    }
}

impl DiskProbe for Sized {
    fn measure(&self) -> BoxFuture<'_, Reading> {
        Box::pin(async { Reading::default() })
    }

    fn storage_used(&self) -> BoxFuture<'_, (Option<u64>, bool)> {
        Box::pin(async move {
            self.walks.fetch_add(1, Ordering::SeqCst);
            *self.used.lock().unwrap()
        })
    }
}

struct Rig {
    watch: StorageWatch,
    task: Option<WatchTask>,
    script: Arc<Script>,
    size: Arc<Sized>,
    rec: Arc<EventRecorder>,
    log: Arc<MemoryLog>,
    seen: Vec<Envelope>,
}

fn rig_with(answers: Vec<Ans>, size: Arc<Sized>, limits: Limits, test_refusal: bool) -> Rig {
    let script = Script::new(answers);
    let rec = EventRecorder::new(RUN, "direct");
    let log = MemoryLog::new();
    let watch = StorageWatch::new(WatchSetup {
        writes: script.clone(),
        probe: size.clone(),
        rec: rec.clone(),
        log: log.clone(),
        run_id: RUN.to_string(),
        session: 1,
        run_dir: common::scratch("b881-watch"),
        limits,
        test_refusal,
    });
    let task = Some(watch.start());
    Rig {
        watch,
        task,
        script,
        size,
        rec,
        log,
        seen: Vec::new(),
    }
}

fn rig(answers: Vec<Ans>) -> Rig {
    rig_with(
        answers,
        Sized::new(Some(165), false),
        Limits::default(),
        false,
    )
}

impl Rig {
    /// One wake, and the wait until the task made its test write and applied the result.
    async fn wake(&mut self, seen: Seen) {
        let before = self.script.tests();
        self.watch.wake(seen);
        self.until("the test write", |r| r.script.tests() > before)
            .await;
        self.settle().await;
    }

    /// Waits until every test write the task was asked for is made and its result applied, with the notice of a first refusal
    /// recorded. `idle` counts the wakes the task answered; `settled` alone returns while the files of a first notice are
    /// measured, because that walk runs with the lock free.
    async fn settle(&self) {
        self.watch.idle().await;
    }

    async fn until(&mut self, what: &str, f: impl Fn(&Rig) -> bool) {
        for _ in 0..4000 {
            if f(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("{what} did not happen; log: {:?}", self.log.lines());
    }

    /// The storage notices recorded so far, with their event ids.
    fn notices(&mut self) -> Vec<(String, Value)> {
        self.seen.extend(self.rec.drain());
        self.seen
            .iter()
            .filter_map(|e| match &e.payload {
                Payload::Inline { value }
                    if e.event_type == EventType::TurnProgress
                        && value["kind"] == "notify"
                        && value["reason"] == "storage_refused" =>
                {
                    Some((e.event_id.clone(), value.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn log_has(&self, words: &str) -> bool {
        self.log.lines().iter().any(|l| l.contains(words))
    }

    async fn stop(mut self) {
        if let Some(t) = self.task.take() {
            t.stop().await;
        }
    }
}

fn copy(count: u32) -> CopyRef {
    CopyRef {
        session: 1,
        count,
        at: format!("2026-11-21T00:3{count}:12Z"),
        head_sha: format!("{count:040x}"),
        version_id: format!("version-of-copy-{count}"),
    }
}

/// The sentence every form of the notice ends with.
fn newest() -> String {
    format!(
        "ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are."
    )
}

// ---- tests 1 and 2: one notice per stretch --------------------------------------------------------------------------------------

#[tokio::test]
async fn b881_a_refused_test_write_opens_one_stretch_and_a_second_refusal_says_nothing() {
    let mut r = rig(vec![Ans::Errno(28), Ans::Errno(28), Ans::Errno(28)]);
    r.wake(Seen::ToolResult).await;
    let first = r.notices();
    assert_eq!(first.len(), 1, "{:?}", r.log.lines());
    assert_eq!(first[0].1["storage"]["stretch"], 1);
    assert_eq!(first[0].1["storage"]["seen"], "tool");
    // More refused tests while the stretch is open bring nothing new.
    r.wake(Seen::PromptBoundary).await;
    r.wake(Seen::ToolResult).await;
    assert_eq!(r.notices().len(), 1);
    assert_eq!(r.size.walks(), 1, "the files are measured once");
    assert_eq!(r.script.tests(), 3);
    r.stop().await;
}

#[tokio::test]
async fn b881_a_write_that_worked_closes_the_stretch_and_the_next_refusal_is_a_short_notice_with_the_first_command()
 {
    let mut r = rig(vec![
        Ans::Errno(28),
        Ans::Took,
        Ans::Errno(28),
        Ans::Took,
        Ans::Errno(28),
    ]);
    r.wake(Seen::PromptBoundary).await;
    r.wake(Seen::PromptBoundary).await;
    assert!(
        r.log_has("storage takes writes again after") && r.log_has("stretch 1 closed"),
        "{:?}",
        r.log.lines()
    );
    r.wake(Seen::ToolResult).await;
    let n = r.notices();
    assert_eq!(n.len(), 2);
    let first_id = n[0].0.clone();
    assert_eq!(n[0].1["storage"]["seen"], "prompt");
    assert!(n[0].1["storage"].get("first").is_none());
    assert_eq!(n[1].1["storage"]["stretch"], 2);
    assert_eq!(n[1].1["storage"]["first"], first_id.as_str());
    assert!(
        n[1].1["storage"].get("usedMb").is_none(),
        "a later notice measures nothing"
    );
    assert_eq!(
        n[1].1["detail"],
        format!(
            "The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session. This session had saved no copy before ham first saw it. {}",
            newest()
        )
    );
    // A third stretch counts on.
    r.wake(Seen::ToolResult).await;
    r.wake(Seen::ToolResult).await;
    let n = r.notices();
    assert_eq!(n.len(), 3);
    assert!(
        n[2].1["detail"]
            .as_str()
            .unwrap()
            .contains("ham has seen this 3 times in this session")
    );
    assert_eq!(n[2].1["storage"]["first"], first_id.as_str());
    assert_eq!(r.size.walks(), 1, "one walk in the session");
    r.stop().await;
}

// ---- tests 3 and 4: what is no refusal ------------------------------------------------------------------------------------------

#[tokio::test]
async fn b881_only_errno_28_is_a_refusal() {
    let mut r = rig(vec![
        Ans::Errno(30),
        Ans::Errno(13),
        Ans::Errno(21),
        Ans::RemoveFailed,
    ]);
    for _ in 0..4 {
        r.wake(Seen::ToolResult).await;
    }
    assert!(r.notices().is_empty(), "{:?}", r.notices());
    let lines: Vec<String> = r
        .log
        .lines()
        .into_iter()
        .filter(|l| l.contains("test write inconclusive"))
        .collect();
    assert_eq!(lines.len(), 4, "{:?}", r.log.lines());
    assert!(
        lines[0].starts_with("direct turn storage: test write inconclusive (write: "),
        "{}",
        lines[0]
    );
    assert!(
        lines[3].starts_with("direct turn storage: test write inconclusive (remove: "),
        "{}",
        lines[3]
    );
    assert_eq!(r.size.walks(), 0);
    // And errno 28 from the write is one.
    r.script.push(Ans::Errno(28));
    r.wake(Seen::ToolResult).await;
    assert_eq!(r.notices().len(), 1);
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn b881_a_test_that_got_no_answer_is_no_refusal_and_no_second_test_starts_meanwhile() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let r = rig(vec![Ans::Hold(gate.clone())]);
    r.watch.wake(Seen::PromptBoundary);
    // The clock is paused: the write is waited for with real time, and the limit is passed by hand.
    let started = |r: &Rig| r.script.tests() == 1;
    for _ in 0..5000 {
        if started(&r) {
            break;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(started(&r), "the test write did not start");
    tokio::time::advance(Duration::from_secs(6)).await;
    for _ in 0..5000 {
        if r.log_has("no answer within 5 s") {
            break;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        r.log_has("direct turn storage: test write inconclusive (no answer within 5 s)"),
        "{:?}",
        r.log.lines()
    );
    // While that write is out, a wake and the stop path start no second one.
    assert_eq!(
        r.watch.test_now(Seen::EngineError).await,
        StorageWrite::NoAnswer
    );
    r.watch.wake(Seen::ToolResult);
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(r.script.tests(), 1, "a second test write started");
    assert_eq!(
        r.rec.drain().len(),
        0,
        "no event for a test that got no answer"
    );
    // The write ends; the next wake tests again.
    {
        let (open, cv) = &*gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
    }
    for _ in 0..5000 {
        r.watch.wake(Seen::ToolResult);
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
        if r.script.tests() >= 2 {
            break;
        }
    }
    assert!(
        r.script.tests() >= 2,
        "no test write after the first one ended"
    );
    r.stop().await;
}

// ---- tests 5 and 6: the copy named, and the order of the results -----------------------------------------------------------------

#[tokio::test]
async fn b881_the_copy_named_is_frozen_at_the_first_refusal() {
    // Copies 1 and 2 are marked; a refusal; the storage heals; copy 3 is marked after it; a second refusal.
    let mut r = rig(vec![
        Ans::Took,
        Ans::Took,
        Ans::Errno(28),
        Ans::Took,
        Ans::Took,
        Ans::Errno(28),
    ]);
    r.watch
        .marker_write(b"{}".to_vec(), Some(copy(1)))
        .await
        .unwrap();
    r.watch
        .marker_write(b"{}".to_vec(), Some(copy(2)))
        .await
        .unwrap();
    r.wake(Seen::ToolResult).await;
    r.wake(Seen::PromptBoundary).await;
    r.watch
        .marker_write(b"{}".to_vec(), Some(copy(3)))
        .await
        .unwrap();
    r.wake(Seen::ToolResult).await;
    let n = r.notices();
    assert_eq!(n.len(), 2, "{:?}", r.log.lines());
    for (_, v) in &n {
        assert_eq!(
            v["storage"]["copyBefore"],
            serde_json::json!({
                "session": 1, "count": 2, "at": "2026-11-21T00:32:12Z",
                "headSha": format!("{:040x}", 2), "versionId": "version-of-copy-2",
            })
        );
    }
    let first_id = n[0].0.clone();
    assert!(
        n[0].1["detail"].as_str().unwrap().contains(&format!(
            "Copy 2 (2026-11-21 00:32 UTC) is the last copy saved before ham first saw this: ham pull {RUN} --before-refusal {first_id} downloads it."
        )),
        "{}",
        n[0].1["detail"]
    );
    assert!(
        n[1].1["detail"].as_str().unwrap().contains(&format!(
            "Copy 2 (2026-11-21 00:32 UTC) is still the last copy saved before ham first saw it: ham pull {RUN} --before-refusal {first_id} downloads it."
        )),
        "{}",
        n[1].1["detail"]
    );
    for (_, v) in &n {
        let d = v["detail"].as_str().unwrap();
        assert!(
            !d.contains("intact") && !d.contains("before the refusal began"),
            "{d}"
        );
    }
    r.stop().await;
}

#[tokio::test]
async fn b881_a_refused_marker_write_opens_a_stretch_and_its_copy_is_never_named() {
    let mut r = rig(vec![Ans::Took, Ans::Errno(28)]);
    r.watch
        .marker_write(b"{}".to_vec(), Some(copy(1)))
        .await
        .unwrap();
    let refused = r
        .watch
        .marker_write(b"{}".to_vec(), Some(copy(2)))
        .await
        .unwrap_err();
    assert_eq!(refused.errno, Some(28));
    let n = r.notices();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].1["storage"]["seen"], "copy");
    assert_eq!(n[0].1["storage"]["copyBefore"]["count"], 1);
    r.stop().await;
}

#[tokio::test]
async fn b881_results_are_applied_in_the_order_the_writes_ran() {
    // A test write is held on its thread while a marker write is asked for. The marker write does not start before the test write
    // ended and its result was applied: one lock is held from before a write until its result is applied.
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let mut r = rig_with(
        vec![Ans::Hold(gate.clone())],
        Sized::new(Some(165), false),
        Limits::default(),
        false,
    );
    r.watch.wake(Seen::ToolResult);
    r.until("the test write", |r| r.script.tests() == 1).await;
    let watch = r.watch.clone();
    let marker =
        tokio::spawn(async move { watch.marker_write(b"{}".to_vec(), Some(copy(7))).await });
    for _ in 0..50 {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        r.script.markers.load(Ordering::SeqCst),
        0,
        "the marker write ran while the test write was out"
    );
    {
        let (open, cv) = &*gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
    }
    marker.await.unwrap().unwrap();
    assert_eq!(*r.script.order.lock().unwrap(), vec!["test", "marker"]);
    r.stop().await;
}

// ---- test 7: the words --------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b881_the_notice_names_its_own_id_in_its_command_carries_the_numbers_and_raises_no_ladder_event()
 {
    let mut r = rig(vec![Ans::Took, Ans::Errno(28)]);
    r.watch
        .marker_write(b"{}".to_vec(), Some(copy(2)))
        .await
        .unwrap();
    r.wake(Seen::ToolResult).await;
    let n = r.notices();
    assert_eq!(n.len(), 1);
    let (id, v) = &n[0];
    assert_eq!(id.chars().count(), 26, "the event id is the selector: {id}");
    assert_eq!(
        v["detail"],
        format!(
            "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device). On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up. In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again. A file that a command wrote meanwhile can be empty; ham does not know which. Copy 2 (2026-11-21 00:32 UTC) is the last copy saved before ham first saw this: ham pull {RUN} --before-refusal {id} downloads it. ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are."
        )
    );
    assert_eq!(v["turn"], harness_protocol::direct::DIRECT_TURN);
    assert_eq!(v["kind"], "notify");
    assert_eq!(v["reason"], "storage_refused");
    assert!(v["outageMinutes"].is_null());
    let s = &v["storage"];
    assert_eq!(s["session"], 1);
    assert_eq!(s["stretch"], 1);
    assert_eq!(s["usedMb"], 165);
    assert_eq!(s["partial"], false);
    assert_eq!(s["quotaMb"], 1024);
    assert!(s.get("testSwitch").is_none());
    // The notice is one event of the run and nothing else: the watch holds no module of the seam, so no ladder event can follow it.
    assert_eq!(r.seen.len(), 1, "{:?}", r.seen);
    assert_eq!(r.seen[0].event_type, EventType::TurnProgress);
    assert!(r.log_has("direct turn storage refused a write (seen at a tool result): no space left on device; stretch 1, files 165 MiB"), "{:?}", r.log.lines());
    r.stop().await;
}

// ---- tests 8 and 9: a notice needs room, and what the size says ------------------------------------------------------------------

#[tokio::test]
async fn b881_files_at_the_warning_mark_bring_no_storage_notice() {
    // The early-warning mark of the disk rule: 90 percent of the stop mark of 960 MiB.
    assert_eq!(Limits::default().storage_warn_mb(), 864);
    let mut below = rig_with(
        vec![Ans::Errno(28)],
        Sized::new(Some(863), false),
        Limits::default(),
        false,
    );
    below.wake(Seen::ToolResult).await;
    assert_eq!(below.notices().len(), 1);
    below.stop().await;

    let mut at = rig_with(
        vec![Ans::Errno(28), Ans::Errno(28), Ans::Took, Ans::Errno(28)],
        Sized::new(Some(864), false),
        Limits::default(),
        false,
    );
    at.wake(Seen::ToolResult).await;
    assert!(at.notices().is_empty());
    assert!(
        at.log_has("direct turn storage refused a write while its files add up to 864 MiB: the disk notices cover it"),
        "{:?}",
        at.log.lines()
    );
    // More refusals in that stretch say and measure nothing; the next stretch measures again.
    at.wake(Seen::ToolResult).await;
    assert_eq!(at.size.walks(), 1);
    at.wake(Seen::ToolResult).await;
    at.wake(Seen::ToolResult).await;
    assert_eq!(at.size.walks(), 2);
    assert!(at.notices().is_empty());
    at.stop().await;

    // With the workspace check switched off the default mark decides.
    let off = Limits {
        root_low_mb: 512,
        workspace_limit_mb: 0,
    };
    assert_eq!(off.storage_warn_mb(), 864);
    let mut r = rig_with(
        vec![Ans::Errno(28)],
        Sized::new(Some(864), false),
        off,
        false,
    );
    r.wake(Seen::ToolResult).await;
    assert!(r.notices().is_empty());
    r.stop().await;
    let mut r = rig_with(
        vec![Ans::Errno(28)],
        Sized::new(Some(863), false),
        off,
        false,
    );
    r.wake(Seen::ToolResult).await;
    assert_eq!(r.notices().len(), 1);
    r.stop().await;
}

#[tokio::test]
async fn b881_a_walk_cut_short_reads_at_least_and_an_unmeasured_size_is_said_so() {
    let mut cut = rig_with(
        vec![Ans::Errno(28)],
        Sized::new(Some(165), true),
        Limits::default(),
        false,
    );
    cut.wake(Seen::ToolResult).await;
    let n = cut.notices();
    assert!(
        n[0].1["detail"].as_str().unwrap().starts_with(
            "The session's storage refused a write while its files add up to at least 165 MiB of 1024 MiB (no space left on device)."
        ),
        "{}",
        n[0].1["detail"]
    );
    assert_eq!(n[0].1["storage"]["partial"], true);
    assert_eq!(n[0].1["storage"]["usedMb"], 165);
    cut.stop().await;
    // A lower bound at the mark is at the mark.
    let mut high = rig_with(
        vec![Ans::Errno(28)],
        Sized::new(Some(900), true),
        Limits::default(),
        false,
    );
    high.wake(Seen::ToolResult).await;
    assert!(high.notices().is_empty());
    high.stop().await;

    let mut none = rig_with(
        vec![Ans::Errno(28)],
        Sized::new(None, false),
        Limits::default(),
        false,
    );
    none.wake(Seen::ToolResult).await;
    let n = none.notices();
    assert!(
        n[0].1["detail"].as_str().unwrap().starts_with(
            "The session's storage refused a write (no space left on device); the size of its files could not be measured. On this storage a refused write with room left has meant"
        ),
        "{}",
        n[0].1["detail"]
    );
    assert!(n[0].1["storage"].get("usedMb").is_none());
    none.stop().await;
}

// ---- test 10: every form names the newest copy -----------------------------------------------------------------------------------

#[tokio::test]
async fn b881_every_form_of_the_notice_names_the_command_for_the_newest_copy() {
    let mut details: Vec<String> = Vec::new();
    // Full with a copy, then short with a copy.
    let mut a = rig(vec![Ans::Took, Ans::Errno(28), Ans::Took, Ans::Errno(28)]);
    a.watch
        .marker_write(b"{}".to_vec(), Some(copy(1)))
        .await
        .unwrap();
    a.wake(Seen::ToolResult).await;
    a.wake(Seen::ToolResult).await;
    a.wake(Seen::ToolResult).await;
    details.extend(
        a.notices()
            .into_iter()
            .map(|(_, v)| v["detail"].as_str().unwrap().to_string()),
    );
    a.stop().await;
    // Full without a copy, then short without a copy.
    let mut b = rig(vec![Ans::Errno(28), Ans::Took, Ans::Errno(28)]);
    for _ in 0..3 {
        b.wake(Seen::PromptBoundary).await;
    }
    details.extend(
        b.notices()
            .into_iter()
            .map(|(_, v)| v["detail"].as_str().unwrap().to_string()),
    );
    b.stop().await;
    // No size.
    let mut c = rig_with(
        vec![Ans::Errno(28)],
        Sized::new(None, false),
        Limits::default(),
        false,
    );
    c.wake(Seen::ToolResult).await;
    details.extend(
        c.notices()
            .into_iter()
            .map(|(_, v)| v["detail"].as_str().unwrap().to_string()),
    );
    c.stop().await;
    assert_eq!(details.len(), 5);
    for d in &details {
        assert!(d.ends_with(&newest()), "{d}");
        assert!(!d.contains('\n'));
    }
    assert!(
        details[2].contains("This session had saved no copy before ham first saw this."),
        "{}",
        details[2]
    );
}

// ---- the cost: one test write per wake, never two at once (section 7, measurement 3) ----------------------------------------------

#[tokio::test]
async fn b881_one_test_write_per_wake_and_never_two_at_once() {
    let mut r = rig(Vec::new());
    for i in 0..50 {
        r.wake(if i % 2 == 0 {
            Seen::ToolResult
        } else {
            Seen::PromptBoundary
        })
        .await;
    }
    assert_eq!(r.script.tests(), 50);
    assert_eq!(r.watch.test_writes_started(), 50);
    assert_eq!(r.script.most_at_once.load(Ordering::SeqCst), 1);
    // Wakes that arrive faster than the task runs are one test write, never one each.
    for _ in 0..200 {
        r.watch.wake(Seen::ToolResult);
    }
    r.until("the task", |r| r.script.tests() > 50).await;
    r.settle().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    r.settle().await;
    assert!(
        r.script.tests() <= 52,
        "{} test writes for one burst of wakes",
        r.script.tests()
    );
    assert_eq!(r.script.most_at_once.load(Ordering::SeqCst), 1);
    assert_eq!(r.size.walks(), 0, "no walk without a refusal");
    assert!(r.notices().is_empty());
    r.stop().await;
}

/// Wakes that arrive before the task ran are one test write. When a tool result that names the words finds the test write of
/// a prompt boundary pending, that write is made after the result and answers it too: the log says that the storage took it, as it
/// does for a write a tool result asked for. The live row's leg `quote` stands on either line. A prompt boundary alone says nothing,
/// and a refusal keeps the word of what asked first.
#[tokio::test]
async fn b884_a_tool_result_that_finds_the_test_write_of_a_prompt_boundary_pending_gets_its_line() {
    let worked = |log: &MemoryLog| -> Vec<String> {
        log.lines()
            .into_iter()
            .filter(|l| l.contains("test write to the session's storage worked"))
            .collect()
    };
    let mut r = rig(Vec::new());
    // Both wakes arrive before the task runs: one test write, which the prompt boundary asked for.
    r.watch.wake(Seen::PromptBoundary);
    r.watch.wake(Seen::ToolResult);
    r.until("the test write", |r| r.script.tests() == 1).await;
    r.settle().await;
    assert_eq!(
        r.script.tests(),
        1,
        "B884 TRIGGER ABSENT: the two wakes were two test writes"
    );
    println!(
        "B884 TRIGGER PRESENT: the wake of the tool result joined the pending test write of the prompt boundary"
    );
    assert_eq!(
        worked(&r.log),
        vec![
            "direct turn storage: a tool result named no space left on device while the test write of a prompt boundary had not started; that test write to the session's storage worked"
        ],
        "{:?}",
        r.log.lines()
    );
    assert!(r.notices().is_empty());
    // A prompt boundary alone: its write is taken and the log says nothing.
    r.wake(Seen::PromptBoundary).await;
    assert_eq!(r.script.tests(), 2);
    assert_eq!(worked(&r.log).len(), 1, "{:?}", r.log.lines());
    // The other order: the tool result asked first and the prompt boundary joined. The line is the one of a tool result.
    r.watch.wake(Seen::ToolResult);
    r.watch.wake(Seen::PromptBoundary);
    r.until("the test write", |r| r.script.tests() == 3).await;
    r.settle().await;
    assert_eq!(r.script.tests(), 3);
    assert_eq!(
        worked(&r.log)[1..],
        [
            "direct turn storage: a tool result named no space left on device; a test write to the session's storage worked"
        ],
        "{:?}",
        r.log.lines()
    );
    // A write the storage refuses says nothing of the kind, and its notice keeps the word of what asked first.
    r.script.push(Ans::Errno(28));
    r.watch.wake(Seen::PromptBoundary);
    r.watch.wake(Seen::ToolResult);
    r.until("the test write", |r| r.script.tests() == 4).await;
    r.settle().await;
    let n = r.notices();
    assert_eq!(n.len(), 1, "{:?}", r.log.lines());
    assert_eq!(n[0].1["storage"]["seen"], "prompt");
    assert_eq!(worked(&r.log).len(), 2, "{:?}", r.log.lines());
    r.stop().await;
}

// ---- the switch of the live test row (the lead's ruling 2) -------------------------------------------------------------------------

#[tokio::test]
async fn b881_the_test_switch_answers_one_tool_result_test_and_never_again() {
    let mut r = rig_with(
        Vec::new(),
        Sized::new(Some(12), false),
        Limits::default(),
        true,
    );
    // A prompt boundary is not answered by the switch: its write is the real one.
    r.wake(Seen::PromptBoundary).await;
    assert_eq!(r.script.tests(), 1);
    assert!(r.notices().is_empty());
    // The first test that a tool result asks for once a copy of the session is marked is answered by the switch, without a write.
    r.watch
        .marker_write(b"{}".to_vec(), Some(copy(1)))
        .await
        .unwrap();
    r.watch.wake(Seen::ToolResult);
    r.until("the notice", |r| {
        r.log_has("the stage's test switch answered this test write")
    })
    .await;
    r.settle().await;
    assert_eq!(r.script.tests(), 1, "the switch touched the storage");
    let n = r.notices();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].1["storage"]["testSwitch"], true);
    assert!(
        n[0].1["detail"].as_str().unwrap().ends_with(
            "This notice was made by the stage's test switch; the storage refused no write."
        ),
        "{}",
        n[0].1["detail"]
    );
    assert!(
        r.log_has("direct turn storage: the stage's test switch answered this test write with no space left on device; the storage refused no write"),
        "{:?}",
        r.log.lines()
    );
    // Never again in the session: every later test is the real write, which closes the stretch.
    for _ in 0..5 {
        r.wake(Seen::ToolResult).await;
    }
    assert_eq!(r.script.tests(), 6);
    assert_eq!(r.notices().len(), 1);
    r.stop().await;
}

#[tokio::test]
async fn b881_without_the_switch_a_tool_result_is_tested_on_the_storage() {
    let mut r = rig(Vec::new());
    r.wake(Seen::ToolResult).await;
    assert_eq!(r.script.tests(), 1);
    assert!(r.notices().is_empty());
    assert!(
        r.log_has("direct turn storage: a tool result named no space left on device; a test write to the session's storage worked"),
        "{:?}",
        r.log.lines()
    );
    r.stop().await;
}

// ---- part 2: the copy task writes its marker through the watch ---------------------------------------------------------------------

#[tokio::test]
async fn b881_the_test_switch_waits_for_the_first_copy_whose_marker_the_storage_took() {
    let mut r = rig_with(
        Vec::new(),
        Sized::new(Some(12), false),
        Limits::default(),
        true,
    );
    // No copy of the session is marked yet: a tool result is tested on the storage, as without the switch.
    r.wake(Seen::ToolResult).await;
    assert_eq!(r.script.tests(), 1);
    assert!(r.notices().is_empty());
    assert!(!r.log_has("test switch"), "{:?}", r.log.lines());
    // The marker of the session's start names no copy: the switch still waits.
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::Start)
        .await
        .unwrap();
    r.wake(Seen::ToolResult).await;
    assert_eq!(r.script.tests(), 2);
    assert!(r.notices().is_empty());
    // The copy task marks a copy through the watch. The next tool result is answered by the switch, and the notice names that copy
    // with the version the bucket gave it.
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::Copy(copy(1)))
        .await
        .unwrap();
    r.watch.wake(Seen::ToolResult);
    r.until("the notice", |r| {
        r.log_has("the stage's test switch answered this test write")
    })
    .await;
    r.settle().await;
    assert_eq!(r.script.tests(), 2, "the switch touched the storage");
    let n = r.notices();
    assert_eq!(n.len(), 1);
    let s = &n[0].1["storage"];
    assert_eq!(s["testSwitch"], true);
    assert_eq!(s["seen"], "tool");
    assert_eq!(s["copyBefore"]["session"], 1);
    assert_eq!(s["copyBefore"]["count"], 1);
    assert_eq!(s["copyBefore"]["versionId"], "version-of-copy-1");
    assert_eq!(s["copyBefore"]["headSha"], format!("{:040x}", 1));
    r.stop().await;
}

#[tokio::test]
async fn b881_a_copy_the_bucket_gave_no_version_is_not_named_and_neither_is_the_copy_before_it() {
    // Copy 1 has a version; copy 2 replaced it at the key and the bucket answered no version for it. Copy 1 is then no longer the
    // last copy saved, and copy 2 cannot be asked for: the notice names none, and still names the command for the newest copy.
    let mut r = rig(vec![Ans::Took, Ans::Took, Ans::Errno(28)]);
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::Copy(copy(1)))
        .await
        .unwrap();
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::Unnamed)
        .await
        .unwrap();
    r.wake(Seen::ToolResult).await;
    let n = r.notices();
    assert_eq!(n.len(), 1);
    assert!(n[0].1["storage"].get("copyBefore").is_none(), "{}", n[0].1);
    let detail = n[0].1["detail"].as_str().unwrap();
    assert!(!detail.contains("--before-refusal"), "{detail}");
    assert!(detail.ends_with(&newest()), "{detail}");
    r.stop().await;
}

#[tokio::test]
async fn b881_the_notice_says_what_is_known_about_this_storage_and_never_that_ham_cannot_read_why()
{
    // The lead's ruling 9: the second measurement round found the storage service's own words for a refused write with room left.
    let mut r = rig(vec![Ans::Errno(28)]);
    r.wake(Seen::PromptBoundary).await;
    let n = r.notices();
    assert_eq!(n.len(), 1);
    let detail = n[0].1["detail"].as_str().unwrap();
    assert!(
        detail.contains("(no space left on device). On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up. In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again. A file that a command wrote meanwhile can be empty; ham does not know which."),
        "{detail}"
    );
    assert!(!detail.contains("cannot read why"), "{detail}");
    r.stop().await;
}

// ----: the marker of a session's start has a word of its own -------------------------------------------------------------------

#[tokio::test]
async fn b884_a_refused_start_marker_brings_the_full_notice_with_the_word_start() {
    // The first write of a session through the watch is the marker of its start. The storage refuses it: the notice is the full one,
    // it names no copy, and its `seen` member says that the marker of the start met the refusal and no copy's marker.
    let mut r = rig(vec![Ans::Errno(28), Ans::Took, Ans::Errno(28)]);
    let refused = r
        .watch
        .marker_write_for(b"{}".to_vec(), Marked::Start)
        .await
        .unwrap_err();
    assert_eq!(refused.errno, Some(28));
    let n = r.notices();
    assert_eq!(n.len(), 1, "{:?}", r.log.lines());
    let s = &n[0].1["storage"];
    assert_eq!(s["seen"], "start");
    assert_eq!(s["stretch"], 1);
    assert_eq!(s["usedMb"], 165);
    assert!(s.get("copyBefore").is_none(), "{}", n[0].1);
    assert!(
        r.log_has(
            "direct turn storage refused a write (seen at the marker of the session's start)"
        ),
        "{:?}",
        r.log.lines()
    );
    // The marker of a stored copy keeps its word: the storage takes the first one, which closes the stretch, and refuses the next.
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::Copy(copy(1)))
        .await
        .unwrap();
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::Copy(copy(2)))
        .await
        .unwrap_err();
    let n = r.notices();
    assert_eq!(n.len(), 2, "{:?}", r.log.lines());
    assert_eq!(n[1].1["storage"]["seen"], "copy");
    assert_eq!(n[1].1["storage"]["stretch"], 2);
    // The copy a notice names was frozen at the session's first refusal, when none was marked.
    assert!(n[1].1["storage"].get("copyBefore").is_none(), "{}", n[1].1);
    r.stop().await;
}

// ----: the walk of the first notice runs with the lock free, under a limit of its own -------------------------------------------

/// A size whose walk does not answer until the test lets it go.
#[derive(Debug)]
struct HungWalk {
    used: (Option<u64>, bool),
    walks: AtomicU32,
    let_go: tokio::sync::Notify,
}

impl HungWalk {
    fn new(used: Option<u64>, partial: bool) -> Arc<Self> {
        Arc::new(Self {
            used: (used, partial),
            walks: AtomicU32::new(0),
            let_go: tokio::sync::Notify::new(),
        })
    }

    fn walks(&self) -> u32 {
        self.walks.load(Ordering::SeqCst)
    }
}

impl DiskProbe for HungWalk {
    fn measure(&self) -> BoxFuture<'_, Reading> {
        Box::pin(async { Reading::default() })
    }

    fn storage_used(&self) -> BoxFuture<'_, (Option<u64>, bool)> {
        Box::pin(async move {
            self.walks.fetch_add(1, Ordering::SeqCst);
            self.let_go.notified().await;
            self.used
        })
    }
}

/// The rig with a walk that hangs in place of the scripted size.
fn rig_walk(answers: Vec<Ans>, walk: Arc<HungWalk>) -> Rig {
    let script = Script::new(answers);
    let rec = EventRecorder::new(RUN, "direct");
    let log = MemoryLog::new();
    let watch = StorageWatch::new(WatchSetup {
        writes: script.clone(),
        probe: walk,
        rec: rec.clone(),
        log: log.clone(),
        run_id: RUN.to_string(),
        session: 1,
        run_dir: common::scratch("b884-watch"),
        limits: Limits::default(),
        test_refusal: false,
    });
    let task = Some(watch.start());
    Rig {
        watch,
        task,
        script,
        size: Sized::new(None, false),
        rec,
        log,
        seen: Vec::new(),
    }
}

/// With the clock paused: lets the other tasks run and gives the threads of the writes real time, until `f` holds.
async fn in_real_time(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..5000 {
        if f() {
            return;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("{what} did not happen");
}

/// Holds for a marker write and a test write that are made while the walk of another write's first notice is out. The write that
/// is itself the session's first refusal waits for the walk, for at most its limit
/// (`b884_a_write_that_is_the_sessions_first_refusal_waits_for_the_walk_for_its_limit_at_the_most`).
#[tokio::test(start_paused = true)]
async fn b884_a_walk_that_hangs_delays_neither_a_marker_write_nor_the_test_write_after_an_engine_error()
 {
    let walk = HungWalk::new(Some(165), false);
    let mut r = rig_walk(vec![Ans::Errno(28)], walk.clone());
    // The session's first refusal: the watch's task measures the files for the notice, and the walk does not answer.
    r.watch.wake(Seen::ToolResult);
    in_real_time("the walk of the first notice", || walk.walks() == 1).await;
    println!(
        "B884 TRIGGER PRESENT: the walk of the session's first notice is out and does not answer"
    );
    // The copy task's marker write and the test write after an engine error are made meanwhile. The clock is paused: a call that
    // waits for the walk has nothing else to wait for and meets the limit below at once. The limit is below the walk's own, so a
    // call that waits for the walk fails the assertion that names the delay, and not a later one.
    let wait = Duration::from_secs(1);
    assert!(wait < TREE_ANSWER_LIMIT);
    let marker =
        tokio::time::timeout(wait, r.watch.marker_write(b"{}".to_vec(), Some(copy(7)))).await;
    assert!(
        marker.is_ok(),
        "the marker write waited for the walk of the files; log: {:?}",
        r.log.lines()
    );
    marker.unwrap().unwrap();
    let test = tokio::time::timeout(wait, r.watch.test_now(Seen::EngineError)).await;
    assert_eq!(
        test,
        Ok(StorageWrite::Took),
        "the test write after an engine error waited for the walk of the files; log: {:?}",
        r.log.lines()
    );
    assert_eq!(
        *r.script.order.lock().unwrap(),
        vec!["test", "marker", "test"]
    );
    // Both were made while the walk was out: the notice is not recorded yet.
    assert_eq!(walk.walks(), 1);
    assert_eq!(
        r.notices().len(),
        0,
        "the first notice was recorded before the walk answered"
    );
    // The walk answers: the notice is the full one with the size, for stretch 1. The storage took the marker write meanwhile, so
    // the stretch of that notice is closed, and the line of the notice says so.
    walk.let_go.notify_one();
    in_real_time("the first notice", || !r.notices().is_empty()).await;
    assert!(
        r.log_has(
            "stretch 1, files 165 MiB; the storage took a write while the files were measured"
        ),
        "{:?}",
        r.log.lines()
    );
    let n = r.notices();
    assert_eq!(n.len(), 1, "{:?}", r.log.lines());
    let first_id = n[0].0.clone();
    assert_eq!(n[0].1["storage"]["stretch"], 1);
    assert_eq!(n[0].1["storage"]["seen"], "tool");
    assert_eq!(n[0].1["storage"]["usedMb"], 165);
    // The copy marked while the files were measured was saved after the refusal: no notice names it.
    assert!(n[0].1["storage"].get("copyBefore").is_none(), "{}", n[0].1);
    // The stretch of that notice closed while the files were measured: the next refusal opens stretch 2, with the short notice.
    r.script.push(Ans::Errno(28));
    r.wake(Seen::PromptBoundary).await;
    let n = r.notices();
    assert_eq!(n.len(), 2, "{:?}", r.log.lines());
    assert_eq!(n[1].1["storage"]["stretch"], 2);
    assert_eq!(n[1].1["storage"]["first"], first_id.as_str());
    assert_eq!(walk.walks(), 1, "the files are measured once in a session");
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn b884_a_walk_that_never_answers_is_given_up_at_its_limit_and_the_notice_says_the_size_was_not_measured()
 {
    let walk = HungWalk::new(Some(165), false);
    let mut r = rig_walk(vec![Ans::Errno(28), Ans::Errno(28)], walk.clone());
    r.watch.wake(Seen::ToolResult);
    in_real_time("the walk of the first notice", || walk.walks() == 1).await;
    println!(
        "B884 TRIGGER PRESENT: the walk of the session's first notice is out and never answers"
    );
    // Short of the limit nothing is recorded.
    tokio::time::advance(Duration::from_secs(4)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(r.notices().len(), 0);
    // Past the limit the watch gives the walk up and records the notice without a size.
    tokio::time::advance(Duration::from_secs(2)).await;
    let mut seen = 0;
    for _ in 0..2000 {
        seen = r.notices().len();
        if seen > 0 {
            break;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        seen,
        1,
        "no notice after the limit of the walk had passed; log: {:?}",
        r.log.lines()
    );
    let n = r.notices();
    assert!(
        n[0].1["detail"].as_str().unwrap().starts_with(
            "The session's storage refused a write (no space left on device); the size of its files could not be measured."
        ),
        "{}",
        n[0].1
    );
    assert!(n[0].1["storage"].get("usedMb").is_none(), "{}", n[0].1);
    assert_eq!(n[0].1["storage"]["stretch"], 1);
    assert!(
        r.log_has("files not measured (no answer within 5 s)"),
        "{:?}",
        r.log.lines()
    );
    // The stretch is open and told: a further refusal says nothing and starts no second walk, and a write the storage takes closes
    // stretch 1.
    r.wake(Seen::PromptBoundary).await;
    assert_eq!(r.notices().len(), 1);
    assert_eq!(walk.walks(), 1);
    r.wake(Seen::PromptBoundary).await;
    assert!(r.log_has("stretch 1 closed"), "{:?}", r.log.lines());
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn b884_what_writes_meet_while_the_files_are_measured_belongs_to_the_first_notice() {
    let walk = HungWalk::new(Some(165), false);
    // Refused (the walk starts); then, while the walk is out: refused, taken, refused.
    let mut r = rig_walk(
        vec![Ans::Errno(28), Ans::Errno(28), Ans::Took, Ans::Errno(28)],
        walk.clone(),
    );
    r.watch.wake(Seen::ToolResult);
    in_real_time("the walk of the first notice", || walk.walks() == 1).await;
    println!(
        "B884 TRIGGER PRESENT: the walk of the session's first notice is out and does not answer"
    );
    // Below the walk's own limit: a test write that waits for the walk fails the assertion that names the delay.
    let wait = Duration::from_secs(1);
    assert!(wait < TREE_ANSWER_LIMIT);
    for expected in [
        StorageWrite::Refused,
        StorageWrite::Took,
        StorageWrite::Refused,
    ] {
        let got = tokio::time::timeout(wait, r.watch.test_now(Seen::EngineError)).await;
        assert_eq!(
            got,
            Ok(expected),
            "a test write waited for the walk of the files; log: {:?}",
            r.log.lines()
        );
    }
    assert_eq!(
        walk.walks(),
        1,
        "a second walk started while the first was out"
    );
    assert_eq!(r.notices().len(), 0);
    walk.let_go.notify_one();
    in_real_time("the first notice", || !r.notices().is_empty()).await;
    let n = r.notices();
    assert_eq!(n.len(), 1, "{:?}", r.log.lines());
    assert_eq!(n[0].1["storage"]["stretch"], 1);
    assert_eq!(n[0].1["storage"]["seen"], "tool");
    // The stretch that is open is the one the notice tells of: a further refusal says nothing, and the next write the storage
    // takes closes stretch 1.
    r.script.push(Ans::Errno(28));
    r.wake(Seen::PromptBoundary).await;
    assert_eq!(r.notices().len(), 1, "{:?}", r.log.lines());
    r.wake(Seen::PromptBoundary).await;
    assert!(r.log_has("stretch 1 closed"), "{:?}", r.log.lines());
    assert_eq!(walk.walks(), 1);
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn b884_a_task_that_ends_while_the_files_are_measured_leaves_the_next_refusal_its_walk_and_its_notice()
 {
    let walk = HungWalk::new(Some(165), false);
    let mut r = rig_walk(vec![Ans::Errno(28), Ans::Errno(28)], walk.clone());
    // The session's first refusal: the watch's task measures the files, and the walk does not answer.
    r.watch.wake(Seen::ToolResult);
    in_real_time("the walk of the first notice", || walk.walks() == 1).await;
    println!(
        "B884 TRIGGER PRESENT: the walk of the session's first notice is out and does not answer"
    );
    // The session ends: the task of the watch is stopped while it waits for the walk. No notice was recorded.
    r.task.take().unwrap().stop().await;
    assert_eq!(r.notices().len(), 0);
    // The end sequence writes the copies' marker, and the storage refuses it. The files are measured again (this walk answers),
    // and the notice comes.
    walk.let_go.notify_one();
    let marker = tokio::time::timeout(
        Duration::from_secs(60),
        r.watch.marker_write(b"{}".to_vec(), None),
    )
    .await;
    let refused = marker
        .expect("the marker write waited for a walk that nobody runs")
        .unwrap_err();
    assert!(refused.refused(), "{refused:?}");
    assert_eq!(
        walk.walks(),
        2,
        "the refusal after the task ended did not measure the files; log: {:?}",
        r.log.lines()
    );
    let n = r.notices();
    assert_eq!(n.len(), 1, "{:?}", r.log.lines());
    assert_eq!(n[0].1["storage"]["stretch"], 1);
    assert_eq!(n[0].1["storage"]["seen"], "start");
    assert_eq!(n[0].1["storage"]["usedMb"], 165);
    // Nothing waits for the walk that was given up with its task.
    assert!(
        tokio::time::timeout(Duration::from_secs(60), r.watch.settled())
            .await
            .is_ok(),
        "settled waits for a walk that nobody runs"
    );
    r.stop().await;
}

// ----, after the review of the walk: what the first refusal's own caller waits for, the mark, the end of a session -----------

/// The write that is itself the session's first refusal waits for the walk of the files, and for no longer than the walk's limit
/// ([`TREE_ANSWER_LIMIT`]). Held for the copy task's marker write and for the test write after an engine error, with a walk that
/// never answers.
#[tokio::test(start_paused = true)]
async fn b884_a_write_that_is_the_sessions_first_refusal_waits_for_the_walk_for_its_limit_at_the_most()
 {
    for who in ["the marker write", "the test write after an engine error"] {
        let walk = HungWalk::new(Some(165), false);
        let mut r = rig_walk(vec![Ans::Errno(28)], walk.clone());
        let began = tokio::time::Instant::now();
        // Four times the limit is given, so that a wait past the limit is measured and not cut short.
        let refused = tokio::time::timeout(TREE_ANSWER_LIMIT * 4, async {
            if who == "the marker write" {
                r.watch
                    .marker_write(b"{}".to_vec(), Some(copy(7)))
                    .await
                    .is_err_and(|e| e.refused())
            } else {
                r.watch.test_now(Seen::EngineError).await == StorageWrite::Refused
            }
        })
        .await;
        let waited = began.elapsed();
        assert_eq!(
            refused,
            Ok(true),
            "{who} did not come back as a refusal within four times the walk's limit; log: {:?}",
            r.log.lines()
        );
        assert_eq!(
            walk.walks(),
            1,
            "B884 TRIGGER ABSENT: {who} was not the session's first refusal, no walk started"
        );
        assert!(
            waited > Duration::ZERO,
            "B884 TRIGGER ABSENT: {who} did not wait for the walk of the files"
        );
        println!(
            "B884 TRIGGER PRESENT: {who} is the session's first refusal and waited {waited:?} for a walk that never answers"
        );
        assert!(
            waited <= TREE_ANSWER_LIMIT,
            "{who} waited {waited:?} for the walk of the files, longer than the walk's limit of {TREE_ANSWER_LIMIT:?}"
        );
        // The notice is recorded when the call returns, and it says that the size could not be measured.
        let n = r.notices();
        assert_eq!(n.len(), 1, "{:?}", r.log.lines());
        assert_eq!(
            n[0].1["storage"]["seen"],
            if who == "the marker write" {
                "copy"
            } else {
                "error"
            }
        );
        assert!(n[0].1["storage"].get("usedMb").is_none(), "{}", n[0].1);
        assert!(
            r.log_has("files not measured (no answer within 5 s)"),
            "{:?}",
            r.log.lines()
        );
        r.stop().await;
    }
}

/// With the files at the early-warning mark a refusal brings one log line and no notice, and it opens a stretch only when the
/// storage took no write while the files were measured. After such a write the next refusal is a refusal of its own: it measures
/// the files again and is not taken for a part of a stretch that is open.
#[tokio::test(start_paused = true)]
async fn b884_at_the_warning_mark_a_write_the_storage_took_while_the_files_were_measured_leaves_no_stretch_open()
 {
    let mark = Limits::default().storage_warn_mb();
    let walk = HungWalk::new(Some(mark), false);
    // Refused (the walk starts); taken while the walk is out; refused again after the walk answered.
    let mut r = rig_walk(
        vec![Ans::Errno(28), Ans::Took, Ans::Errno(28)],
        walk.clone(),
    );
    let covered = |log: &MemoryLog| {
        log.lines()
            .iter()
            .filter(|l| l.contains("the disk notices cover it"))
            .count()
    };
    r.watch.wake(Seen::ToolResult);
    in_real_time("the walk of the first refusal", || walk.walks() == 1).await;
    println!(
        "B884 TRIGGER PRESENT: the walk of the session's first refusal is out and does not answer"
    );
    let wait = Duration::from_secs(1);
    assert!(wait < TREE_ANSWER_LIMIT);
    let took = tokio::time::timeout(wait, r.watch.test_now(Seen::EngineError)).await;
    assert_eq!(
        took,
        Ok(StorageWrite::Took),
        "the write the storage takes while the files are measured; log: {:?}",
        r.log.lines()
    );
    assert_eq!(covered(&r.log), 0);
    // The walk answers a size at the mark: the disk rule's own notices cover a storage that is full. One log line, no notice.
    walk.let_go.notify_one();
    in_real_time("the log line of a refusal at the mark", || {
        covered(&r.log) == 1
    })
    .await;
    assert!(
        r.log_has(&format!(
            "direct turn storage refused a write while its files add up to {mark} MiB: the disk notices cover it"
        )),
        "{:?}",
        r.log.lines()
    );
    assert_eq!(r.notices().len(), 0);
    // The storage took a write while the files were measured, so no stretch is open. The next refusal is one of its own: the files
    // are measured again (this walk answers at once) and the log says so again.
    walk.let_go.notify_one();
    r.wake(Seen::PromptBoundary).await;
    assert_eq!(
        walk.walks(),
        2,
        "the refusal after a write the storage took was taken for a part of an open stretch; log: {:?}",
        r.log.lines()
    );
    assert_eq!(covered(&r.log), 2, "{:?}", r.log.lines());
    assert_eq!(r.notices().len(), 0);
    // That second refusal met no write the storage took: its stretch is open. A further refusal says nothing and measures
    // nothing, and the next write the storage takes closes the stretch.
    r.script.push(Ans::Errno(28));
    r.wake(Seen::PromptBoundary).await;
    assert_eq!(walk.walks(), 2);
    assert_eq!(covered(&r.log), 2, "{:?}", r.log.lines());
    r.wake(Seen::PromptBoundary).await;
    assert!(
        r.log_has("direct turn storage takes writes again after"),
        "{:?}",
        r.log.lines()
    );
    assert_eq!(r.notices().len(), 0);
    r.stop().await;
}

/// The end of a session waits, through `idle`, for the test writes the watch's own task was asked for. A walk of the files that
/// another caller started (here a marker write that is the session's first refusal) is that caller's wait: `settled` and `idle`
/// return while it is out, and the caller gets its notice when the walk answers.
#[tokio::test(start_paused = true)]
async fn b884_the_end_of_a_session_does_not_wait_for_a_walk_that_another_caller_started() {
    let walk = HungWalk::new(Some(165), false);
    let mut r = rig_walk(vec![Ans::Errno(28)], walk.clone());
    let watch = r.watch.clone();
    let marker =
        tokio::spawn(async move { watch.marker_write(b"{}".to_vec(), Some(copy(7))).await });
    in_real_time("the walk of the first notice", || walk.walks() == 1).await;
    println!(
        "B884 TRIGGER PRESENT: a marker write is the session's first refusal and its walk of the files is out"
    );
    let wait = Duration::from_secs(1);
    assert!(wait < TREE_ANSWER_LIMIT);
    assert!(
        tokio::time::timeout(wait, r.watch.settled()).await.is_ok(),
        "settled waited for a walk of the files that another caller started"
    );
    assert!(
        tokio::time::timeout(wait, r.watch.idle()).await.is_ok(),
        "idle waited for a walk of the files that another caller started"
    );
    assert_eq!(walk.walks(), 1);
    assert_eq!(
        r.notices().len(),
        0,
        "the notice of that walk was recorded before the walk answered"
    );
    // The walk answers: the caller of that write returns with the refusal, and its notice is recorded.
    walk.let_go.notify_one();
    let refused = marker.await.unwrap().unwrap_err();
    assert!(refused.refused(), "{refused:?}");
    let n = r.notices();
    assert_eq!(n.len(), 1, "{:?}", r.log.lines());
    assert_eq!(n[0].1["storage"]["seen"], "copy");
    assert_eq!(n[0].1["storage"]["usedMb"], 165);
    r.stop().await;
}
