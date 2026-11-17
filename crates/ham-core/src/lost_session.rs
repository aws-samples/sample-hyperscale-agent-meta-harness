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
//! * **The cancelled run.** A session that is lost after the user's keep-the-work cancel was recorded gets no successor: the
//!   service's event carries the time of the cancel ([`SessionLost::cancelled_at`]), and the sentences say that the run ends because the
//!   user had stopped it, whatever the count of lost sessions says.
//!
//! Nothing here is an outcome of the run: a lost session changes no status and no label.

use crate::sanitize::sanitize_line;
use crate::saved_copy::utc_minute;
use harness_protocol::common::is_utc_timestamp;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use harness_protocol::direct::{MAX_LOST_SLICES, RECOVERY_LIMIT_S, SESSION_LOST_AFTER_S};

/// The `kind` of the payload of the `run.waiting` event that announces a lost session.
pub const KIND: &str = harness_protocol::direct_input::SESSION_LOST_KIND;
/// `cause`: the service received no heartbeat for [`SESSION_LOST_AFTER_S`] seconds.
pub const CAUSE_NO_HEARTBEAT: &str = "no-heartbeat";
/// `cause`: the session ended without a result for another reason (its start failed, its state failed).
pub const CAUSE_NO_RESULT: &str = "no-result";
/// `cause`: the session's worker gave notice that the session is being ended from outside. It is a notice, no proof that the
/// machine stopped; the service stops the session itself.
pub const CAUSE_END_REQUESTED: &str = "end-requested";
/// `cause`: the session was planned to start from the files read back from the session before it and never worked on them
/// (its start was refused, the files could not be restored, or it was lost before it began). It is not counted as a lost session.
pub const CAUSE_RESTORE_FAILED: &str = "restore-failed";
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
/// `from`: the new session starts from the files the service read back from the lost session's storage after it ended.
pub const FROM_RECOVERED: &str = "recovered";
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
    /// When the service took the worker's notice that the session is being ended ([`CAUSE_END_REQUESTED`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// What the service decided about the files it read back from the session's storage; absent when no read concerned the session
    /// and in an event of a service that predates the read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<LostRecovery>,
    /// The session whose storage the files were read back from that the run goes on from ([`FROM_RECOVERED`]) or delivers when
    /// it ends here. It is the lost session itself when the service chose the files it read back from that session, and an earlier
    /// session when the lost session had started from that session's files and saved no copy of its own. Absent when the run goes on
    /// from, or delivers, a saved copy, and in an event of a service that predates the member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_back_session: Option<u32>,
    /// When the user's keep-the-work cancel was recorded, for a session that was lost after it: no new session starts and the run
    /// ends ([`THEN_END`]) because of the cancel. Absent for every other lost session and from a service that predates the member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at: Option<String>,
    /// The run of the event, which the sentences name in the command that downloads files read back. It is no member of the payload:
    /// [`SessionLost::parse`] takes it from the event's envelope, and a value built without it is told with the placeholder `<runId>`.
    #[serde(skip)]
    pub run_id: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The service's decision about the files it read back from a lost session's storage, as the event carries it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LostRecovery {
    /// A read of the storage began.
    pub tried: bool,
    /// The run goes on from the files read back.
    pub used: bool,
    /// Why they are not used, as one word, when a word says it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_write_at: Option<String>,
    /// The files of which the saved copy holds more than the storage did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regressed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regressed_path: Option<String>,
    /// The files read back are stored in the stage's bucket, whether the run goes on from them or not: `ham pull <runId> --read-back
    /// <session>` downloads them. Absent when the read stored nothing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub kept: bool,
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
        lost.cancelled_at = lost.cancelled_at.filter(|t| is_utc_timestamp(t));
        lost.cause = sanitize_line(&lost.cause);
        lost.then = sanitize_line(&lost.then);
        lost.from = lost.from.map(|f| sanitize_line(&f));
        lost.delivers = lost.delivers.map(|d| sanitize_line(&d));
        lost.ended_at = lost.ended_at.filter(|t| is_utc_timestamp(t));
        if harness_protocol::common::is_ulid(&e.run_id) {
            lost.run_id = e.run_id.clone();
        }
        if let Some(r) = lost.recovery.as_mut() {
            r.reason = r.reason.take().map(|w| sanitize_line(&w));
            r.regressed_path = r.regressed_path.take().map(|p| sanitize_line(&p));
            r.newest_write_at = r.newest_write_at.take().filter(|t| is_utc_timestamp(t));
        }
        Some(lost)
    }

    /// The head of the notice for the two causes the read added, the same in both tenses; `None` for the causes before it.
    fn head_of_the_read(&self) -> Option<String> {
        let n = self.session;
        if self.cause == CAUSE_END_REQUESTED {
            Some(match self.ended_at.as_deref() {
                Some(t) => format!(
                    "Session {n} was asked to end from outside at {}.",
                    utc_minute(t)
                ),
                None => format!("Session {n} was asked to end from outside."),
            })
        } else if self.cause == CAUSE_RESTORE_FAILED {
            Some(format!(
                "Session {n} could not start from the files read back from session {}'s storage.",
                n.saturating_sub(1)
            ))
        } else {
            None
        }
    }

    /// What the notice says of the files read back, the same in both tenses. The bound of about a second was measured for a
    /// session that was stopped from outside ([`CAUSE_END_REQUESTED`]). For every other cause (a session that went silent, an end
    /// without a result) nothing was measured, so the words name the files and state no bound.
    fn files_read_back(&self) -> &'static str {
        if self.cause == CAUSE_END_REQUESTED {
            "They hold what was written and closed up to about a second before the end; a file that was still being written may lack \
             its last lines."
        } else {
            "They are the files that storage held; a file that was still being written may lack its last lines."
        }
    }

    /// The command that downloads the files the service read back from this session's storage, used or not.
    pub fn read_back_command(&self) -> String {
        let run = if self.run_id.is_empty() {
            "<runId>"
        } else {
            self.run_id.as_str()
        };
        format!("ham pull {run} --read-back {}", self.session)
    }

    /// Files read back from this session's storage are stored and the run does not go on from them. Every notice of such files
    /// names the command that downloads them: a check may refuse to use work, it never hides it.
    fn kept_unused(&self) -> bool {
        self.recovery.as_ref().is_some_and(|r| r.kept && !r.used)
    }

    /// The read found the same tree as the saved copy.
    fn nothing_newer(&self) -> bool {
        self.recovery
            .as_ref()
            .is_some_and(|r| !r.used && r.reason.as_deref() == Some("nothing-newer"))
    }

    /// The notice does not say that file changes after the saved copy are lost: the files read back are stored and can be
    /// downloaded, or the storage held nothing newer than that copy.
    fn nothing_called_lost(&self) -> bool {
        self.kept_unused() || self.nothing_newer()
    }

    /// The earlier session whose storage the files were read back from that the lost session itself had started from, when the
    /// run goes on from those files again.
    fn carried_from(&self) -> Option<u32> {
        self.read_back_session.filter(|k| *k != self.session)
    }

    /// The session whose storage the files were read back from that the run delivers when it ends here: the files the service
    /// read back from this session's storage and chose, or the ones of an earlier session that this session had started from and
    /// left as they were. `None` when the run delivers a saved copy or nothing.
    fn delivered_read_back(&self) -> Option<u32> {
        self.read_back_session.or_else(|| {
            self.recovery
                .as_ref()
                .filter(|r| r.used)
                .map(|_| self.session)
        })
    }

    /// What the notice says of the files the next session `m` starts from (`verb`: `starts` or `started`), when they are files
    /// read back.
    fn start_from_the_files_read_back(&self, m: u32, verb: &str) -> String {
        let n = self.session;
        match self.carried_from() {
            Some(k) => format!(
                "Session {m} {verb} from the files the service read back from session {k}'s storage, which session {n} had started \
                 from; session {n} saved no copy of its own."
            ),
            None => format!(
                "Session {m} {verb} from the files the service read back from session {n}'s storage after it ended. {}",
                self.files_read_back()
            ),
        }
    }

    /// Why the files read back from this session's storage are not used, or why none could be read, the same in both tenses.
    /// `None` when no read concerned the session, when the run goes on from the files read back, and for a decision that carries
    /// neither a reason nor stored files.
    fn reason_sentence(&self) -> Option<String> {
        let r = self.recovery.as_ref().filter(|r| !r.used)?;
        let n = self.session;
        let not_used = format!("The service read session {n}'s storage back and did not use it");
        let not_read = format!("The service could not read session {n}'s storage back");
        let than = if self.from.as_deref() == Some(FROM_START) {
            "the files the run started with"
        } else {
            "that copy"
        };
        Some(match r.reason.as_deref() {
            Some("nothing-newer") => {
                format!("The service read session {n}'s storage back and found nothing newer.")
            }
            Some("storage-older") => {
                format!("{not_used}: the storage did not prove to be newer than {than}.")
            }
            Some("older-file") => match (r.regressed, r.regressed_path.as_deref()) {
                (Some(k), Some(path)) => format!(
                    "{not_used}: {k} file(s) in the saved copy hold more than the storage did, for example `{path}`."
                ),
                (Some(k), None) => {
                    format!("{not_used}: {k} file(s) in the saved copy hold more than the storage did.")
                }
                _ => format!("{not_used}: a file in the saved copy holds more than the storage did."),
            },
            Some("superseded") => format!("{not_used}: a newer copy was saved while it read."),
            Some("unstable") => format!(
                "The service did not use session {n}'s storage: its files were still changing."
            ),
            Some("not-mounted" | "no-tree") => format!("Session {n}'s storage held no work tree."),
            Some("worker-refused") => {
                format!("{not_read}: the run's worker is of a release that cannot do it.")
            }
            Some("timeout" | "budget" | "abandoned") => {
                format!("{not_read} within {RECOVERY_LIMIT_S} seconds.")
            }
            Some("session-active") => {
                format!("{not_read}: the stopped session's machine was still answering.")
            }
            Some("not-whole" | "upload-failed" | "exists" | "tree-unreadable" | "untrusted-git-dir") => {
                format!("{not_read} whole.")
            }
            Some("off") => "Reading a stopped session's storage back is switched off for this run (run.recover_after_stop)."
                .to_string(),
            Some("instances") => {
                "This run's machine keeps no storage after a session ends, so there was nothing to read back.".to_string()
            }
            Some(_) => format!("{not_read}."),
            // A doubt the service has no word for (files without a readable history, a comparison that was not made, a file that is
            // an earlier version than the saved copy's): the files are stored and not used.
            None if r.kept => format!("{not_used}: its checks left a doubt."),
            None => return None,
        })
    }

    /// `said` with what the read of this session's storage came to, and the command that downloads the files read back when
    /// they are stored and not used.
    fn with_the_read(&self, said: String) -> String {
        let mut said = said;
        if let Some(reason) = self.reason_sentence() {
            said.push(' ');
            said.push_str(&reason);
        }
        if self.kept_unused() {
            said.push_str(&format!(
                " Download the files read back with `{}`.",
                self.read_back_command()
            ));
        }
        said
    }

    /// The notice of the run screen and of `ham watch`: which session was lost and why, which session starts from which copy, and what
    /// that means for the work.
    pub fn sentence(&self) -> String {
        let n = self.session;
        let head = if let Some(head) = self.head_of_the_read() {
            head
        } else if self.cause == CAUSE_NO_HEARTBEAT {
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
            (Some(m), Some(FROM_RECOVERED), _) => self.start_from_the_files_read_back(m, "starts"),
            (Some(m), Some(FROM_START), _) if self.nothing_called_lost() => format!(
                "Session {m} starts from the files the run started with: no copy had been saved."
            ),
            (Some(m), Some(FROM_START), _) => format!(
                "Session {m} starts from the files the run started with: no copy had been saved, so the work of session {n} is lost."
            ),
            (Some(m), _, Some(t)) if self.nothing_called_lost() => {
                format!(
                    "Session {m} starts from the copy saved at {}.",
                    utc_minute(t)
                )
            }
            (Some(m), _, Some(t)) => format!(
                "Session {m} starts from the copy saved at {}; file changes after that copy are lost.",
                utc_minute(t)
            ),
            (Some(m), _, None) if self.nothing_called_lost() => {
                format!("Session {m} starts from the last saved copy.")
            }
            (Some(m), _, None) => format!(
                "Session {m} starts from the last saved copy; file changes after that copy are lost."
            ),
            (None, _, _) => {
                let why = if let Some(at) = self.cancelled_at.as_deref() {
                    // The user had stopped the run. That is why no session follows, whatever the count of lost sessions says.
                    format!(
                        "You had stopped the run at {}, so no new session starts: the run ends",
                        utc_minute(at)
                    )
                } else if self.lost_in_a_row >= self.limit {
                    format!(
                        "It was lost session {} in a row, so the run ends",
                        self.lost_in_a_row
                    )
                } else {
                    "No new session starts, so the run ends".to_string()
                };
                // The files the run delivers are files read back: the ones the service read back from this session's storage
                // and chose, or the ones of an earlier session that this session had started from and left as they were.
                let read_back = self.delivered_read_back();
                // The delivery is named only when the service says there is a copy to deliver.
                match (self.delivers.as_deref(), read_back) {
                    (Some(DELIVERS_COPY), Some(k)) => format!(
                        "{why} and the service delivers the files it read back from session {k}'s storage."
                    ),
                    (Some(DELIVERS_COPY), None) => {
                        format!("{why} and the service delivers the last saved copy.")
                    }
                    (Some(DELIVERS_NOTHING), _) => format!(
                        "{why}. The run saved no copy of its work, so there is nothing to deliver."
                    ),
                    _ => format!("{why}."),
                }
            }
        };
        self.with_the_read(format!("{head} {then}"))
    }

    /// [`SessionLost::sentence`] once the session it names has started: the same facts in the past tense, with the last heartbeat
    /// the service received and the time it called the session lost, which is [`SessionLost::silence_s`] seconds later.
    /// An event that starts no session reads as
    /// [`SessionLost::sentence`].
    pub fn sentence_started(&self) -> String {
        let n = self.session;
        let at = utc_minute(&self.decided_at);
        let head = if let Some(head) = self.head_of_the_read() {
            head
        } else if self.cause == CAUSE_NO_HEARTBEAT {
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
            (Some(m), Some(FROM_RECOVERED), _) => self.start_from_the_files_read_back(m, "started"),
            (Some(m), Some(FROM_START), _) if self.nothing_called_lost() => format!(
                "Session {m} started from the files the run started with: no copy had been saved."
            ),
            (Some(m), Some(FROM_START), _) => format!(
                "Session {m} started from the files the run started with: no copy had been saved, so the work of session {n} was lost."
            ),
            (Some(m), _, Some(t)) if self.nothing_called_lost() => {
                format!(
                    "Session {m} started from the copy saved at {}.",
                    utc_minute(t)
                )
            }
            (Some(m), _, Some(t)) => format!(
                "Session {m} started from the copy saved at {}; file changes after that copy were lost.",
                utc_minute(t)
            ),
            (Some(m), _, None) if self.nothing_called_lost() => {
                format!("Session {m} started from the last saved copy.")
            }
            (Some(m), _, None) => format!(
                "Session {m} started from the last saved copy; file changes after that copy were lost."
            ),
            (None, _, _) => return self.sentence(),
        };
        self.with_the_read(format!("{head} {then}"))
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
            (Some(m), Some(FROM_RECOVERED), _) => format!(
                "session {m} started from the files read back from session {}'s storage",
                self.carried_from().unwrap_or(n)
            ),
            (Some(m), Some(FROM_START), _) if self.nothing_called_lost() => {
                format!("session {m} started from the files the run started with")
            }
            (Some(m), Some(FROM_START), _) => format!(
                "session {m} started from the files the run started with; the work of session {n} was lost"
            ),
            (Some(m), _, Some(t)) if self.nothing_called_lost() => {
                format!(
                    "session {m} started from the copy saved at {}",
                    utc_minute(t)
                )
            }
            (Some(m), _, Some(t)) => format!(
                "session {m} started from the copy saved at {}; file changes after that copy were lost",
                utc_minute(t)
            ),
            (Some(m), _, None) if self.nothing_called_lost() => {
                format!("session {m} started from the last saved copy")
            }
            (Some(m), _, None) => format!(
                "session {m} started from the last saved copy; file changes after that copy were lost"
            ),
            (None, _, _) if self.cancelled_at.is_some() => {
                // With: the copy the run ended with, as the service's decision names it.
                let ended = "you had stopped the run, so no new session started and the run ended";
                match (self.delivers.as_deref(), self.delivered_read_back()) {
                    (Some(DELIVERS_COPY), Some(k)) => {
                        format!(
                            "{ended} with the files the service read back from session {k}'s storage"
                        )
                    }
                    (Some(DELIVERS_COPY), None) => format!("{ended} with the last saved copy"),
                    _ => ended.to_string(),
                }
            }
            (None, _, _) if self.lost_in_a_row >= self.limit => format!(
                "it was lost session {} in a row, so the run ended",
                self.lost_in_a_row
            ),
            (None, _, _) => "no new session started, so the run ended".to_string(),
        };
        // Files that were read back and are not used are named with the command that downloads them.
        let then = if self.kept_unused() {
            format!(
                "{then}; the storage was read back and not used; {} downloads the files read back",
                self.read_back_command()
            )
        } else {
            then
        };
        // A session whose worker gave notice of its end is dated by that notice.
        if self.cause == CAUSE_END_REQUESTED
            && let Some(t) = self.ended_at.as_deref()
        {
            return format!(
                "{LOST_LABEL}session {n}: asked to end {}; {then}",
                utc_minute(t)
            );
        }
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
    /// The service said at the start of the run that it reads a stopped session's storage back
    /// ([`crate::state::RunView::reads_back`]). False for a run that switched the read off, for a machine that keeps no storage, and
    /// on a stage of a release before the read.
    pub reads_back: bool,
    /// When the user's cancel was recorded, for a run whose cancel the service recorded
    /// ([`crate::state::RunView::cancel`]). The service starts no further session for such a run: when it calls this session lost,
    /// the run ends and delivers by the rule of the cancel ([`crate::cancel_note::DELIVERS`]).
    pub cancelled_at: Option<String>,
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
        // The cancel is recorded, so no session follows this one, whatever the count of lost sessions says: the run ends when
        // the service calls the session lost, and delivers by the rule of the cancel lines. A run for which the service reads no
        // storage back is told of no files read back.
        if let Some(cancelled) = self.cancelled_at.as_deref() {
            let stopped = format!(
                "{head} the service calls the session lost. You stopped the run at {}, so no new session starts:",
                utc_minute(cancelled)
            );
            return match (self.reads_back, self.saved_at.as_deref()) {
                (true, _) => format!(
                    "{stopped} the run ends and the service {}.",
                    crate::cancel_note::DELIVERS
                ),
                (false, Some(t)) => format!(
                    "{stopped} the run ends and the service delivers the last saved copy (saved {}).",
                    utc_minute(t)
                ),
                (false, None) => format!(
                    "{stopped} the run ends, and it has saved no copy that the service could deliver."
                ),
            };
        }
        // The service reads the session's storage back before it decides, and it said so. The notice says what the service
        // then does and calls nothing lost: what the read found is in the notice of the lost session. Without the service's word
        // (the read is off for the run, its machine keeps no storage, or the stage predates the read) the sentences below stand.
        if self.reads_back {
            return match (self.would_end, self.saved_at.as_deref()) {
                (Some(k), Some(_)) => format!(
                    "{head} the session is lost; it would be lost session {k} in a row, so the run would end. The service would read \
                     the session's storage back and deliver those files when its checks leave no doubt, and the last saved copy \
                     otherwise."
                ),
                (Some(k), None) => format!(
                    "{head} the session is lost; it would be lost session {k} in a row, so the run would end. The service would read \
                     the session's storage back and deliver those files when its checks leave no doubt; the run has saved no copy \
                     that it could deliver otherwise."
                ),
                (None, Some(t)) => format!(
                    "{head} it reads the session's storage back and starts a new session when the run has time left: from the files \
                     it read back when its checks leave no doubt, and from the last saved copy (saved {}) otherwise.",
                    utc_minute(t)
                ),
                (None, None) => format!(
                    "{head} it reads the session's storage back and starts a new session when the run has time left: from the files \
                     it read back when its checks leave no doubt, and from the files the run started with otherwise, because the run \
                     has saved no copy yet."
                ),
            };
        }
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
