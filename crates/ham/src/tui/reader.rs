//! The terminal key reader and `suspend_for`: the thread that waits for terminal events, and the one primitive that pauses it, gives
//! the real tty to a child (an editor, a git prompt) and re-enters the TUI through the session guard. Nothing else may read the terminal
//! or hand the tty to a child.
//!
//! * **The reader.** [`start`] runs a thread that polls crossterm for 40 ms at a time and forwards each event, stamped with the instant it
//!   was read, to the event loop. A poll (not a blocking read) is what lets the thread stop between two events when a child needs the
//!   terminal. The thread ends when the receiver is gone or the terminal cannot be read (closed stdin).
//! * **The hand-over.** [`suspend_for`] asks the reader to park, waits until it has (at most 2 s, and not at all when no reader runs,
//!   as in the offline tests), takes the terminal out of the TUI modes (`session::suspend`: the alternate screen, mouse, paste and focus
//!   reports and raw mode go off, the cursor comes back), runs the child with the terminal as its standard input and output, turns the
//!   modes on again (`session::resume`) and lets the reader go on. The loop clears its screen afterwards, because the alternate
//!   screen it comes back to is blank.

use crossterm::event::Event;
use std::io;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// How long the reader waits for an event before it looks at the pause flag.
const POLL: Duration = Duration::from_millis(40);
/// The longest `suspend_for` waits for the reader to park.
const PARK_WAIT: Duration = Duration::from_secs(2);

/// A child needs the terminal: the reader parks until this is false again.
static PAUSE: AtomicBool = AtomicBool::new(false);
/// The reader is parked (it reads nothing now).
static PARKED: AtomicBool = AtomicBool::new(false);
/// A reader thread is alive.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Marks the reader as gone when its thread ends, however it ends.
struct Alive;

impl Drop for Alive {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
        PARKED.store(false, Ordering::SeqCst);
    }
}

/// Starts the reader thread: each terminal event goes to `tx` with the instant it was read.
pub fn start(tx: UnboundedSender<(Event, Instant)>) {
    RUNNING.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        let _alive = Alive;
        loop {
            if PAUSE.load(Ordering::SeqCst) {
                PARKED.store(true, Ordering::SeqCst);
                while PAUSE.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                PARKED.store(false, Ordering::SeqCst);
                continue;
            }
            match crossterm::event::poll(POLL) {
                Ok(true) => match crossterm::event::read() {
                    Ok(ev) => {
                        if tx.send((ev, Instant::now())).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
}

/// Runs `cmd` with the terminal handed over to it, and takes the terminal back. The reader is parked meanwhile and the TUI modes are off;
/// both come back whatever the child does. With no session (an offline test) only the command runs.
pub fn suspend_for(cmd: &mut Command) -> io::Result<ExitStatus> {
    PAUSE.store(true, Ordering::SeqCst);
    let began = Instant::now();
    while RUNNING.load(Ordering::SeqCst)
        && !PARKED.load(Ordering::SeqCst)
        && began.elapsed() < PARK_WAIT
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    let modes = match super::session::suspend() {
        Ok(m) => m,
        Err(e) => {
            PAUSE.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };
    let status = cmd.status();
    let resumed = match modes {
        Some(m) => super::session::resume(m),
        None => Ok(()),
    };
    PAUSE.store(false, Ordering::SeqCst);
    match (status, resumed) {
        (Ok(status), Ok(())) => Ok(status),
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
}
