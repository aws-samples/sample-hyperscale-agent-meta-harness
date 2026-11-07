//! Protocol 1.38.0: a message from the user to a running Cline.
//!
//! A message is written by control-api (`POST /v1/runs/{runId}/messages`), carries a status the server owns, and reaches the engine
//! through agentd in four steps that agentd takes with worker-api: it reports its session (the epoch), it is told about pending
//! messages in the response to its event posts (hand-out; it changes nothing), it claims a message before it sends it (one conditional
//! write: pending to dispatching) and it seals the run with the one atomic question "anything pending? if not, the run is done". The
//! claim is why a message is delivered at most once or is reported as unknown ([`MessageState::DeliveryUnknown`]); the seal is why a
//! message that arrives at the last moment is delivered or answered with a clear refusal and never vanishes.
//!
//! The types here are the contract of those calls and of the two store items behind them ([`MessageItem`], [`MessageCtlItem`]). The
//! rules that move a message from one state to the next live in the `message-ledger` crate; this module holds only data, the limits
//! and the checks that need no store.

use crate::common::is_ulid;
use crate::direct_input::MAX_INPUT_TEXT_BYTES;
use crate::store::{Key, Keyed, keys};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The harness control an Agent registers when its agentd takes a message at the end of a turn.
pub const MESSAGE_NEXT_CONTROL: &str = "message.next";
/// The harness control an Agent registers when its agentd also takes a message as an interrupt (`/now`).
pub const MESSAGE_NOW_CONTROL: &str = "message.now";

/// A message holds at most this many bytes, as an answer does.
pub const MAX_MESSAGE_TEXT_BYTES: usize = MAX_INPUT_TEXT_BYTES;
/// A run holds at most this many pending, held and dispatching messages together.
pub const MAX_OUTSTANDING_MESSAGES: u32 = 3;
/// A run accepts at most this many bytes of message text in total, superseded messages included: sixteen messages of the largest size. A guard
/// against a run that is fed without end, not a limit on a message.
pub const MAX_RUN_MESSAGE_BYTES: u64 = 4 * 1024 * 1024;
/// A run accepts at most this many messages.
pub const MAX_RUN_MESSAGES: u32 = 100;
/// A run accepts at most this many admissions in one clock minute.
pub const MAX_RUN_ADMISSIONS_PER_MINUTE: u32 = 10;
/// An owner is admitted at most this many times in one clock minute across all runs.
pub const MAX_OWNER_ADMISSIONS_PER_MINUTE: u32 = 30;
/// One claim call takes at most this many messages. agentd sends each claimed message as a prompt of its own, in the order the server
/// gave (`now` first), so several messages are several turns and never one prompt.
pub const MAX_CLAIM_BATCH: u8 = 3;
/// The detail of a failed message is cut to this many characters.
pub const MAX_DETAIL_CHARS: usize = 200;
/// A message keeps at most this many history steps (a message passes through at most six states).
pub const MAX_HISTORY_STEPS: usize = 16;
/// A control item remembers this many replaced agentd incarnations (the oldest are forgotten).
pub const MAX_RETIRED_GENERATIONS: usize = 32;
/// An agentd incarnation the server has not seen replaces the current one unless its ULID is older than the current one's by more than
/// this many milliseconds: that is the delayed first report of an incarnation that was replaced already. It tolerates a replacement
/// microVM whose clock runs a little behind.
pub const GENERATION_CLOCK_SKEW_MS: u64 = 60_000;
/// A rate counter item lives this long past its minute (DynamoDB `ttl`, housekeeping only).
pub const OWNER_RATE_TTL_S: u64 = 300;

/// When a message is delivered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MessageMode {
    /// At the end of the current turn, as its own user turn (the default).
    #[default]
    Next,
    /// Now: agentd cancels the current turn, waits for it to settle and prompts on the same session.
    Now,
}

impl MessageMode {
    pub fn word(&self) -> &'static str {
        match self {
            MessageMode::Next => "next",
            MessageMode::Now => "now",
        }
    }
}

