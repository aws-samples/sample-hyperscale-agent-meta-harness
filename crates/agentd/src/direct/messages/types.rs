//! The words of message control: what a message is, the states it passes through, why it can be refused, and what the
//! server answers to each call agentd makes. These are agentd's own types; `http.rs` maps them to and from the wire records of
//! `harness_protocol::messages` (protocol 1.38.0) so that nothing else in the module knows the wire.

use std::fmt;

/// The longest text agentd sends to the engine as one message.
/// The protocol's one definition.
pub const MAX_TEXT_BYTES: usize = harness_protocol::messages::MAX_MESSAGE_TEXT_BYTES;

/// How a message is delivered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Mode {
    /// When the current prompt turn ends, as its own user turn. The default.
    #[default]
    Next,
    /// Cancel the current turn, wait for it to settle, then prompt on the same session.
    Now,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Next => "next",
            Self::Now => "now",
        }
    }
}

/// What the server tells agentd about a message in the hand-out: that it exists, never what it says. The text comes with the claim
/// (protocol 1.38.0 `MessageNotice`). A hand-out only transports; it changes no state and can repeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub id: String,
    /// The server's order (0: unknown, arrival order decides).
    pub seq: u64,
    pub mode: Mode,
    /// The session id the message was written against.
    pub epoch: String,
    pub bytes: usize,
}

/// A message with its text: what a claim returns, and what the server holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub text: String,
    pub mode: Mode,
    pub epoch: String,
    pub seq: u64,
}

impl Message {
    pub fn notice(&self) -> Notice {
        Notice {
            id: self.id.clone(),
            seq: self.seq,
            mode: self.mode,
            epoch: self.epoch.clone(),
            bytes: self.text.len(),
        }
    }
}

/// Why a message ended without running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RejectReason {
    RunSealed,
    RunCancelled,
    EngineBusy,
    NoMessageControl,
    ContinuationLimit,
    Limit,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunSealed => "run-sealed",
            Self::RunCancelled => "run-cancelled",
            Self::EngineBusy => "engine-busy",
            Self::NoMessageControl => "no-message-control",
            Self::ContinuationLimit => "continuation-limit",
            Self::Limit => "limit",
        }
    }
}

/// Why a claimed message never reached the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailReason {
    /// The run ended (or the vote was dropped) between the claim and the prompt.
    NotSent,
    /// The text is longer than agentd sends.
    TooLarge,
    /// The session changed between the claim and the prompt.
    SessionChanged,
}

impl FailReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotSent => "not-sent",
            Self::TooLarge => "too-large",
            Self::SessionChanged => "session-changed",
        }
    }
}

/// The states of a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Durably accepted. HTTP 202 means exactly this; the next state follows at once.
    Accepted,
    /// Eligible.
    Pending,
    /// Written against a session that is gone; waits for the user to confirm it.
    Held,
    /// agentd won the claim and is sending it.
    Dispatching,
    /// agentd sent the prompt to the engine in turn `turn`. It does not mean the engine acted on it.
    Delivered {
        turn: u32,
    },
    Withdrawn,
    Rejected(RejectReason),
    /// A confirmed copy replaced it.
    Superseded,
    Failed(FailReason),
    /// Claimed, and the worker was lost before the outcome was known. Never sent again.
    DeliveryUnknown,
}

impl State {
    /// An end state: nothing follows it.
    pub fn is_end(&self) -> bool {
        !matches!(
            self,
            Self::Accepted | Self::Pending | Self::Held | Self::Dispatching
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Pending => "pending",
            Self::Held => "held",
            Self::Dispatching => "dispatching",
            Self::Delivered { .. } => "delivered",
            Self::Withdrawn => "withdrawn",
            Self::Rejected(_) => "rejected",
            Self::Superseded => "superseded",
            Self::Failed(_) => "failed",
            Self::DeliveryUnknown => "delivery-unknown",
        }
    }
}

/// Why a claim gave agentd nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The message is not pending (withdrawn, held, taken, ended): the state's name.
    NotPending(String),
    /// The message was written against another session than the one agentd claims for.
    EpochMoved,
    /// The user's cancel is queued: it wins.
    Cancelled,
    /// Admission is closed.
    Sealed,
    /// The caller is a worker generation that has been replaced.
    StaleGeneration,
    Unknown,
}

/// The answer to a claim.
/// A message the control plane says this agentd holds in `dispatching`: one answer to a claim without a message id (protocol 1.38.0).
/// The module asks it after a claim whose answer may have been lost and before it seals, to learn what it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub message: Message,
    /// The claim repeats one this agentd incarnation made earlier (the control plane's `redelivered`), as opposed to a pending message
    /// the question claimed now.
    pub redelivered: bool,
    /// The message was claimed in the session agentd holds now. A message claimed in an older session must not be sent into this one.
    pub current_session: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimOutcome {
    Claimed(Message),
    Refused(Refusal),
}

/// What kind of seal agentd asks for (protocol 1.38.0 `SealMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SealMode {
    /// "Anything pending? if not, the run is done": eligible messages come back, otherwise admission closes.
    Normal,
    /// The continuation allowance is spent: admission closes and pending messages are rejected for the limit instead of returned.
    LimitReached,
    /// The run ends whatever happens (the engine is gone, the user cancelled, the deadline passed, no time is left for another turn):
    /// admission closes, pending and held messages are rejected, no continuation can come back.
    Terminal,
}

impl SealMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::LimitReached => "limit-reached",
            Self::Terminal => "terminal",
        }
    }
}

/// The answer to a seal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SealOutcome {
    /// Eligible messages exist in the session: they come back in server order, admission stays open, and agentd claims and sends
    /// them as the next prompts.
    Pending(Vec<Notice>),
    /// Nothing is pending: admission is closed and nothing more is accepted. `watermark` counts the messages the run accepted.
    Sealed { watermark: u64 },
}

/// A call to the store failed. `retryable` is the transport's reading (a timeout, a 5xx, a throttle); a refusal the server meant is
/// not an error but an outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError {
    pub retryable: bool,
    pub detail: String,
}

impl StoreError {
    pub fn retryable(detail: impl Into<String>) -> Self {
        Self {
            retryable: true,
            detail: detail.into(),
        }
    }

    pub fn fatal(detail: impl Into<String>) -> Self {
        Self {
            retryable: false,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({})",
            self.detail,
            if self.retryable { "retryable" } else { "final" }
        )
    }
}

impl std::error::Error for StoreError {}
