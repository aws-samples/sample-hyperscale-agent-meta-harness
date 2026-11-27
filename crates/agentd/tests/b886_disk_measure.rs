//! The time limit of the disk rule's measurement, and which folders its test write is made in.
//!
//! `HostDisk::measure` reads the free space of the root volume, walks the workspace folder and makes a test write, on a thread of the
//! blocking pool. Each of the three waits for a file system, and the prompt boundary and the check after an engine error wait for the
//! measurement. The measurement therefore has a time limit of its own: when it has not answered by then, the reading says that nothing
//! was measured (which is never read as full), the run goes on, and while that measurement is still out no second one starts. The
//! machine's report for the log has the same limit.
//!
//! How the tests get a measurement that does not answer: the runtime has ONE thread for blocking work and the test holds it, so the
//! work of the measurement never starts. The clock is stopped and moved by the test, so no test waits for the limit in real time.
//!
//! The test write is made only in a folder that is the worker's own: a real folder (a link in its place is not followed) of the
//! worker's user that neither group nor others may write. Any other folder gets no test write and says nothing about the disk. The
//! file has a name that cannot be made ahead. `tests/b886_test_write.rs` holds the tests of the write itself.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;

use agentd::direct::disk::{self, DiskProbe, HostDisk, Limits, Reading};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

/// The limit of one measurement, as `crates/agentd/AGENTS.md` states it ("Session disks").
const LIMIT: Duration = Duration::from_secs(10);

/// A folder of the test's own that neither group nor others may write, whatever the umask of the test run.
fn own(label: &str) -> PathBuf {
    let d = common::scratch(label);
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    d
}

/// A runtime with a stopped clock and one thread for blocking work, and that thread held until the sender is used or dropped.
fn held_runtime() -> (tokio::runtime::Runtime, std::sync::mpsc::Sender<()>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    rt.spawn_blocking(move || {
        let _ = wait.recv();
    });
    (rt, release)
}

/// The answer of `f` when it has one now; the clock does not move.
async fn now_or_never<T>(f: &mut (impl Future<Output = T> + Unpin)) -> Option<T> {
    std::future::poll_fn(|cx| match Pin::new(&mut *f).poll(cx) {
        Poll::Ready(v) => Poll::Ready(Some(v)),
        Poll::Pending => Poll::Ready(None),
    })
    .await
}

fn names_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// ---- the time limit ---------------------------------------------------------------------------------------------------------------

#[test]
fn b886_a_measure_that_does_not_answer_ends_at_its_limit_with_nothing_measured() {
    let (rt, release) = held_runtime();
    let (root, ws) = (own("b886-root"), own("b886-ws"));
    rt.block_on(async {
        let host = HostDisk::new(&root, &ws);
        let mut m = host.measure();
        assert!(
            now_or_never(&mut m).await.is_none(),
            "the measurement answered though its thread is held: the test proves nothing"
        );
        tokio::time::advance(LIMIT - Duration::from_millis(1)).await;
        assert!(
            now_or_never(&mut m).await.is_none(),
            "the measurement ended before its limit"
        );
        tokio::time::advance(Duration::from_millis(2)).await;
        assert_eq!(
            now_or_never(&mut m).await,
            Some(Reading::default()),
            "a measurement that has not answered within {LIMIT:?} says that nothing was measured"
        );
    });
    // What was not measured is never read as full: the run goes on as after a measurement that failed.
    assert!(Limits::default().full(&Reading::default()).is_none());
    assert!(
        Limits::default()
            .full_with(&Reading::default(), None)
            .is_none()
    );
    drop(release);
}

#[test]
fn b886_while_a_measure_is_out_the_next_one_answers_at_once_and_a_later_one_reads_again() {
    let (rt, release) = held_runtime();
    let (root, ws) = (own("b886-out-root"), own("b886-out-ws"));
    let host = HostDisk::new(&root, &ws);
    rt.block_on(async {
        let mut first = host.measure();
        assert!(now_or_never(&mut first).await.is_none());
        tokio::time::advance(LIMIT + Duration::from_millis(1)).await;
        assert_eq!(now_or_never(&mut first).await, Some(Reading::default()));
        // The first measurement's work still waits for its thread. A second measurement does not wait behind it.
        let mut second = host.measure();
        assert_eq!(
            now_or_never(&mut second).await,
            Some(Reading::default()),
            "a measurement that found one still out did not answer at once"
        );
    });
    // The thread comes free and the runtime ends: the work that was out has ended or was never started.
    release.send(()).unwrap();
    drop(rt);
    // A later measurement reads again.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let r = rt.block_on(host.measure());
    assert_eq!(r.writable, Some(true), "{r:?}");
    assert!(r.root_free_mb.is_some(), "{r:?}");
    assert_eq!(r.workspace_used_mb, Some(0), "{r:?}");
}

