//! The burst guard: a pure state machine over injected instants that tells a paste that arrives as keys (a terminal
//! without bracketed paste) from typing, so an Enter inside a burst is a new line and not a send, and no key soon after a burst can send
//! or confirm. The three timings are starting values that the paste scenarios measure and pin:
//!
//! - keys less than [`KEY_GAP`] apart, with no paste event, are a burst;
//! - no key within [`AFTER_BURST`] after a paste event or a burst can send or confirm;
//! - a confirming key less than [`AFTER_PREVIEW`] after the preview frame was drawn is ignored.
//!
//! The first key of a paste cannot be told from a typed key; every later one can, and the Enter that would send is one of the later ones.

use std::time::{Duration, Instant};

/// Keys closer together than this are a burst. A held key repeats about every 30 ms, so a held Enter is a burst too, and a sent message
/// is never repeated by key repeat.
pub const KEY_GAP: Duration = Duration::from_millis(30);
/// The time after a paste event or a burst in which no key can send or confirm.
pub const AFTER_BURST: Duration = Duration::from_millis(150);
/// The time after a preview frame was drawn in which a confirming key is ignored.
pub const AFTER_PREVIEW: Duration = Duration::from_millis(300);

/// What the guard remembers between keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Burst {
    last_key: Option<Instant>,
    quiet_until: Option<Instant>,
    preview_until: Option<Instant>,
}

impl Burst {
    pub fn new() -> Self {
        Self::default()
    }

    /// A key press at `at`. True when it is calm: not part of a burst and not inside the quiet time that follows a burst or a paste, so
    /// it may send or confirm.
    pub fn key(&mut self, at: Instant) -> bool {
        let burst = self
            .last_key
            .is_some_and(|last| at.saturating_duration_since(last) < KEY_GAP);
        if burst {
            self.quiet_until = Some(at + AFTER_BURST);
        }
        self.last_key = Some(at);
        !burst && self.quiet_until.is_none_or(|q| at >= q)
    }

    /// A paste event at `at` (bracketed paste): the keys right after it cannot send or confirm.
    pub fn paste(&mut self, at: Instant) {
        self.quiet_until = Some(at + AFTER_BURST);
    }

    /// A preview frame (the line that names what a second key will do) was drawn at `at`.
    pub fn preview_drawn(&mut self, at: Instant) {
        self.preview_until = Some(at + AFTER_PREVIEW);
    }

    /// A key press at `at` that confirms a preview: calm, and not too soon after the preview was drawn.
    pub fn confirm(&mut self, at: Instant) -> bool {
        let calm = self.key(at);
        calm && self.preview_until.is_none_or(|p| at >= p)
    }
}