/// The status of a message, as the server holds it. HTTP 202 on the send means `pending` and nothing more: Cline has not
/// read the message yet. `delivered` means agentd sent the prompt to the engine, not that the engine acted on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MessageState {
    /// Durably accepted and eligible: it will be claimed at the end of the turn (`next`) or now (`now`).
    Pending,
    /// Pending when the session changed: it waits for the user to confirm it again and never makes the run wait.
    Held,
    /// agentd won the claim and is submitting it.
    Dispatching,
    /// agentd reported sending the prompt; `deliveredTurn` names the turn.
    Delivered,
    /// The user withdrew it before it was claimed.
    Withdrawn,
    /// The run could not take it; `reason` says why.
    Rejected,
    /// A confirmed copy replaced it (the only way into this state).
    Superseded,
    /// agentd could not submit it (it was never sent); `detail` says why.
    Failed,
    /// It was claimed, then the worker was lost before the outcome was known: it may have reached Cline. It is never sent again by itself.
    DeliveryUnknown,
}

impl MessageState {
    pub fn word(&self) -> &'static str {
        match self {
            MessageState::Pending => "pending",
            MessageState::Held => "held",
            MessageState::Dispatching => "dispatching",
            MessageState::Delivered => "delivered",
            MessageState::Withdrawn => "withdrawn",
            MessageState::Rejected => "rejected",
            MessageState::Superseded => "superseded",
            MessageState::Failed => "failed",
            MessageState::DeliveryUnknown => "delivery-unknown",
        }
    }

    /// Counts toward the cap of outstanding messages and can still change by itself.
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            MessageState::Pending | MessageState::Held | MessageState::Dispatching
        )
    }

    /// No transition by the harness follows (a `held` or `delivery-unknown` message can still be replaced by the user's confirmed copy,
    /// but that is a new message).
    pub fn is_settled(&self) -> bool {
        !self.is_open()
    }

    /// Only a message nobody has claimed can be withdrawn.
    pub fn can_withdraw(&self) -> bool {
        matches!(self, MessageState::Pending | MessageState::Held)
    }
}

/// Why a message was rejected. Every reason but `engine-busy` and `continuation-limit` is the server's alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RejectReason {
    /// The run was sealed: it ended or was ending when the message was about to be sent.
    RunSealed,
    /// The user stopped the run; a Cancel wins over a message.
    RunCancelled,
    /// agentd could not take the message into its queue.
    EngineBusy,
    /// The run's Agent registered no message control (a run on an older image).
    NoMessageControl,
    /// The allowance of message-driven turns after "done" is used up.
    ContinuationLimit,
    /// A limit (the cap on messages or bytes of one run).
    Limit,
}

impl RejectReason {
    pub fn word(&self) -> &'static str {
        match self {
            RejectReason::RunSealed => "run-sealed",
            RejectReason::RunCancelled => "run-cancelled",
            RejectReason::EngineBusy => "engine-busy",
            RejectReason::NoMessageControl => "no-message-control",
            RejectReason::ContinuationLimit => "continuation-limit",
            RejectReason::Limit => "limit",
        }
    }

    /// The reasons agentd may post in [`MessageOutcome::Rejected`].
    pub fn agent_may_set(&self) -> bool {
        matches!(
            self,
            RejectReason::EngineBusy | RejectReason::ContinuationLimit
        )
    }
}

/// Whether new messages are admitted. It closes once, in the seal (or at finalize or cancel), and never opens again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Admission {
    #[default]
    Open,
    Closed,
}

/// One step of a message's history: the state it entered and when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageStep {
    pub state: MessageState,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RejectReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
}

// ---- the client routes ----------------------------------------------------------------------------------------------------

/// The held (or delivery-unknown) message a new message confirms, and the session the user saw. The server refuses when
/// the message is no longer held (or unknown) or the session is no longer the current one, and then sends nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfirmHeld {
    pub message_id: String,
    /// The current session (the epoch) the user was shown.
    pub epoch: String,
}