#[test]
fn b886_the_machines_report_that_does_not_answer_ends_at_the_same_limit_and_says_so() {
    let (rt, release) = held_runtime();
    let (root, ws) = (own("b886-rep-root"), own("b886-rep-ws"));
    rt.block_on(async {
        let host = HostDisk::new(&root, &ws);
        let mut report = host.diagnose();
        assert!(
            now_or_never(&mut report).await.is_none(),
            "the report answered though its thread is held: the test proves nothing"
        );
        tokio::time::advance(LIMIT + Duration::from_millis(1)).await;
        // The part about the folders is given up at the limit; the tail of the kernel log follows with its own short wait.
        let mut text = None;
        for _ in 0..12_000 {
            text = now_or_never(&mut report).await;
            if text.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            tokio::task::yield_now().await;
        }
        // The cap is long on purpose: the loop ends as soon as the text is there, and a crowded host only makes it wait longer.
        let text = text.expect("the report had not ended a minute after its limit");
        assert!(
            text.contains("the folders gave no answer within 10 s"),
            "{text}"
        );
        assert!(text.chars().count() <= 4096);
    });
    drop(release);
}

// ---- which folders get the test write ---------------------------------------------------------------------------------------------

/// A link in the place of a folder: the write does not go through it, and the reading says nothing about the disk for it.
#[tokio::test]
async fn b886_a_link_in_a_folders_place_gets_no_test_write() {
    let real = own("b886-real");
    let link = own("b886-links").join("to-real");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let before = std::fs::metadata(&real).unwrap().modified().unwrap();
    // A file made and removed in the folder would move the folder's time: give the clock of the file system room to show it.
    std::thread::sleep(Duration::from_millis(30));
    assert!(
        disk::test_write(&link).is_err(),
        "the test write went through a link"
    );
    assert_eq!(
        std::fs::metadata(&real).unwrap().modified().unwrap(),
        before,
        "a file was made in the folder the link names"
    );
    assert!(names_in(&real).is_empty());
    // A folder that is not tried says nothing about the disk.
    let r = HostDisk::new(&link, &link).measure().await;
    assert_eq!(r.writable, None, "{r:?}");
    // Beside a folder that takes the write, the link changes nothing.
    let ok = own("b886-ok");
    let r = HostDisk::new(&ok, &ok)
        .with_write_dirs([link.clone()])
        .measure()
        .await;
    assert_eq!(r.writable, Some(true), "{r:?}");
    assert!(names_in(&ok).is_empty());
}

/// A link that stands under the name the test write had before this change (a fixed word and the worker's process number) is not
/// written through: the file is made new under a name that cannot be made ahead.
#[tokio::test]
async fn b886_a_link_under_a_name_made_ahead_is_not_written_through() {
    let dir = own("b886-ahead");
    let kept = own("b886-kept").join("kept.txt");
    std::fs::write(&kept, b"kept").unwrap();
    let ahead = format!(".agentd-write-probe-{}", std::process::id());
    std::os::unix::fs::symlink(&kept, dir.join(&ahead)).unwrap();
    assert!(disk::test_write(&dir).is_ok());
    assert_eq!(
        std::fs::read(&kept).unwrap(),
        b"kept",
        "the file behind the link was written"
    );
    assert!(dir.join(&ahead).is_symlink(), "the link was removed");
    assert_eq!(names_in(&dir), [ahead], "the test write left a file behind");
    let r = HostDisk::new(&dir, &dir).measure().await;
    assert_eq!(r.writable, Some(true), "{r:?}");
}

/// A folder that group or others may write is not one the worker alone writes: it gets no test write.
#[tokio::test]
async fn b886_a_folder_that_others_may_write_gets_no_test_write() {
    for mode in [0o777, 0o775, 0o757] {
        let open = common::scratch("b886-open");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(disk::test_write(&open).is_err(), "mode {mode:o}");
        assert!(names_in(&open).is_empty(), "mode {mode:o}");
        let r = HostDisk::new(&open, &open).measure().await;
        assert_eq!(r.writable, None, "mode {mode:o}: {r:?}");
    }
}

/// A measurement that is given up at its limit leaves one line in the worker's log, so that a maintainer can read that the disk was
/// not measured. The measurement that answers at once while that read is still out adds none, and a measurement that answers adds none.
#[test]
fn b886_a_measurement_given_up_at_its_limit_leaves_one_line_in_the_log() {
    let (rt, release) = held_runtime();
    let (root, ws) = (own("b886-log-root"), own("b886-log-ws"));
    let log = agentd::logging::MemoryLog::new();
    let host = HostDisk::new(&root, &ws).with_log(log.clone());
    rt.block_on(async {
        let mut m = host.measure();
        assert!(
            now_or_never(&mut m).await.is_none(),
            "the measurement answered though its thread is held: the test proves nothing"
        );
        assert!(log.lines().is_empty(), "{:?}", log.lines());
        tokio::time::advance(LIMIT + Duration::from_millis(1)).await;
        assert_eq!(now_or_never(&mut m).await, Some(Reading::default()));
        // While that read is still out, the next measurement answers at once and adds no line.
        assert_eq!(host.measure().await, Reading::default());
    });
    assert_eq!(
        log.lines(),
        vec![
            "direct turn disk not measured: the measurement gave no answer within 10 s".to_string()
        ]
    );
    drop(release);
    // A measurement that answers adds no line.
    let quiet = agentd::logging::MemoryLog::new();
    let host = HostDisk::new(&root, &ws).with_log(quiet.clone());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let r = rt.block_on(host.measure());
    assert_eq!(r.writable, Some(true), "{r:?}");
    assert!(quiet.lines().is_empty(), "{:?}", quiet.lines());
}
