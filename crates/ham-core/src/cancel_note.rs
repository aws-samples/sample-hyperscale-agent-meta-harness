//! The user's cancel that the service recorded.
//!
//! `ham cancel <runId>` keeps the work. Since protocol 1.63.0 the service records such a cancel on the run and answers what it recorded
//! (`RunControlResponse.cancel`); it writes the same facts as one `run.waiting` event of the harness whose payload kind is
//! [`KIND`]. The record stands whatever the run's session does: the run starts no further session and delivers what exists. This module
//! holds what the client says about it:
//!
//! * [`CancelNote::of`] reads the answer of the cancel route and [`CancelNote::parse`] the event; a client older than the event reads
//!   `run.waiting` only for a lost session and shows nothing for this one.
//! * [`CancelNote::answer_lines`] are the lines `ham cancel` prints after the status line. [`CancelNote::notice_line`] is the one
//!   line of `ham watch`, of the conversation's notice on the run screen and of the notice row of the terminal app:
//!   [`CancelNote::watch_line`] and then the command that downloads the newest saved copy. The notice row is one row and a
//!   terminal cuts it; the conversation's notice is wrapped and says the line whole.
//!
//! Three forms, by what the service knew when the cancel arrived: a session that is asked to stop (with the latest time at which the
//! service calls it lost when it stays silent: no promise of when the run ends), a run that is already delivering, and a run that has
//! no session that answers. Every form names the command that downloads the newest saved copy of the work: a cancel removes none
//! (root invariant 9).
//!
//! Nothing here is an outcome of the run: a recorded cancel changes no status and no label.

use crate::sanitize::sanitize_line;
use crate::saved_copy::utc_minute;
use harness_protocol::api::v1::{CancelRecorded, RunSummary};
use harness_protocol::common::is_utc_timestamp;
use harness_protocol::direct::SESSION_LOST_AFTER_S;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::goal::GoalStatus;
use serde_json::Value;

/// The `kind` of the payload of the `run.waiting` event that says the cancel is recorded.
pub const KIND: &str = harness_protocol::direct_input::CANCEL_RECORDED_KIND;
/// The words beside the status of a run whose cancel is recorded and that has not ended yet (the run list, the run screen).
pub const RECORDED_WORDS: &str = "cancel recorded";
/// With: which copy the service delivers after a cancel that no session answers, in the one wording of `ham cancel`, of
/// `ham watch`, of the run screen and of the guides. The service reads the gone session's storage back and chooses. A run whose
/// machine keeps no storage, or that switched the read off, has no files read back: the words after "otherwise" are then the whole
/// rule. When the service has decided, [`crate::lost_session::SessionLost::sentence`] names the copy it chose.
pub const DELIVERS: &str = "delivers the files it read back from the session's storage when its checks leave no doubt, and otherwise the \
                            last saved copy, if the run saved one";

/// What the service recorded for a cancel that keeps the work, with every time checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelNote {
    /// When the cancel was recorded (RFC 3339 UTC): the time of the run's first cancel.
    pub requested_at: String,
    /// The number of the run's session when the cancel arrived.
    pub session: Option<u32>,
    /// The run has a session that the service has not ended; it is asked to stop.
    pub session_live: bool,
    /// The run is already delivering what exists.
    pub delivering: bool,
    /// The last heartbeat the service received from the session.
    pub last_heartbeat_at: Option<String>,
    /// The latest time at which the service calls a session that stays silent lost.
    pub lost_after: Option<String>,
}

/// What a row of the run list says after the phase of a run that is `active`, from the service's own members: `cancel
/// recorded` when the user's cancel is recorded, else `session silent since 07:05 UTC` when the service finds the session silent (the
/// time of its last heartbeat). `None` for every other row, for a run that has ended, and for a time that is no UTC time. The client's
/// clock is not asked: the service decides what is silent.
pub fn row_trailer(r: &RunSummary) -> Option<String> {
    if r.status != GoalStatus::Active {
        return None;
    }
    let utc = |t: &Option<String>| t.clone().filter(|t| is_utc_timestamp(t));
    if utc(&r.cancel_requested_at).is_some() {
        return Some(RECORDED_WORDS.to_string());
    }
    let since = utc(&r.session_silent_since)?;
    Some(format!("session silent since {} UTC", since.get(11..16)?))
}

/// `2026-11-16T07:05:22Z` as `2026-11-16 07:05:22 UTC`.
fn utc_second(at: &str) -> String {
    let time = at.replace('T', " ");
    time.strip_suffix('Z')
        .map_or(time.clone(), |t| format!("{t} UTC"))
}

