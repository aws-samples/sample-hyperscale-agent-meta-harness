//! A measurement of the session's disks whose test writes do not answer within the limit keeps the numbers it has read.
//!
//! `HostDisk::measure` walks the workspace folder, reads the free space of the root volume and then makes the test writes, and the
//! whole measurement has one limit. Before this item the whole reading was dropped at the limit: when only the test writes
//! had not answered, the numbers already read went with them and the reading said that nothing was measured, so a storage whose
//! numbers said full was not seen as full at that measurement. Now the reading keeps the numbers and says that the test writes
//! were not judged (`writable` is `None`), and the log has one line that says so.
//!
//! `b979_support` is how the tests get test writes that do not answer while the numbers are read. The last test runs a whole
//! session: it pins that the session gives its log to the measurement, so the line of a measurement that was given up reaches
//! the worker's log.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod b979_support;
mod common;
mod direct_messages_world;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::disk::{DiskProbe, HostDisk, Limits, Reading};
use agentd::direct::hooks::{DirectHook, TurnEnd, TurnVote};
use agentd::logging::MemoryLog;
use b979_support::{Held, LIMIT, measure_with_the_test_writes_held};
use direct_messages_world::{World, directive, pid_dir, quick};
use harness_protocol::direct::DirectOutcome;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const NOT_JUDGED: &str = "direct turn disk test writes not judged: the numbers were read and the test writes gave no answer within 10 s";
const NOT_MEASURED: &str =
    "direct turn disk not measured: the measurement gave no answer within 10 s";

/// A folder of the test's own that neither group nor others may write, whatever the umask of the test run.
fn own(label: &str) -> PathBuf {
    let d = common::scratch(label);
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    d
}

/// A workspace folder that holds 3 MiB, and a root folder.
fn three_mib() -> (PathBuf, PathBuf) {
    let (root, ws) = (own("b979-root"), own("b979-ws"));
    std::fs::write(ws.join("work.bin"), vec![7u8; 3 << 20]).unwrap();
    (root, ws)
}

/// TRIGGER PRESENT: the numbers' work ran, the test writes were held, and the measurement waited for them up to its limit.
fn the_test_writes_were_held(held: &Held) {
    assert!(
        held.at_once.is_none(),
        "the measurement answered before any of its work could run: the test proves nothing: {held:?}"
    );
    assert!(
        held.with_the_writes_held.is_none(),
        "the measurement answered though the thread of its test writes is held: its numbers and its test writes are one piece of \
         work, so test writes that do not answer take the numbers with them at the limit: {held:?}"
    );
    assert!(
        held.before_the_limit.is_none(),
        "the measurement ended before its limit of {LIMIT:?}: {held:?}"
    );
}

#[test]
fn b979_test_writes_that_do_not_answer_leave_the_numbers_that_were_read() {
    let (root, ws) = three_mib();
    let log = MemoryLog::new();
    let host = HostDisk::new(&root, &ws).with_log(log.clone());
    let held = measure_with_the_test_writes_held(&host);
    the_test_writes_were_held(&held);
    let r = held.at_the_limit.expect("a measurement ends at its limit");
    // The numbers that were read are in the reading.
    assert_eq!(r.workspace_used_mb, Some(3), "{r:?}");
    assert!(!r.workspace_partial, "{r:?}");
    assert!(r.root_free_mb.is_some(), "{r:?}");
    // The test writes were not judged: the reading says neither that the folders took a write nor that one refused it.
    assert_eq!(r.writable, None, "{r:?}");
    // One line of the log says so, in its own words: the disk was measured.
    assert_eq!(log.lines(), vec![NOT_JUDGED.to_string()]);
    // While those test writes are out, the next measurement answers at once that nothing was measured and adds no line.
    assert_eq!(held.next_while_out, Some(Reading::default()), "{held:?}");
    // A later measurement, with the thread free again, reads everything and adds no line.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let later = rt.block_on(host.measure());
    assert_eq!(later.workspace_used_mb, Some(3), "{later:?}");
    assert_eq!(later.writable, Some(true), "{later:?}");
    assert_eq!(log.lines(), vec![NOT_JUDGED.to_string()]);
}