/// `POST /v1/runs/{runId}/messages` (owner only; protocol 1.38.0). The client mints the message id (a ULID): it is the idempotency key,
/// so a retry with the same id and the same payload returns the stored message and uses no slot, and the same id with another payload is
/// 409 `idempotency-conflict`. A confirmation (`confirms`) must repeat the text and mode of the message it replaces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendMessageRequest {
    pub message_id: String,
    pub text: String,
    #[serde(default)]
    pub mode: MessageMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirms: Option<ConfirmHeld>,
}

impl SendMessageRequest {
    /// SHA-256 (hex) over what a retry must repeat: the mode, the confirmed message and session, and the text, each field prefixed with its
    /// length. The id is the key, not payload.
    pub fn payload_sha256(&self) -> String {
        let (cid, cep) = self
            .confirms
            .as_ref()
            .map_or(("", ""), |c| (c.message_id.as_str(), c.epoch.as_str()));
        // Length-prefixed fields: an epoch is an engine's session id and may hold a line break, so a separator alone could not tell
        // where a field ends.
        let mode = self.mode.word();
        let canonical = format!(
            "{}:{mode}\n{}:{cid}\n{}:{cep}\n{}",
            mode.len(),
            cid.len(),
            cep.len(),
            self.text
        );
        Sha256::digest(canonical.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// What is wrong with a [`SendMessageRequest`] before anything is read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MessageInputError {
    #[error("messageId must be a ULID the client made for this message")]
    BadId,
    #[error("the message is blank; write something for Cline")]
    BlankText,
    #[error("the message is {bytes} bytes; the limit is {MAX_MESSAGE_TEXT_BYTES} bytes (256 KiB)")]
    TextTooLong { bytes: usize },
    #[error("confirms must name another message (a ULID) and the session the user was shown")]
    BadConfirm,
}

/// The checks of a send that need no store: the id, the text and the confirmation's shape.
pub fn validate_send(req: &SendMessageRequest) -> Result<(), MessageInputError> {
    if !is_ulid(&req.message_id) {
        return Err(MessageInputError::BadId);
    }
    if req.text.trim().is_empty() {
        return Err(MessageInputError::BlankText);
    }
    if req.text.len() > MAX_MESSAGE_TEXT_BYTES {
        return Err(MessageInputError::TextTooLong {
            bytes: req.text.len(),
        });
    }
    if let Some(c) = &req.confirms
        && (!is_ulid(&c.message_id) || c.message_id == req.message_id || c.epoch.trim().is_empty())
    {
        return Err(MessageInputError::BadConfirm);
    }
    Ok(())
}

/// A message as the owner sees it: everything the client needs to draw the pending row, nothing only the server needs (not the principal,
/// the payload digest, the claiming worker or the item version).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageView {
    pub message_id: String,
    /// The order the server admitted it in (1 is the first message of the run).
    pub seq: u32,
    pub mode: MessageMode,
    pub state: MessageState,
    pub text: String,
    /// The session (the engine's session id) the message was written against; absent before the first session started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    /// The number of that session within the run (1, 2, 3 ...): "written to session 2, now session 3".
    pub epoch_no: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RejectReason>,
    /// Why a message failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The turn that carried it, once delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_turn: Option<u32>,
    /// The id of the held message this one confirms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirms: Option<String>,
    /// The id of the confirmed copy that replaced this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    pub accepted_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<MessageStep>,
}

/// Whether a send stored a new message or found the one a retry repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SendOutcome {
    /// HTTP 202: durably accepted as `pending`.
    Accepted,
    /// HTTP 200: the id was seen before with the same payload; `message` is its current state.
    AlreadyAccepted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageResponse {
    pub outcome: SendOutcome,
    pub message: MessageView,
}

/// One message: `GET /v1/runs/{runId}/messages/{messageId}` and the answer of a withdrawal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageResponse {
    pub message: MessageView,
}