/// `t` beside `first` in one sentence: the time alone (`07:15 UTC`) on the day of `first`, else the day too.
fn utc_minute_beside(first: &str, t: &str) -> String {
    let whole = utc_minute(t);
    match (first.get(..10), t.get(..10), whole.get(11..)) {
        (Some(a), Some(b), Some(time)) if a == b => time.to_string(),
        _ => whole,
    }
}

impl CancelNote {
    /// The record as the service sent it. `None` when its time is no UTC time; another time that is none is dropped.
    pub fn of(c: &CancelRecorded) -> Option<Self> {
        if !is_utc_timestamp(&c.requested_at) {
            return None;
        }
        let utc = |t: &Option<String>| t.clone().filter(|t| is_utc_timestamp(t));
        Some(CancelNote {
            requested_at: c.requested_at.clone(),
            session: c.session,
            session_live: c.session_live,
            delivering: c.delivering,
            last_heartbeat_at: utc(&c.last_heartbeat_at),
            lost_after: utc(&c.lost_after),
        })
    }

    /// The payload of a harness `run.waiting` event of kind [`KIND`]; `None` for every other event.
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
        let recorded: CancelRecorded = serde_json::from_value(value.clone()).ok()?;
        Self::of(&recorded)
    }

    /// `session 1`, or `its session` when the service named none.
    fn session_words(&self) -> String {
        match self.session {
            Some(n) => format!("session {n}"),
            None => "its session".to_string(),
        }
    }

    /// When a session that stays silent is called lost: `at the latest at about 07:15 UTC`, or the rule when the service sent no time.
    fn lost_words(&self) -> String {
        match self.lost_after.as_deref() {
            Some(t) => format!(
                "at the latest at about {}",
                utc_minute_beside(&self.requested_at, t)
            ),
            None => format!(
                "when it has received no heartbeat from it for {SESSION_LOST_AFTER_S} seconds"
            ),
        }
    }

    /// The lines `ham cancel` prints after the status line. `run_id` is the run as the user names it.
    pub fn answer_lines(&self, run_id: &str) -> Vec<String> {
        let run = sanitize_line(run_id);
        let at = utc_second(&self.requested_at);
        let copy = Self::copy_line(run_id);
        if self.delivering {
            return vec![
                format!(
                    "The cancel is recorded ({at}). The run is already delivering what exists."
                ),
                copy,
            ];
        }
        if self.session_live {
            return vec![
                format!(
                    "The cancel is recorded ({at}). The run's session is asked to stop and the work so far is kept: the run delivers what \
                     exists."
                ),
                format!(
                    "If {} does not answer, the service calls it lost {}, starts no further session and {DELIVERS}.",
                    self.session_words(),
                    self.lost_words()
                ),
                copy,
            ];
        }
        vec![
            format!(
                "The cancel is recorded ({at}). The run has no session that answers: the service starts no further session and \
                 {DELIVERS}."
            ),
            format!("{copy} ham cancel {run} --discard ends the run now and delivers nothing."),
        ]
    }

    /// The sentence that names the command of the newest saved copy of the work: a cancel removes no copy (root invariant 9). The
    /// record of the cancel does not say whether the run has saved a copy (a run saves one only after a file changed), so the
    /// sentence says "if the run saved one": the command of a run without a copy answers that none is saved yet.
    pub fn copy_line(run_id: &str) -> String {
        format!(
            "ham pull {} --snapshot downloads the newest saved copy of the work at any time, if the run saved one.",
            sanitize_line(run_id)
        )
    }

    /// The one line of `ham watch`, the conversation's notice on the run screen and the notice row of the terminal app after a
    /// cancel made with its keys: [`CancelNote::watch_line`] and then [`CancelNote::copy_line`].
    pub fn notice_line(&self, run_id: &str) -> String {
        format!("{} {}", self.watch_line(), Self::copy_line(run_id))
    }

    /// What the service recorded and what follows, in one line: the first part of [`CancelNote::notice_line`].
    pub fn watch_line(&self) -> String {
        let at = utc_minute(&self.requested_at);
        if self.delivering {
            return format!("Cancel recorded at {at}: the run is already delivering what exists.");
        }
        if self.session_live {
            return format!(
                "Cancel recorded at {at}: the run starts no further session. If {} stays silent, the service calls it lost {} and \
                 {DELIVERS}.",
                self.session_words(),
                self.lost_words()
            );
        }
        format!(
            "Cancel recorded at {at}: the run has no session that answers and starts no further one. The service {DELIVERS}."
        )
    }
}
