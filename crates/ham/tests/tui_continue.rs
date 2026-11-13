//! A process that job control stopped and continued (SIGSTOP, then SIGCONT) cannot know what was drawn on its terminal while it was
//! stopped, so the continue is read as the terminal reporting that the window is back: the event loop gets a `FocusGained` and repaints.
//! The forwarder holds only a weak sender, so it never keeps the key channel open: the loop still ends when the key reader ends.
//!
//! The tests send SIGCONT to their own process. A process that is not stopped ignores it, so nothing else happens.
#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(clippy::unwrap_used)]

use crossterm::event::Event;
use ham::tui::session::forward_continue;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

fn continue_self() {
    let status = std::process::Command::new("kill")
        .args(["-CONT", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -CONT failed: {status:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_continue_signal_arrives_as_the_terminal_reporting_focus_again() {
    let (tx, mut rx) = unbounded_channel::<(Event, Instant)>();
    let task = forward_continue(tx.downgrade()).expect("this platform has SIGCONT");
    for round in 1..=2 {
        continue_self();
        let (event, _) = timeout(WAIT, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no event after continue number {round}"))
            .expect("the channel is open");
        assert!(
            matches!(event, Event::FocusGained),
            "continue number {round} gave {event:?}"
        );
    }
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_forwarder_keeps_the_key_channel_open_for_no_one() {
    let (tx, mut rx) = unbounded_channel::<(Event, Instant)>();
    let task = forward_continue(tx.downgrade()).expect("this platform has SIGCONT");
    drop(tx);
    // The reader's sender was the last strong one: the receiver sees the end at once, as the event loop does when the key reader ends.
    let end = timeout(WAIT, rx.recv())
        .await
        .expect("the forwarder holds a strong sender and keeps the channel open");
    assert!(end.is_none());
    // And the forwarder ends with the next signal, because it finds nobody to send to.
    continue_self();
    timeout(WAIT, task)
        .await
        .expect("the forwarder did not end")
        .unwrap();
}