/// `GET /v1/runs/{runId}/messages`: every message of the run in the order admitted (at most [`MAX_RUN_MESSAGES`]), with what the client
/// needs to confirm a held one: the current session and whether the run still takes messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessagesPage {
    pub run_id: String,
    pub messages: Vec<MessageView>,
    pub admission: Admission,
    /// The current session (the epoch); absent before the first session started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    pub epoch_no: u32,
    /// How many messages the run has admitted so far.
    pub watermark: u32,
}

// ---- agentd's side --------------------------------------------------------------------------------------------------------

/// A pending message as agentd is told about it: no text (the claim returns that). It is handed out in every events response while the
/// message is pending in the current session; the hand-out changes nothing, so it can repeat, and agentd treats a repeat as the same message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageNotice {
    pub message_id: String,
    pub seq: u32,
    pub mode: MessageMode,
    /// The session it is pending in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    pub bytes: u32,
    pub accepted_at: String,
}

/// `POST /internal/message-session`: agentd tells the server which incarnation of agentd (`generation`, a ULID made at its start) holds
/// which session (`epoch`, the engine's session id). An incarnation the server has not seen replaces the current one, unless its ULID is
/// older than the current one's by more than [`GENERATION_CLOCK_SKEW_MS`]; one the server has replaced is refused for good. The server
/// commits it before any later claim: a message pending in an older session becomes `held`, a message the replaced incarnation was
/// dispatching becomes `delivery-unknown`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionReport {
    pub run_id: String,
    pub agent_id: String,
    pub generation: String,
    pub epoch: String,
    /// The ordinal of this session within the incarnation: 1 for its first session, then 2, 3 ... A report whose number is lower than the
    /// last one of the incarnation is the late retry of an older report and changes nothing; the same number with another `epoch` is refused.
    pub number: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionAck {
    /// The number of the session within the run.
    pub epoch_no: u32,
    /// Messages this report moved from `pending` to `held`.
    pub held: u32,
    /// Messages this report moved from `dispatching` to `delivery-unknown`.
    pub delivery_unknown: u32,
    pub admission: Admission,
}

/// `POST /internal/message-claim`: "give me the next pending message of this session". With `messageId` it claims that message (a `now`
/// message agentd was told about); without it, the oldest `now` message, else the oldest `next` message, in server order. `limit` (1 to
/// [`MAX_CLAIM_BATCH`], default 1) takes several at once. A claim is one conditional write per message, fenced by `generation` and `epoch`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimRequest {
    pub run_id: String,
    pub agent_id: String,
    pub generation: String,
    pub epoch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u8>,
}

/// A message agentd claimed: its text, which agentd sends exactly once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimedMessage {
    pub message_id: String,
    pub seq: u32,
    pub mode: MessageMode,
    pub epoch_no: u32,
    /// Which claim of the message this is: 1 for the first, one more each time a claim was given back (`released`) and taken again. The
    /// outcome names it, so a late retry of an earlier attempt's outcome cannot settle a later attempt. A redelivery repeats the number.
    pub attempt: u32,
    pub text: String,
    pub accepted_at: String,
}

/// Why the server would not hand out a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ClaimRefusal {
    /// A run-level Cancel is queued: it wins, and the pending messages end `rejected` with `run-cancelled`.
    CancelQueued,
    /// The seal closed admission: nothing is pending and nothing will be.
    AdmissionClosed,
}

/// The answer to a claim. The fencing errors (a stale generation or session) are HTTP 409 with their own codes, not an outcome: they mean
/// this agentd must stop acting on messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum ClaimResponse {
    /// The messages are `dispatching` now and belong to this generation, interrupts first, then oldest first.
    ///
    /// `redelivered` is true when these are messages this generation claimed earlier and has posted no outcome for: the retry of a claim whose
    /// answer was lost. agentd that already sent one of them must not send it again; it posts the outcome it has.
    #[serde(rename_all = "camelCase")]
    Claimed {
        // at most MAX_CLAIM_BATCH (3): the bound is in the schema too
        #[schemars(length(max = 3))]
        messages: Vec<ClaimedMessage>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redelivered: bool,
    },
    /// Nothing is eligible in this session.
    #[serde(rename_all = "camelCase")]
    Nothing { admission: Admission },
    /// The message named is not pending any more (withdrawn, claimed, rejected ...): `state` says what it is.
    #[serde(rename_all = "camelCase")]
    NotPending { state: MessageState },
    #[serde(rename_all = "camelCase")]
    Refused { reason: ClaimRefusal },
}

