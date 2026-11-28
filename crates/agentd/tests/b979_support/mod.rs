//! One measurement of a real `HostDisk` whose numbers are read and whose test writes do not answer.
//!
//! How the test writes are held: the runtime has ONE thread for blocking work. The measurement reads its numbers there (the walk of
//! the workspace folder, the free space of the root volume). The test's own piece of blocking work is queued behind that and tells
//! when it runs: from then on the thread is the test's, and the test writes, which the measurement asks for after its numbers,
//! wait for it. The clock is stopped and moved by the test, so nothing waits for the limit in real time.
#![allow(dead_code, clippy::unwrap_used)]

use agentd::direct::disk::{DiskProbe, HostDisk, Reading};
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

/// The limit of one measurement, as `crates/agentd/AGENTS.md` states it ("Session disks").
pub const LIMIT: Duration = Duration::from_secs(10);

/// What the measurement answered at each look of the test. `None`: no answer yet.
#[derive(Debug)]
pub struct Held {
    /// At once, before any blocking work could run.
    pub at_once: Option<Reading>,
    /// When the numbers' work had ended and the thread of the test writes was held; the clock had not moved.
    pub with_the_writes_held: Option<Reading>,
    /// One millisecond before the limit.
    pub before_the_limit: Option<Reading>,
    /// One millisecond after the limit.
    pub at_the_limit: Option<Reading>,
    /// The next measurement, asked while the test writes of the first were still out. `None` when the first had answered early.
    pub next_while_out: Option<Reading>,
}

/// The answer of `f` when it has one now; the clock does not move.
async fn now_or_never<T>(f: &mut (impl Future<Output = T> + Unpin)) -> Option<T> {
    std::future::poll_fn(|cx| match Pin::new(&mut *f).poll(cx) {
        Poll::Ready(v) => Poll::Ready(Some(v)),
        Poll::Pending => Poll::Ready(None),
    })
    .await
}

/// One measurement of `host` with its test writes held, on a runtime of its own (call it from a plain thread, never from inside a
/// runtime). When it returns, the held thread is free again and the runtime has ended: the work that was out has ended or was
/// never started, so a later measurement of `host` reads again.
pub fn measure_with_the_test_writes_held(host: &HostDisk) -> Held {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let held = rt.block_on(async {
        let mut m = host.measure();
        let at_once = now_or_never(&mut m).await;
        // Queued behind the work the measurement has asked for so far. It runs when that work has ended, and holds the thread.
        let (runs, running) = std::sync::mpsc::channel::<()>();
        tokio::task::spawn_blocking(move || {
            let _ = runs.send(());
            let _ = wait.recv();
        });
        running.recv().unwrap();
        let mut held = Held {
            at_once,
            with_the_writes_held: None,
            before_the_limit: None,
            at_the_limit: None,
            next_while_out: None,
        };
        if held.at_once.is_some() {
            return held;
        }
        held.with_the_writes_held = now_or_never(&mut m).await;
        if held.with_the_writes_held.is_some() {
            return held;
        }
        tokio::time::advance(LIMIT - Duration::from_millis(1)).await;
        held.before_the_limit = now_or_never(&mut m).await;
        if held.before_the_limit.is_some() {
            return held;
        }
        tokio::time::advance(Duration::from_millis(2)).await;
        held.at_the_limit = now_or_never(&mut m).await;
        if held.at_the_limit.is_some() {
            let mut next = host.measure();
            held.next_while_out = now_or_never(&mut next).await;
        }
        held
    });
    let _ = release.send(());
    drop(rt);
    held
}