/// Numbers that say full are read as full, by the rule that stops a run, though the test writes were not judged; and the words
/// are those of a full disk, never those of a disk that refused a write. Numbers with room are not read as full.
#[test]
fn b979_numbers_that_say_full_are_read_as_full_though_the_test_writes_were_not_judged() {
    let (root, ws) = three_mib();
    let host = HostDisk::new(&root, &ws);
    let held = measure_with_the_test_writes_held(&host);
    the_test_writes_were_held(&held);
    let r = held.at_the_limit.expect("a measurement ends at its limit");
    assert_eq!((r.workspace_used_mb, r.writable), (Some(3), None), "{r:?}");
    // A stop mark of 2 MiB: the folder is over it.
    let tight = Limits {
        root_low_mb: 0,
        workspace_limit_mb: 2,
    };
    let full = tight
        .full_with(&r, None)
        .expect("the numbers say full: the rule that stops a run reads them as full");
    assert!(full.workspace_short && !full.root_short, "{full:?}");
    assert!(
        !full.unwritable,
        "no test write was judged, so none refused: {full:?}"
    );
    let detail = full.detail();
    let label = DirectOutcome::StoppedOther.label_for(Some(detail.as_str()));
    assert!(
        label.starts_with(
            "Cline stopped: the harness ended the run (the session disk is full, workspace 3 MiB against a stop mark of 2 MiB, root "
        ),
        "{label}"
    );
    // The same numbers under a mark with room stop nothing.
    let roomy = Limits {
        root_low_mb: 0,
        workspace_limit_mb: 960,
    };
    assert!(roomy.full_with(&r, None).is_none());
    assert!(roomy.full(&r).is_none());
}

// ---- the session gives its log to the measurement ------------------------------------------------------------------------------

/// How many threads the session's runtime has for blocking work in the test below.
const BLOCKING_THREADS: usize = 4;

/// Holds every thread the runtime has for blocking work, from the first turn end until the test lets go, and votes a prompt. The
/// prompt begins with a prompt boundary, where the session measures its disks: that measurement's work waits behind the held
/// threads, as it waits for a file system that does not answer.
struct HoldTheThreads {
    done: AtomicBool,
    held: AtomicBool,
    let_go: Arc<(Mutex<bool>, Condvar)>,
}

impl HoldTheThreads {
    fn release(&self) {
        let (flag, wake) = &*self.let_go;
        *flag.lock().unwrap() = true;
        wake.notify_all();
    }
}

impl DirectHook for HoldTheThreads {
    fn name(&self) -> &'static str {
        "hold-the-threads"
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            if end.n != 1 || self.done.swap(true, Ordering::SeqCst) {
                return TurnVote::Done;
            }
            // Twice as many pieces of work as there are threads: every thread is held, and what is asked for later is queued
            // behind the rest.
            for _ in 0..2 * BLOCKING_THREADS {
                let let_go = self.let_go.clone();
                tokio::task::spawn_blocking(move || {
                    let (flag, wake) = &*let_go;
                    let mut free = flag.lock().unwrap();
                    while !*free {
                        free = wake.wait(free).unwrap();
                    }
                });
            }
            self.held.store(true, Ordering::SeqCst);
            TurnVote::Prompt("Please go on.".to_string())
        })
    }
}

/// The whole session runs; this test waits for the limit in real time (10 s), because the measurement is the session's own.
#[test]
fn b979_a_session_writes_the_line_of_a_measurement_that_was_given_up_to_its_log() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(BLOCKING_THREADS)
        .build()
        .unwrap();
    rt.block_on(async {
        let w = World::start(&directive(r#"{"work_ms": 100}"#), &[], quick(), &pid_dir()).await;
        let hold = Arc::new(HoldTheThreads {
            done: AtomicBool::new(false),
            held: AtomicBool::new(false),
            let_go: Arc::new((Mutex::new(false), Condvar::new())),
        });
        let run = w.run_after(vec![hold.clone() as Arc<dyn DirectHook>]);
        let given_up = |w: &World| {
            w.d.log
                .lines()
                .iter()
                .filter(|l| l.as_str() == NOT_MEASURED || l.as_str() == NOT_JUDGED)
                .cloned()
                .collect::<Vec<String>>()
        };
        // The line comes when the limit of the measurement has passed. The wait is long on purpose: it ends as soon as the line
        // is there, and a crowded host only makes it wait longer.
        // Without a line the wait ends when the voted prompt has reached the engine: the boundary of that prompt is behind then.
        let began = Instant::now();
        let end = began + Duration::from_secs(120);
        let sent = |w: &World| {
            w.d.prompts_received()
                .contains(&"Please go on.".to_string())
        };
        while given_up(&w).is_empty() && !sent(&w) && Instant::now() < end && !run.is_finished() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let waited = began.elapsed();
        let lines = given_up(&w);
        let held = hold.held.load(Ordering::SeqCst);
        // The threads come free whatever was found, so the session can end.
        hold.release();
        let _ = run.await.unwrap();
        let agentd_log = w.d.log.lines();
        assert!(
            held,
            "TRIGGER: the threads were never held, so no measurement of the session was given up: the test proves nothing\n{agentd_log:?}"
        );
        assert_eq!(
            lines,
            vec![NOT_MEASURED.to_string()],
            "the session's disk check left no line of the measurement it gave up after {waited:?}: the session did not give its \
             log to the measurement\n{agentd_log:?}"
        );
        // The session went on after the measurement that was given up: the voted prompt reached the engine.
        assert!(
            w.d.prompts_received()
                .contains(&"Please go on.".to_string()),
            "{:?}\n{agentd_log:?}",
            w.d.prompts_received()
        );
    });
}