/// How a seal asks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SealMode {
    /// "Anything pending? if not, the run is done": pending messages come back; none closes admission.
    #[default]
    Normal,
    /// The allowance of message-driven turns is used up: pending and held messages end `rejected` with `continuation-limit`
    /// and admission closes.
    LimitReached,
    /// The run is ending whatever is pending (shutdown, finalize): pending and held messages end `rejected` with `run-sealed`, a
    /// dispatching one becomes `delivery-unknown`, admission closes, nothing comes back.
    Terminal,
}

/// `POST /internal/message-seal`: agentd's last question before it declares the run done, and its closing call at shutdown. The server
/// answers with one conditional write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SealRequest {
    pub run_id: String,
    pub agent_id: String,
    pub generation: String,
    pub epoch: String,
    #[serde(default)]
    pub mode: SealMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum SealResponse {
    /// Eligible messages exist in this session, in server order: agentd claims and sends them, then asks again. Admission stays open.
    #[serde(rename_all = "camelCase")]
    Pending {
        // at most MAX_OUTSTANDING_MESSAGES (3) can be pending: the bound is in the schema too
        #[schemars(length(max = 3))]
        messages: Vec<MessageNotice>,
    },
    /// Admission is closed. After this a send is 409 `run-sealed` and stores nothing. `watermark` is how many messages the run admitted.
    #[serde(rename_all = "camelCase")]
    Sealed {
        watermark: u32,
        /// Pending and held messages this seal rejected.
        rejected: u32,
        /// Dispatching messages this seal turned into `delivery-unknown`.
        delivery_unknown: u32,
    },
}

/// What agentd tells the server about a message it claimed (or, for `rejected`, one it will not take).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MessageOutcome {
    /// The prompt was sent to the engine in `turn`: `dispatching` becomes `delivered`.
    #[serde(rename_all = "camelCase")]
    Submitted { turn: u32 },
    /// The message was not sent and cannot be: `dispatching` becomes `failed`. `detail` is cut to [`MAX_DETAIL_CHARS`].
    #[serde(rename_all = "camelCase")]
    Failed { detail: String },
    /// The claim is given back (an interrupt that could not cancel the turn): `dispatching` becomes `pending` again, in mode `next`.
    Released,
    /// agentd will not take it: `pending` or `dispatching` becomes `rejected`. Only the reasons [`RejectReason::agent_may_set`] names.
    #[serde(rename_all = "camelCase")]
    Rejected { reason: RejectReason },
}

/// `POST /internal/message-outcome`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutcomeRequest {
    pub run_id: String,
    pub agent_id: String,
    pub generation: String,
    /// The session agentd is in now. It is informative: an outcome is fenced by the incarnation and the attempt, because a session may
    /// change between a claim and its outcome.
    pub epoch: String,
    pub message_id: String,
    /// The `attempt` of the claim this answers ([`ClaimedMessage::attempt`]); for `rejected` of a message agentd never claimed, 0.
    pub attempt: u32,
    pub outcome: MessageOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OutcomeResponse {
    pub message_id: String,
    pub state: MessageState,
}

