//! The bell and the terminal notification for "Cline needs you". [`Alerts`] watches which runs wait for the user and says what
//! to sound when one starts to wait: the bell once per batch of runs, and, when the option is on and the window is not focused, one OSC 9
//! notification per run. A run that keeps waiting does not ring again; one that is answered and waits again does.
//!
//! The bytes come only from fixed words and a run id that passes the ULID check ([`notification_bytes`]): the module takes no text from a
//! transcript, an event or the engine, so nothing written there can reach the terminal through it.

use harness_protocol::common::is_ulid;
use std::collections::HashSet;
use std::io::{self, Write};

/// The bell.
pub fn bell_bytes() -> &'static [u8] {
    b"\x07"
}

/// The words of a notification: fixed text and, for a valid run id, its first eight characters.
fn notification_text(run_id: &str) -> String {
    if is_ulid(run_id) {
        format!("ham: Cline needs you (run {})", &run_id[..8])
    } else {
        "ham: Cline needs you".to_string()
    }
}

/// An OSC 9 notification: ESC ] 9 ; text BEL. The text is printable ASCII.
pub fn notification_bytes(run_id: &str) -> Vec<u8> {
    let mut out = b"\x1b]9;".to_vec();
    out.extend_from_slice(notification_text(run_id).as_bytes());
    out.push(0x07);
    out
}

/// The same notification wrapped for tmux, which drops an OSC it does not know unless it is passed through: a DCS `tmux;` with every ESC
/// of the payload doubled, ended by ST. tmux also needs `allow-passthrough on`.
pub fn notification_bytes_tmux(run_id: &str) -> Vec<u8> {
    let mut out = b"\x1bPtmux;".to_vec();
    for b in notification_bytes(run_id) {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// What to sound for one run that started to wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub run_id: String,
    /// Ring the bell (the first alert of a batch only, and only while the bell is on).
    pub bell: bool,
    /// Send a terminal notification (the option is on and the window is not focused).
    pub notify: bool,
    /// Wrap the notification for tmux.
    pub tmux: bool,
}

impl Alert {
    /// The notification's words: fixed text and, for a valid run id, its first eight characters. `None` when none is to be sent.
    pub fn notification_text(&self) -> Option<String> {
        self.notify.then(|| notification_text(&self.run_id))
    }

    /// Writes what this alert sounds: the bell, then the notification. The bytes are built here from fixed words and the run id, never
    /// from text handed in.
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        if self.bell {
            w.write_all(bell_bytes())?;
        }
        if self.notify {
            let bytes = if self.tmux {
                notification_bytes_tmux(&self.run_id)
            } else {
                notification_bytes(&self.run_id)
            };
            w.write_all(&bytes)?;
        }
        Ok(())
    }
}

/// The runs that wait for the user, and the options.
#[derive(Clone, Debug)]
pub struct Alerts {
    bell: bool,
    notify: bool,
    tmux: bool,
    focused: bool,
    /// The runs that waited at the last observation.
    waiting: HashSet<String>,
}

impl Default for Alerts {
    fn default() -> Self {
        Self::new()
    }
}

impl Alerts {
    /// The bell on, the notification off, the window focused.
    pub fn new() -> Self {
        Self {
            bell: true,
            notify: false,
            tmux: false,
            focused: true,
            waiting: HashSet::new(),
        }
    }

    pub fn set_bell(&mut self, on: bool) {
        self.bell = on;
    }

    /// The option that sends a terminal notification while the window is not focused.
    pub fn set_notify(&mut self, on: bool) {
        self.notify = on;
    }

    /// The terminal is tmux (`$TMUX` is set): the notification is wrapped to pass through (it also needs `allow-passthrough on`).
    pub fn set_tmux(&mut self, on: bool) {
        self.tmux = on;
    }

    /// The terminal's focus events (`FocusGained`, `FocusLost`). A terminal that never reports focus leaves this at its start value,
    /// focused, and no notification is sent.
    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Marks these runs as already waiting, so the first read of the list makes no sound.
    pub fn seed(&mut self, waiting: &[&str]) {
        self.waiting = waiting.iter().map(|s| (*s).to_string()).collect();
    }

    /// The waits now. Returns an alert for each that did not wait at the last call, in the order given. An id is opaque to this type: the Runs
    /// list of the TUI passes `<run id>|<request id>`, so a run that asks a new question is a new wait, and the alert's `run_id` is the
    /// id as it was given.
    pub fn observe(&mut self, waiting: &[&str]) -> Vec<Alert> {
        let mut out: Vec<Alert> = Vec::new();
        for &id in waiting {
            if self.waiting.contains(id) || out.iter().any(|a| a.run_id == id) {
                continue;
            }
            out.push(Alert {
                run_id: id.to_string(),
                bell: self.bell && out.is_empty(),
                notify: self.notify && !self.focused,
                tmux: self.tmux,
            });
        }
        self.waiting = waiting.iter().map(|s| (*s).to_string()).collect();
        out
    }
}
