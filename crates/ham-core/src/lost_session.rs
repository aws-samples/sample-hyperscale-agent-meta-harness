//! A session that stopped answering, and the session the service started in its place.
//!
//! The worker of a session sends the service a heartbeat every minute. When none arrives for [`SESSION_LOST_AFTER_S`] seconds the session
//! is lost: the service starts a new session from the last saved copy of the work and the run goes on, and after [`MAX_LOST_SLICES`] lost
//! sessions in a row it ends the run and delivers that copy. This module holds what the client says about it, read from the run's events
//! alone:
//!
//! * **The lost session.** When the service decides that a session is lost it writes one `run.waiting` event (harness authority) whose
//!   payload is a [`SessionLost`] (`kind` [`KIND`]). `run-session` builds the payload with this type and the client reads it with
//!   [`SessionLost::parse`]; a client older than the payload accepts the event and shows nothing for it. [`SessionLost::sentence`] is the
//!   notice of the run screen and of `ham watch`, [`SessionLost::card_row`] the row of the card.
//! * **The silent session.** A worker says at its start how often it writes an event while it lives (`beatS` of `turn.started`): for a
//!   heartbeat interval without any event it writes a beat ([`is_beat`]). The age of the run's newest event therefore tells a session that
//!   stopped answering from one that runs a long command, and after two missed beats ([`notice_after_s`]) the view holds a [`Silence`]
//!   that says since when nothing was heard and what the service does next. A session of a worker that writes no beat is never called
//!   silent, and neither is a session that waits for the user's answer to a question: it writes no beat, so that the question stays
//!   among the newest events of the agent, where the run list looks for it.
//!
//! Nothing here is an outcome of the run: a lost session changes no status and no label.

use crate::sanitize::sanitize_line;
use crate::saved_copy::utc_minute;
use harness_protocol::common::is_utc_timestamp;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use harness_protocol::direct::{MAX_LOST_SLICES, SESSION_LOST_AFTER_S};

/// The `kind` of the payload of the `run.waiting` event that announces a lost session.
pub const KIND: &str = harness_protocol::direct_input::SESSION_LOST_KIND;
/// `cause`: the service received no heartbeat for [`SESSION_LOST_AFTER_S`] seconds.
pub const CAUSE_NO_HEARTBEAT: &str = "no-heartbeat";
/// `cause`: the session ended without a result for another reason (its start failed, its state failed).
pub const CAUSE_NO_RESULT: &str = "no-result";
/// `then`: the service starts a new session.
pub const THEN_RESTART: &str = "restart";
/// `then`: the run ends; the service delivers the last saved copy when there is one.
pub const THEN_END: &str = "end";
/// `delivers`: the run ends and the service delivers the last saved copy of the work.
pub const DELIVERS_COPY: &str = "copy";
/// `delivers`: the run ends and no copy of its work was saved, so nothing is delivered.
pub const DELIVERS_NOTHING: &str = "nothing";
/// `from`: the new session starts from a saved copy of the work.
pub const FROM_COPY: &str = "copy";
/// `from`: the new session starts from the files the run started with, because no copy had been saved.
pub const FROM_START: &str = "start";
/// The label of the card's row for a lost session.
pub const LOST_LABEL: &str = "Lost      ";
/// The most lost sessions a view keeps (the newest ones).
pub const MAX_KEPT: usize = 16;
/// The seconds after the start of the session that followed a lost one during which the status block of the run screen still tells
/// the lost session, in the past tense ([`SessionLost::sentence_started`]). After them the conversation and the card keep it.
pub const TOLD_AFTER_START_S: i64 = 300;

/// What the service decided when a session was lost: the payload of its `run.waiting` event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLost {
    /// [`KIND`].
    pub kind: String,
    /// The number of the session that was lost (1 for the run's first).
    pub session: u32,
    /// When the service decided it (RFC 3339 UTC).
    pub decided_at: String,
    /// The last heartbeat the service received from the session, when it holds one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<String>,
    /// [`CAUSE_NO_HEARTBEAT`] or [`CAUSE_NO_RESULT`].
    pub cause: String,
    /// The seconds without a heartbeat after which a session is lost.
    pub silence_s: u64,
    /// The lost sessions in a row, this one included.
    pub lost_in_a_row: u32,
    /// The number of lost sessions in a row at which the run ends.
    pub limit: u32,
    /// [`THEN_RESTART`] or [`THEN_END`].
    pub then: String,
    /// The number of the session that starts ([`THEN_RESTART`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_session: Option<u32>,
    /// What the new session starts from: [`FROM_COPY`] or [`FROM_START`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// When the copy the new session starts from was saved, when its record says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<String>,
    /// What the run delivers when it ends here ([`THEN_END`]): [`DELIVERS_COPY`] or [`DELIVERS_NOTHING`]. Absent for a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivers: Option<String>,
}

