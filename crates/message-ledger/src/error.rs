//! What the ledger refuses, and how an HTTP caller says it.

use crate::table::TableError;
use harness_protocol::messages::{MessageState, RejectReason, codes};

/// A refusal is an answer the caller acts on, not a failure of the store: it carries its HTTP status and its error code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The request is not valid on its face (400 `bad-input`).
    BadInput(String),
    /// The run, the message or the session does not exist for this caller (404).
    NotFound,
    /// The run no longer takes messages (409 `run-sealed`); the reason says how it closed.
    RunSealed(Option<RejectReason>),
    /// The most messages that may wait are outstanding already (409 `too-many-pending`).
    TooManyPending,
    /// The run reached its cap on messages or bytes (409 `run-limit`).
    RunLimit(String),
    /// The run or the owner reached its admissions per minute (429 `rate-limited`); the scope is `run` or `owner`.
    RateLimited(&'static str),
    /// The id was used with another payload (409 `idempotency-conflict`).
    IdempotencyConflict,
    /// The message a confirmation names is not held (409 `not-held`).
    NotHeld(MessageState),
    /// The session the user was shown is not the current one (409 `epoch-changed`).
    EpochChanged,
    /// A confirmation repeats neither the text nor the mode of the message it replaces (409 `confirm-mismatch`).
    ConfirmMismatch,
    /// A claimed message cannot be withdrawn (409 `already-dispatching`).
    AlreadyDispatching,
    /// The message is past the point where it can be withdrawn (409 `not-withdrawable`).
    NotWithdrawable(MessageState),
    /// A newer agentd holds the run (409 `stale-generation`).
    StaleGeneration,
    /// The session named is not the current one (409 `stale-epoch`).
    StaleEpoch,
    /// agentd has not reported a session (409 `no-session`).
    NoSession,
    /// The transition asked for is not one this message can make (409 `bad-transition`).
    BadTransition(String),
    /// The outcome answers an earlier claim attempt than the current one (409 `stale-attempt`).
    StaleAttempt,
}

impl Refusal {
    pub fn status(&self) -> u16 {
        match self {
            Refusal::BadInput(_) => 400,
            Refusal::NotFound => 404,
            Refusal::RateLimited(_) => 429,
            _ => 409,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Refusal::BadInput(_) => "bad-input",
            Refusal::NotFound => "not-found",
            Refusal::RunSealed(_) => codes::RUN_SEALED,
            Refusal::TooManyPending => codes::TOO_MANY_PENDING,
            Refusal::RunLimit(_) => codes::RUN_LIMIT,
            Refusal::RateLimited(_) => codes::RATE_LIMITED,
            Refusal::IdempotencyConflict => codes::IDEMPOTENCY_CONFLICT,
            Refusal::NotHeld(_) => codes::NOT_HELD,
            Refusal::EpochChanged => codes::EPOCH_CHANGED,
            Refusal::ConfirmMismatch => codes::CONFIRM_MISMATCH,
            Refusal::AlreadyDispatching => codes::ALREADY_DISPATCHING,
            Refusal::NotWithdrawable(_) => codes::NOT_WITHDRAWABLE,
            Refusal::StaleGeneration => codes::STALE_GENERATION,
            Refusal::StaleEpoch => codes::STALE_EPOCH,
            Refusal::NoSession => codes::NO_SESSION,
            Refusal::BadTransition(_) => codes::BAD_TRANSITION,
            Refusal::StaleAttempt => codes::STALE_ATTEMPT,
        }
    }

    /// The words the person (or agentd's log) reads, for a run of Cline.
    pub fn message(&self) -> String {
        self.message_for("Cline")
    }

    /// [`message`](Self::message) for a run of the engine `engine`: the two refusals that name the engine say its name.
    pub fn message_for(&self, engine: &str) -> String {
        match self {
            Refusal::BadInput(m) | Refusal::RunLimit(m) | Refusal::BadTransition(m) => m.clone(),
            Refusal::NotFound => "no such message for this caller".to_string(),
            Refusal::RunSealed(reason) => match reason {
                Some(RejectReason::RunCancelled) => {
                    "the run was stopped; it takes no more messages".to_string()
                }
                Some(RejectReason::ContinuationLimit) => {
                    "the run reached its limit of turns after done; start a follow-up run".to_string()
                }
                _ => "the run is ending and takes no more messages; the work is delivered, start a follow-up run".to_string(),
            },
            Refusal::TooManyPending => format!(
                "{} messages are waiting for {engine} already; wait for one to be delivered or withdraw one",
                harness_protocol::messages::MAX_OUTSTANDING_MESSAGES
            ),
            Refusal::RateLimited(scope) => match *scope {
                "owner" => "too many messages from you in the last minute; wait a moment".to_string(),
                _ => "too many messages to this run in the last minute; wait a moment".to_string(),
            },
            Refusal::IdempotencyConflict => {
                "this message id was used with different text or mode".to_string()
            }
            Refusal::NotHeld(s) => format!(
                "the message to confirm is {}, not held; read the run's messages again",
                s.word()
            ),
            Refusal::EpochChanged => {
                "the session changed again while you were looking; read the run's messages and confirm again".to_string()
            }
            Refusal::ConfirmMismatch => {
                "a confirmation must repeat the text and the mode of the held message".to_string()
            }
            Refusal::AlreadyDispatching => {
                format!("{engine} is already taking this message; it cannot be withdrawn")
            }
            Refusal::NotWithdrawable(s) => {
                format!("a message that is {} cannot be withdrawn", s.word())
            }
            Refusal::StaleGeneration => "a newer agentd holds this run".to_string(),
            Refusal::StaleEpoch => "the session named is not the current session".to_string(),
            Refusal::NoSession => "no session was reported for this run".to_string(),
            Refusal::StaleAttempt => {
                "that claim attempt is over; the message was claimed again or never claimed".to_string()
            }
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LedgerError {
    #[error("{0}")]
    Refused(Refusal),
    /// The store could not be read or written; the caller answers 503 and the client retries.
    #[error("the message store is unavailable: {0}")]
    Unavailable(String),
}

impl From<Refusal> for LedgerError {
    fn from(r: Refusal) -> Self {
        LedgerError::Refused(r)
    }
}

impl From<TableError> for LedgerError {
    fn from(e: TableError) -> Self {
        LedgerError::Unavailable(e.to_string())
    }
}