/// The error codes of the message routes (the `code` of an [`crate::api::ApiError`]).
pub mod codes {
    /// The run's Agent registered no `message.next` (or no `message.now` for a `now` message): a run on an older image.
    pub const NO_MESSAGE_CONTROL: &str = "no-message-control";
    /// The run was sealed: it ended or is ending.
    pub const RUN_SEALED: &str = "run-sealed";
    /// A needs-input question is open: the text is its answer (`POST /v1/runs/{runId}/input`), not a message.
    pub const QUESTION_OPEN: &str = "question-open";
    /// The id was used with another payload.
    pub const IDEMPOTENCY_CONFLICT: &str = "idempotency-conflict";
    /// Three messages are outstanding already.
    pub const TOO_MANY_PENDING: &str = "too-many-pending";
    /// The run reached its cap on messages or bytes.
    pub const RUN_LIMIT: &str = "run-limit";
    /// The run or the owner reached its admissions per minute.
    pub const RATE_LIMITED: &str = "rate-limited";
    /// The message named by a confirmation is not held (or unknown).
    pub const NOT_HELD: &str = "not-held";
    /// The session the user was shown is not the current one.
    pub const EPOCH_CHANGED: &str = "epoch-changed";
    /// A confirmation repeats the text and mode of the message it replaces.
    pub const CONFIRM_MISMATCH: &str = "confirm-mismatch";
    /// A claimed message cannot be withdrawn.
    pub const ALREADY_DISPATCHING: &str = "already-dispatching";
    /// The message is in a state that cannot be withdrawn.
    pub const NOT_WITHDRAWABLE: &str = "not-withdrawable";
    /// This agentd generation was replaced by a newer one.
    pub const STALE_GENERATION: &str = "stale-generation";
    /// The session named is not the current one.
    pub const STALE_EPOCH: &str = "stale-epoch";
    /// agentd has not reported a session yet.
    pub const NO_SESSION: &str = "no-session";
    /// The transition asked for is not one the message can make.
    pub const BAD_TRANSITION: &str = "bad-transition";
    /// The outcome answers an earlier claim attempt than the current one.
    pub const STALE_ATTEMPT: &str = "stale-attempt";
}

// ---- the store items ------------------------------------------------------------------------------------------------------

/// `RUN#<runId>` / `MSG#<messageId>`: the full record of one message. control-api creates it; worker-api and the ledger change its state
/// under a condition on the state it read. The members avoid `ownerPrincipalArn` and `createdAt` together, so the message stays out of the
/// sparse GSI1 that lists runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageItem {
    pub run_id: String,
    pub message_id: String,
    pub seq: u32,
    /// The exact principal ARN of the caller who sent it (the run's owner), kept on every message for audit.
    pub principal_arn: String,
    pub text: String,
    pub mode: MessageMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    pub epoch_no: u32,
    pub state: MessageState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RejectReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_turn: Option<u32>,
    /// The message this one confirms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirms: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// The generation of agentd that holds the claim, while `dispatching`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// How many times the message has been claimed (0 before the first claim).
    #[serde(default)]
    pub attempt: u32,
    pub payload_sha256: String,
    pub accepted_at: String,
    pub updated_at: String,
    pub history: Vec<MessageStep>,
    pub version: u64,
}

impl MessageItem {
    /// The owner's view of the message.
    pub fn view(&self) -> MessageView {
        MessageView {
            message_id: self.message_id.clone(),
            seq: self.seq,
            mode: self.mode,
            state: self.state,
            text: self.text.clone(),
            epoch: self.epoch.clone(),
            epoch_no: self.epoch_no,
            reason: self.reason,
            detail: self.detail.clone(),
            delivered_turn: self.delivered_turn,
            confirms: self.confirms.clone(),
            superseded_by: self.superseded_by.clone(),
            accepted_at: self.accepted_at.clone(),
            updated_at: self.updated_at.clone(),
            history: self.history.clone(),
        }
    }
}

/// One open message in [`MessageCtlItem::open`]: what the hand-out, the seal and the cap need without reading the message items.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenMessage {
    pub message_id: String,
    pub seq: u32,
    pub mode: MessageMode,
    pub state: MessageState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    pub bytes: u32,
    pub accepted_at: String,
}