impl SessionLost {
    /// The payload of a harness `run.waiting` event of kind [`KIND`]; `None` for every other event. Its words are made safe for a
    /// terminal, and a time that is no UTC time is dropped (the event is then read without it; without the time of the decision it is
    /// not read at all).
    pub fn parse(e: &Envelope) -> Option<Self> {
        if e.event_type != EventType::RunWaiting || e.authority != Authority::Harness {
            return None;
        }
        let Payload::Inline { value } = &e.payload else {
            return None;
        };
        if value.get("kind").and_then(Value::as_str) != Some(KIND) {
            return None;
        }
        let mut lost: SessionLost = serde_json::from_value(value.clone()).ok()?;
        if !is_utc_timestamp(&lost.decided_at) {
            return None;
        }
        lost.last_heartbeat_at = lost.last_heartbeat_at.filter(|t| is_utc_timestamp(t));
        lost.saved_at = lost.saved_at.filter(|t| is_utc_timestamp(t));
        lost.cause = sanitize_line(&lost.cause);
        lost.then = sanitize_line(&lost.then);
        lost.from = lost.from.map(|f| sanitize_line(&f));
        lost.delivers = lost.delivers.map(|d| sanitize_line(&d));
        Some(lost)
    }

    /// The notice of the run screen and of `ham watch`: which session was lost and why, which session starts from which copy, and what
    /// that means for the work.
    pub fn sentence(&self) -> String {
        let n = self.session;
        let head = if self.cause == CAUSE_NO_HEARTBEAT {
            match self.last_heartbeat_at.as_deref() {
                Some(t) => format!(
                    "Session {n} was lost: the service received no heartbeat from it for {} seconds (the last one came at {}).",
                    self.silence_s,
                    utc_minute(t)
                ),
                None => format!(
                    "Session {n} was lost: the service received no heartbeat from it for {} seconds.",
                    self.silence_s
                ),
            }
        } else {
            format!("Session {n} was lost: it ended without a result.")
        };
        let then = match (
            self.restart(),
            self.from.as_deref(),
            self.saved_at.as_deref(),
        ) {
            (Some(m), Some(FROM_START), _) => format!(
                "Session {m} starts from the files the run started with: no copy had been saved, so the work of session {n} is lost."
            ),
            (Some(m), _, Some(t)) => format!(
                "Session {m} starts from the copy saved at {}; file changes after that copy are lost.",
                utc_minute(t)
            ),
            (Some(m), _, None) => format!(
                "Session {m} starts from the last saved copy; file changes after that copy are lost."
            ),
            (None, _, _) => {
                let why = if self.lost_in_a_row >= self.limit {
                    format!(
                        "It was lost session {} in a row, so the run ends",
                        self.lost_in_a_row
                    )
                } else {
                    "No new session starts, so the run ends".to_string()
                };
                // The delivery is named only when the service says there is a copy to deliver.
                match self.delivers.as_deref() {
                    Some(DELIVERS_COPY) => {
                        format!("{why} and the service delivers the last saved copy.")
                    }
                    Some(DELIVERS_NOTHING) => format!(
                        "{why}. The run saved no copy of its work, so there is nothing to deliver."
                    ),
                    _ => format!("{why}."),
                }
            }
        };
        format!("{head} {then}")
    }

    /// [`SessionLost::sentence`] once the session it names has started: the same facts in the past tense, with the last heartbeat
    /// the service received and the time it called the session lost, which is [`SessionLost::silence_s`] seconds later.
    /// An event that starts no session reads as
    /// [`SessionLost::sentence`].
    pub fn sentence_started(&self) -> String {
        let n = self.session;
        let at = utc_minute(&self.decided_at);
        let head = if self.cause == CAUSE_NO_HEARTBEAT {
            match self.last_heartbeat_at.as_deref() {
                Some(beat) => format!(
                    "Session {n} was lost: its last heartbeat came at {}, and the service called it lost at {} after {} seconds \
                     without one.",
                    utc_minute(beat),
                    utc_minute_after(beat, &self.decided_at),
                    self.silence_s
                ),
                None => format!(
                    "Session {n} was lost: the service called it lost at {at} after {} seconds without a heartbeat.",
                    self.silence_s
                ),
            }
        } else {
            format!("Session {n} was lost at {at}: it ended without a result.")
        };
        let then = match (
            self.restart(),
            self.from.as_deref(),
            self.saved_at.as_deref(),
        ) {
            (Some(m), Some(FROM_START), _) => format!(
                "Session {m} started from the files the run started with: no copy had been saved, so the work of session {n} was lost."
            ),
            (Some(m), _, Some(t)) => format!(
                "Session {m} started from the copy saved at {}; file changes after that copy were lost.",
                utc_minute(t)
            ),
            (Some(m), _, None) => format!(
                "Session {m} started from the last saved copy; file changes after that copy were lost."
            ),
            (None, _, _) => return self.sentence(),
        };
        format!("{head} {then}")
    }