impl OpenMessage {
    pub fn notice(&self) -> MessageNotice {
        MessageNotice {
            message_id: self.message_id.clone(),
            seq: self.seq,
            mode: self.mode,
            epoch: self.epoch.clone(),
            bytes: self.bytes,
            accepted_at: self.accepted_at.clone(),
        }
    }
}

/// `RUN#<runId>` / `MSGCTL`: the one item every message transition of a run goes through. Each change reads it and writes it back under a
/// condition on `version`, so the admission of a message, a claim, a withdrawal, a session report and the seal are linearized: a late
/// message and the seal cannot both win. It is a sibling of the Run item (not a member of it) because the Run item is replaced whole by
/// its writers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageCtlItem {
    pub run_id: String,
    pub version: u64,
    pub admission: Admission,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_reason: Option<RejectReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    /// The incarnation of agentd that may claim (a ULID); absent until the first session report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    /// The current session (the engine's session id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    pub epoch_no: u32,
    /// The number of the current session within the current incarnation ([`SessionReport::number`]).
    #[serde(default)]
    pub session_number: u32,
    /// The incarnations of agentd this run has replaced, oldest first (at most [`MAX_RETIRED_GENERATIONS`]). A report or call from one is
    /// refused with `stale-generation`; an incarnation the server has not seen replaces the current one, unless its ULID is older than the
    /// current one's by more than [`GENERATION_CLOCK_SKEW_MS`]. Within that margin the order of the ULIDs does not decide, so a
    /// replacement VM whose clock is a little behind the old one's is still the new agentd.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired: Vec<String>,
    /// Messages admitted so far; the last one's `seq`.
    pub seq: u32,
    /// Bytes of message text admitted so far, superseded messages included.
    pub bytes_total: u64,
    /// The clock minute (Unix minutes) `rate_count` counts.
    pub rate_minute: u64,
    pub rate_count: u32,
    /// The messages that are `pending`, `held` or `dispatching`, in `seq` order.
    pub open: Vec<OpenMessage>,
}

impl MessageCtlItem {
    /// A run that has had no message and no session.
    pub fn new(run_id: &str) -> Self {
        Self {
            run_id: run_id.to_string(),
            version: 0,
            admission: Admission::Open,
            closed_reason: None,
            closed_at: None,
            generation: None,
            epoch: None,
            epoch_no: 0,
            session_number: 0,
            retired: Vec::new(),
            seq: 0,
            bytes_total: 0,
            rate_minute: 0,
            rate_count: 0,
            open: Vec::new(),
        }
    }

    /// How many messages count toward [`MAX_OUTSTANDING_MESSAGES`].
    pub fn outstanding(&self) -> u32 {
        u32::try_from(self.open.iter().filter(|m| m.state.is_open()).count()).unwrap_or(u32::MAX)
    }

    /// The pending messages, in server order.
    pub fn pending(&self) -> impl Iterator<Item = &OpenMessage> {
        self.open
            .iter()
            .filter(|m| m.state == MessageState::Pending)
    }

    pub fn find(&self, message_id: &str) -> Option<&OpenMessage> {
        self.open.iter().find(|m| m.message_id == message_id)
    }
}

/// `MSGRATE#<principalArn>` / `<unix minute, 12 digits>`: how many messages an owner was admitted in one clock minute across runs. DynamoDB
/// removes it after `ttl`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OwnerRateItem {
    pub principal_arn: String,
    pub minute: u64,
    pub count: u32,
    pub ttl: u64,
}

impl Keyed for MessageItem {
    fn key(&self) -> Key {
        Key {
            pk: keys::run_pk(&self.run_id),
            sk: keys::message_sk(&self.message_id),
        }
    }
}

impl Keyed for MessageCtlItem {
    fn key(&self) -> Key {
        Key {
            pk: keys::run_pk(&self.run_id),
            sk: keys::MESSAGE_CTL.to_string(),
        }
    }
}

impl Keyed for OwnerRateItem {
    fn key(&self) -> Key {
        Key {
            pk: keys::owner_rate_pk(&self.principal_arn),
            sk: keys::owner_rate_sk(self.minute),
        }
    }
}