    /// The row of the card: `Lost      session 1: last heartbeat <time>, called lost <time>; session 2 started from the copy saved at
    /// <time>; file changes after that copy were lost`. The two times are the last heartbeat the service received and its decision,
    /// [`SessionLost::silence_s`] seconds later; a service that holds no heartbeat of the session gives the decision alone.
    pub fn card_row(&self) -> String {
        let n = self.session;
        let then = match (
            self.restart(),
            self.from.as_deref(),
            self.saved_at.as_deref(),
        ) {
            (Some(m), Some(FROM_START), _) => format!(
                "session {m} started from the files the run started with; the work of session {n} was lost"
            ),
            (Some(m), _, Some(t)) => format!(
                "session {m} started from the copy saved at {}; file changes after that copy were lost",
                utc_minute(t)
            ),
            (Some(m), _, None) => format!(
                "session {m} started from the last saved copy; file changes after that copy were lost"
            ),
            (None, _, _) if self.lost_in_a_row >= self.limit => format!(
                "it was lost session {} in a row, so the run ended",
                self.lost_in_a_row
            ),
            (None, _, _) => "no new session started, so the run ended".to_string(),
        };
        match self.last_heartbeat_at.as_deref() {
            Some(beat) => format!(
                "{LOST_LABEL}session {n}: last heartbeat {}, called lost {}; {then}",
                utc_minute(beat),
                utc_minute_after(beat, &self.decided_at)
            ),
            None => format!(
                "{LOST_LABEL}session {n}, called lost {}; {then}",
                utc_minute(&self.decided_at)
            ),
        }
    }

    /// The number of the session the service starts, when it starts one.
    pub fn restart(&self) -> Option<u32> {
        self.next_session.filter(|_| self.then == THEN_RESTART)
    }
}

/// The second of two times in one sentence: `07:20 UTC` when `t` is on the day of `first`, else the whole [`utc_minute`].
fn utc_minute_after(first: &str, t: &str) -> String {
    let whole = utc_minute(t);
    match (first.get(..10), t.get(..10), whole.get(11..)) {
        (Some(a), Some(b), Some(time)) if a == b => time.to_string(),
        _ => whole,
    }
}

/// A beat: the event a worker writes for a heartbeat interval in which it wrote no other (`turn.progress` with the member `alive` and no
/// `kind`). It says that the session lives and nothing about the work.
pub fn is_beat(e: &Envelope) -> bool {
    e.event_type == EventType::TurnProgress
        && matches!(&e.payload, Payload::Inline { value } if value.get("alive").is_some() && value.get("kind").is_none())
}

/// The seconds without any event after which a session that writes a beat every `beat_s` seconds is called silent: two beats are
/// missing, and half a minute more for the worker's posts and the clocks.
pub fn notice_after_s(beat_s: u64) -> u64 {
    beat_s.saturating_mul(2).saturating_add(30)
}

/// A session that says it writes beats and from which nothing has been read for [`notice_after_s`] seconds or more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Silence {
    /// The time of the run's newest event (Unix seconds).
    pub since_unix: i64,
    /// The same time as the event wrote it (RFC 3339 UTC).
    pub since: String,
    /// The seconds from that event to the client's clock.
    pub secs: i64,
    /// When the newest copy of the run's work was saved, when the run saved one: what a new session would start from.
    pub saved_at: Option<String>,
    /// The number this session would have among the lost sessions in a row, when losing it would end the run. The client counts them
    /// from the run's events (`RunView::lost_in_a_row`); it cannot see how much of the run's time is left, so the notice of a
    /// restart says "when the run has time left".
    pub would_end: Option<u32>,
}

/// `45 s`, `2 min`, `3 h`: a duration in its largest whole unit.
fn age(secs: i64) -> String {
    match secs.max(0) {
        s @ 0..=59 => format!("{s} s"),
        s @ 60..=3_599 => format!("{} min", s / 60),
        s => format!("{} h", s / 3_600),
    }
}

impl Silence {
    /// The notice of the run screen and of `ham watch`: since when no event of the run was read, and what the service does when
    /// that lasts. The moment is the newest event the client read ([`Silence::since`]); the service counts from the last heartbeat
    /// it received, which [`SessionLost::sentence`] names, and the two can be about a minute apart.
    pub fn sentence(&self) -> String {
        let head = format!(
            "No event from the run's session since {} ({}). When the service receives no heartbeat from a session for \
             {SESSION_LOST_AFTER_S} seconds,",
            utc_minute(&self.since),
            age(self.secs)
        );
        match (self.would_end, self.saved_at.as_deref()) {
            (Some(k), Some(_)) => format!(
                "{head} the session is lost; it would be lost session {k} in a row, so the run would end and the service would \
                 deliver the last saved copy."
            ),
            (Some(k), None) => format!(
                "{head} the session is lost; it would be lost session {k} in a row, so the run would end, and it has saved no \
                 copy that the service could deliver."
            ),
            (None, Some(t)) => format!(
                "{head} it starts a new session from the last saved copy (saved {}) when the run has time left; file changes \
                 after that copy are lost.",
                utc_minute(t)
            ),
            (None, None) => format!(
                "{head} it starts a new session from the files the run started with when the run has time left, because the run \
                 has saved no copy yet."
            ),
        }
    }
}
