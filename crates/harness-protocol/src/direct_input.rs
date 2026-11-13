//! The user correction path of a direct run.
//!
//! When Cline fails or gives up, agentd raises a question (a `needs-input` event), keeps the Cline session and the work tree alive for a
//! bounded wait, and takes one answer from the user: a new prompt in the same session, a fresh session, another model, or finish. This
//! module holds the shared wire shapes: the constants, the closed enums, the event payloads, the summary members, the fold that turns
//! the event log into the question the screen shows, and the size bound that keeps the question inside one inline event.
//!
//! Everything here is re-exported from [`crate::direct`], so a caller writes `harness_protocol::direct::NeedsInputPayload`.
//!
//! Reading rule (as everywhere in `direct`): event payloads and `summary.json` are read leniently (an unknown member is ignored, a
//! writer adds a member only with a protocol minor bump); the request bodies ([`crate::api::v1::InputAnswer`]) are strict.
//! Reason, `why`, `how` and `trigger` are strings on the wire so a reader survives a word added later; the enums below give the
//! words their one spelling.

use crate::envelope::{Envelope, EventType, Payload};
use crate::goal::GoalStatus;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---- constants ------------------------------

/// `turn.progress` kind of the question (and of the non-blocking outage notice).
pub const PROGRESS_NEEDS_INPUT: &str = "needs-input";
/// `turn.progress` kind: the machine took the user's answer.
pub const PROGRESS_INPUT_RECEIVED: &str = "input-received";
/// `turn.progress` kind: the machine did not take an answer, with a reason.
pub const PROGRESS_INPUT_IGNORED: &str = "input-ignored";
/// `turn.progress` kind: the question is closed.
pub const PROGRESS_INPUT_CLOSED: &str = "input-closed";
/// `turn.progress` kind: a fresh Cline session started in the same work tree.
pub const PROGRESS_NEW_SESSION: &str = "new-session";

/// The longest user text (an answer to a question, a message to a running Cline), in UTF-8 bytes. The text is stored in one
/// DynamoDB item (limit 400 KB) beside a few small attributes, so 256 KiB leaves more than 130 KB of room; it is not in any event, so there is no
/// other bound below the request. There is no limit in characters.
pub const MAX_INPUT_TEXT_BYTES: usize = 256 * 1024;
/// The largest `inputWaitS` a request may carry: the longest run. agentd cuts every window to the time its session has left less
/// [`INPUT_MARGIN_S`], so a value above that is not an error and has no effect beyond it.
pub const MAX_INPUT_WAIT_S: u32 = 2_592_000;
// The two numbers are one: the wait may be as long as the run.
const _: () = assert!(MAX_INPUT_WAIT_S == crate::direct::MAX_MAX_RUN_S);
/// What an interactive `ham` asks for (decision A: 30 minutes). The server default when a client sends nothing is 0.
pub const DEFAULT_CLIENT_INPUT_WAIT_S: u32 = 1_800;
/// The window is clamped to the remaining life of the run less this margin.
pub const INPUT_MARGIN_S: u64 = 900;
/// No question opens when less than this would be left after the clamp.
pub const INPUT_MIN_WINDOW_S: u64 = 300;
/// Questions per run (decision G; the memo said 2).
pub const MAX_INPUT_REQUESTS: u32 = 3;
/// Automatic fresh sessions per run (memo R3).
pub const MAX_AUTO_SESSIONS: u32 = 1;
/// Fresh sessions the user may request per run, counted apart from the automatic one.
pub const MAX_USER_SESSIONS: u32 = 3;
/// Every route down for this long raises the non-blocking outage notice (memo R5).
pub const OUTAGE_NOTICE_AFTER_S: u64 = 1_800;
/// The ladder stops after this many prompts in a row without a new tool call or changed file (memo phase 3).
pub const MAX_FRUITLESS_PROMPTS: u32 = 12;
/// The notes of a fresh session besides the user's task: 200,000 bytes, about 50K tokens, 5 percent of a window of a million tokens. Whole sections are
/// dropped from the end, never cut. The task itself is whole in the digest and outside this budget.
pub const DIGEST_BUDGET_BYTES: usize = 200_000;
/// The part of the digest that is also written into the prompt of a fresh session.
pub const INLINE_SUMMARY_BYTES: usize = 24_000;
/// The serialized `needs-input` payload is bounded to this many bytes: the inline event limit less 296 bytes for the rest of the event.
pub const NEEDS_INPUT_PAYLOAD_BUDGET: usize = crate::envelope::MAX_INLINE_PAYLOAD_BYTES - 296;
/// control-api accepts an answer this many seconds after `expiresAt`.
pub const INPUT_ADMISSION_GRACE_S: i64 = 30;
/// The stop digest written at every stop is at most this large.
pub const MAX_STOP_DIGEST_BYTES: usize = 256 * 1024;

/// The most characters of the one-line `text` of a `needs-input` event.
pub const MAX_NEEDS_INPUT_TEXT_CHARS: usize = 160;
/// The most characters of its `title`.
pub const MAX_NEEDS_INPUT_TITLE_CHARS: usize = 120;
/// The most bytes of `lastMessage` (the tail of Cline's last message).
pub const MAX_NEEDS_INPUT_LAST_MESSAGE_BYTES: usize = 1_200;
/// The most paths in `files`.
pub const MAX_NEEDS_INPUT_FILES: usize = 12;
/// The most characters of one path in `files`.
pub const MAX_NEEDS_INPUT_FILE_CHARS: usize = 120;
/// The most rows in `models`.
pub const MAX_NEEDS_INPUT_MODELS: usize = 8;
/// The most characters of `evidence.lastError`.
pub const MAX_NEEDS_INPUT_ERROR_CHARS: usize = 200;
/// The most characters of `question` (the analyst's human question, later).
pub const MAX_NEEDS_INPUT_QUESTION_CHARS: usize = 300;
/// The most interventions and fresh sessions `summary.json` lists.
pub const MAX_DIRECT_INTERVENTIONS: usize = 32;

// ---- enums -----------------------------------------------------------------------------------------------------------

/// What the user (or, later, the analyzer) asks the machine to do. Wire words: `continue_with_prompt`, `new_session_with_prompt`,
/// `try_model`, `finish`. One control carries all four so a person and the analyzer share one path.
// Plain comments on the variants: a doc comment would turn the schema's `enum` into a `oneOf` of constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructAction {
    // a new prompt in the same Cline session (the text is required)
    ContinueWithPrompt,
    // a fresh Cline session in the same work tree (the text is optional)
    NewSessionWithPrompt,
    // another route of this run, then a prompt (the text is optional)
    TryModel,
    // end the run with what exists
    Finish,
}

impl InstructAction {
    /// Every action, in declaration order.
    pub const ALL: [InstructAction; 4] = [
        Self::ContinueWithPrompt,
        Self::NewSessionWithPrompt,
        Self::TryModel,
        Self::Finish,
    ];

    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContinueWithPrompt => "continue_with_prompt",
            Self::NewSessionWithPrompt => "new_session_with_prompt",
            Self::TryModel => "try_model",
            Self::Finish => "finish",
        }
    }

    /// The action for a wire word.
    pub fn from_wire(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.as_str() == word)
    }
}

/// Who answered. control-api always writes `user`; `analyzer` is written only inside agentd.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructOrigin {
    // a person, through control-api
    User,
    // the in-process analyzer, later
    Analyzer,
}

impl InstructOrigin {
    /// The serde default of a control stored without an origin.
    pub fn user() -> Self {
        Self::User
    }

    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Analyzer => "analyzer",
        }
    }
}

/// Why a question was raised (or, for `provider_outage`, a notice).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NeedsInputReason {
    // a second Cline stop of its own (loop guard or mistake limit), after the one resume
    EngineStop,
    // end_turn with a message and no changed file
    NoChanges,
    // end_turn with a blank message
    NoMessage,
    // stopped_max_tokens
    OutputLimit,
    // stopped_max_requests
    RequestLimit,
    // stopped_refusal, or a provider content filter
    Refused,
    // the provider rejected the saved conversation after the automatic fresh session
    ReplayRejected,
    // the conversation no longer fits the context window after the automatic fresh session
    ContextFull,
    // a provider error the ladder could not absorb
    ProviderError,
    // the ladder stopped on its cap of fruitless prompts
    CapHit,
    // the engine process died after the automatic restart
    EngineDied,
    // a second silence
    Silence,
    // an unknown stop reason
    OtherStop,
    // NOT blocking: every route down for 30 minutes
    ProviderOutage,
}

impl NeedsInputReason {
    /// Every reason, in declaration order.
    pub const ALL: [NeedsInputReason; 14] = [
        Self::EngineStop,
        Self::NoChanges,
        Self::NoMessage,
        Self::OutputLimit,
        Self::RequestLimit,
        Self::Refused,
        Self::ReplayRejected,
        Self::ContextFull,
        Self::ProviderError,
        Self::CapHit,
        Self::EngineDied,
        Self::Silence,
        Self::OtherStop,
        Self::ProviderOutage,
    ];

    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EngineStop => "engine_stop",
            Self::NoChanges => "no_changes",
            Self::NoMessage => "no_message",
            Self::OutputLimit => "output_limit",
            Self::RequestLimit => "request_limit",
            Self::Refused => "refused",
            Self::ReplayRejected => "replay_rejected",
            Self::ContextFull => "context_full",
            Self::ProviderError => "provider_error",
            Self::CapHit => "cap_hit",
            Self::EngineDied => "engine_died",
            Self::Silence => "silence",
            Self::OtherStop => "other_stop",
            Self::ProviderOutage => "provider_outage",
        }
    }

    /// The reason for a wire word.
    pub fn from_wire(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == word)
    }

    /// True when the machine waits for an answer; false only for the outage notice.
    pub fn is_blocking(self) -> bool {
        self != Self::ProviderOutage
    }
}

/// Why the machine did not take an answer (`input-ignored.why`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputIgnoreWhy {
    WindowClosed,
    UnknownRequest,
    AlreadyAnswered,
    SessionDead,
    BadAction,
    BadModel,
    Limit,
}

impl InputIgnoreWhy {
    /// Every word, in declaration order.
    pub const ALL: [InputIgnoreWhy; 7] = [
        Self::WindowClosed,
        Self::UnknownRequest,
        Self::AlreadyAnswered,
        Self::SessionDead,
        Self::BadAction,
        Self::BadModel,
        Self::Limit,
    ];

    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WindowClosed => "window-closed",
            Self::UnknownRequest => "unknown-request",
            Self::AlreadyAnswered => "already-answered",
            Self::SessionDead => "session-dead",
            Self::BadAction => "bad-action",
            Self::BadModel => "bad-model",
            Self::Limit => "limit",
        }
    }
}

/// How a question closed (`input-closed.how`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputClosedHow {
    Expired,
    Cancelled,
    Deadline,
    Finish,
    Superseded,
    Recovered,
}

impl InputClosedHow {
    /// Every word, in declaration order.
    pub const ALL: [InputClosedHow; 6] = [
        Self::Expired,
        Self::Cancelled,
        Self::Deadline,
        Self::Finish,
        Self::Superseded,
        Self::Recovered,
    ];

    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
            Self::Deadline => "deadline",
            Self::Finish => "finish",
            Self::Superseded => "superseded",
            Self::Recovered => "recovered",
        }
    }
}

/// Who started a fresh session (`new-session.trigger`, `freshSessions[].trigger`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NewSessionTrigger {
    Harness,
    User,
    Analyzer,
}

impl NewSessionTrigger {
    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::User => "user",
            Self::Analyzer => "analyzer",
        }
    }
}

// ---- event payloads --------------------------------------------------------------

/// One row of the `models` list of a question: a route of this run that `try_model` may name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NeedsInputModel {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context_tokens: u32,
}

/// The numbers a card shows. Counters of the run so far.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NeedsInputEvidence {
    pub attempts: u32,
    pub tool_calls: u64,
    pub files_changed: u32,
    /// The shape counter (identical tool calls in a row at the end of the stage), when the collector has it.
    pub identical_calls: Option<u32>,
    /// The shape counter (failed tool calls in a row), when the collector has it.
    pub failed_turns: Option<u32>,
    /// Scrubbed, at most [`MAX_NEEDS_INPUT_ERROR_CHARS`] characters.
    pub last_error: Option<String>,
    pub run_s: u64,
    pub sessions: u32,
}

/// The `payload.value` of a `turn.progress` event of kind `needs-input`; the envelope's `eventId` IS [`Self::request_id`]. A client that
/// predates this event still shows [`Self::text`] in the agent's last column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NeedsInputPayload {
    /// [`crate::direct::DIRECT_TURN`].
    #[serde(default = "default_turn")]
    pub turn: u32,
    /// [`PROGRESS_NEEDS_INPUT`].
    pub kind: String,
    /// One line, at most [`MAX_NEEDS_INPUT_TEXT_CHARS`] characters.
    #[serde(default)]
    pub text: String,
    /// Equal to the envelope's `eventId`.
    pub request_id: String,
    /// 1 for the first request of the run.
    #[serde(default = "default_turn")]
    pub n: u32,
    /// A [`NeedsInputReason`] wire word; a string so a reader survives a new code.
    pub reason: String,
    /// Plain English, at most [`MAX_NEEDS_INPUT_TITLE_CHARS`] characters.
    #[serde(default)]
    pub title: String,
    /// The wire word of the held stop (`DirectOutcome`).
    #[serde(default)]
    pub stop_outcome: Option<String>,
    #[serde(default)]
    pub evidence: NeedsInputEvidence,
    /// The tail of Cline's last message, at most [`MAX_NEEDS_INPUT_LAST_MESSAGE_BYTES`] bytes.
    #[serde(default)]
    pub last_message: Option<String>,
    /// The first [`MAX_NEEDS_INPUT_FILES`] changed paths.
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub files_total: u32,
    /// False only for the `provider_outage` notice: no window, no options.
    #[serde(default = "default_true")]
    pub blocking: bool,
    #[serde(default)]
    pub options: Vec<InstructAction>,
    /// The routes `try_model` may name (at most [`MAX_NEEDS_INPUT_MODELS`]); empty when it is not offered.
    #[serde(default)]
    pub models: Vec<NeedsInputModel>,
    /// The analyzer's human question, later; absent in v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// Null in v1 for every reason (the member stays on the wire for the analyzer).
    #[serde(default)]
    pub recommended: Option<InstructAction>,
    #[serde(default)]
    pub can_continue: bool,
    /// The window actually opened (0 for a notice).
    #[serde(default)]
    pub wait_s: u32,
    /// RFC 3339 UTC, agentd's clock; null for a notice.
    #[serde(default)]
    pub expires_at: Option<String>,
}

fn default_turn() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

/// A `String` cut to its first `max` characters.
fn head_chars_owned(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The last `max` bytes of `s`, starting on a character boundary.
pub fn tail_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

impl NeedsInputPayload {
    /// The payload with every member cut to its stated limit (see the `MAX_NEEDS_INPUT_*` constants). Never changes `kind`, `requestId`,
    /// `n`, `reason`, `blocking`, `options`, `canContinue`, `waitS` or `expiresAt`.
    pub fn clamp(mut self) -> Self {
        self.text = head_chars_owned(&self.text, MAX_NEEDS_INPUT_TEXT_CHARS);
        self.title = head_chars_owned(&self.title, MAX_NEEDS_INPUT_TITLE_CHARS);
        if let Some(m) = self.last_message.take() {
            self.last_message =
                Some(tail_bytes(&m, MAX_NEEDS_INPUT_LAST_MESSAGE_BYTES).to_string());
        }
        self.files.truncate(MAX_NEEDS_INPUT_FILES);
        for f in &mut self.files {
            if f.chars().count() > MAX_NEEDS_INPUT_FILE_CHARS {
                *f = head_chars_owned(f, MAX_NEEDS_INPUT_FILE_CHARS);
            }
        }
        self.models.truncate(MAX_NEEDS_INPUT_MODELS);
        if let Some(e) = self.evidence.last_error.take() {
            self.evidence.last_error = Some(head_chars_owned(&e, MAX_NEEDS_INPUT_ERROR_CHARS));
        }
        if let Some(q) = self.question.take() {
            self.question = Some(head_chars_owned(&q, MAX_NEEDS_INPUT_QUESTION_CHARS));
        }
        self
    }

    /// The compact JSON size of the payload, the figure the inline event limit counts.
    pub fn json_len(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |v| v.len())
    }

    /// The payload cut to its limits ([`Self::clamp`]) and then shrunk until its compact JSON is at most `budget` bytes
    /// ([`NEEDS_INPUT_PAYLOAD_BUDGET`] for an event), in this order: `lastMessage` (halved by bytes at a character boundary, keeping its
    /// tail, then dropped), `files` (halved, then dropped; `filesTotal` stays), `evidence.lastError`, `question`, `title` to 80
    /// characters, and last the model names (the ids stay). `kind`, `text`, `requestId`, `n`, `reason`, `blocking`, `options`,
    /// `canContinue`, `waitS`, `expiresAt` and the model ids are never dropped or changed.
    pub fn bounded(self, budget: usize) -> Self {
        let mut p = self.clamp();
        // 1. lastMessage
        while p.json_len() > budget {
            match p.last_message.take() {
                Some(m) if m.len() > 1 => {
                    p.last_message = Some(tail_bytes(&m, m.len() / 2).to_string());
                }
                _ => break,
            }
        }
        if p.json_len() > budget {
            p.last_message = None;
        }
        // 2. files
        while p.json_len() > budget && !p.files.is_empty() {
            let keep = p.files.len() / 2;
            p.files.truncate(keep);
        }
        // 3. lastError, then the analyst's question
        if p.json_len() > budget {
            p.evidence.last_error = None;
        }
        if p.json_len() > budget {
            p.question = None;
        }
        // 4. title
        if p.json_len() > budget {
            p.title = head_chars_owned(&p.title, 80);
        }
        // 5. model names last
        if p.json_len() > budget {
            for m in &mut p.models {
                m.name = String::new();
            }
        }
        p
    }

    /// The payload as the JSON value of an event.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// `input-received`: the machine took the answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InputReceivedPayload {
    #[serde(default = "default_turn")]
    pub turn: u32,
    /// [`PROGRESS_INPUT_RECEIVED`].
    pub kind: String,
    #[serde(default)]
    pub text: String,
    pub request_id: String,
    pub action: InstructAction,
    /// `try_model` only: the route id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "InstructOrigin::user")]
    pub origin: InstructOrigin,
    /// The length of the user's text in bytes; the text itself is never in an event.
    #[serde(default)]
    pub text_bytes: u64,
    /// Seconds from the question to the answer.
    #[serde(default)]
    pub waited_s: u64,
}

/// `input-ignored`: the machine did not take an answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InputIgnoredPayload {
    #[serde(default = "default_turn")]
    pub turn: u32,
    /// [`PROGRESS_INPUT_IGNORED`].
    pub kind: String,
    #[serde(default)]
    pub text: String,
    pub request_id: String,
    /// An [`InputIgnoreWhy`] wire word.
    pub why: String,
}

/// `input-closed`: the question is closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InputClosedPayload {
    #[serde(default = "default_turn")]
    pub turn: u32,
    /// [`PROGRESS_INPUT_CLOSED`].
    pub kind: String,
    #[serde(default)]
    pub text: String,
    pub request_id: String,
    /// An [`InputClosedHow`] wire word.
    pub how: String,
    #[serde(default)]
    pub waited_s: u64,
}

/// `new-session`: a fresh Cline session started in the same work tree ("restarted from the work tree").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionPayload {
    #[serde(default = "default_turn")]
    pub turn: u32,
    /// [`PROGRESS_NEW_SESSION`].
    pub kind: String,
    #[serde(default)]
    pub text: String,
    /// Null for the automatic (harness) session.
    pub request_id: Option<String>,
    /// 2 for the first fresh session.
    pub session: u32,
    /// A [`NewSessionTrigger`] wire word.
    pub trigger: String,
    /// A [`NeedsInputReason`] wire word.
    pub reason: String,
    /// True when the engine process was stopped and started again.
    pub restarted_process: bool,
    pub digest_bytes: u64,
    pub prompt_bytes: u64,
}

// ---- summary.json members -----------------------------------------------------------------

/// One question and what became of it (`DirectSummary.interventions`). Never carries the user's text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectIntervention {
    pub n: u32,
    pub request_id: String,
    pub reason: String,
    /// RFC 3339.
    pub opened_at: String,
    /// RFC 3339.
    pub closed_at: String,
    pub waited_s: u64,
    /// `answered`, `expired`, `cancelled`, `deadline`, `finish` or `superseded`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<InstructAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<InstructOrigin>,
    /// `try_model` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_bytes: Option<u64>,
    /// The `n` of the next attempt this answer started, or of the last one for a close.
    pub at_attempt: u32,
}

/// One fresh session (`DirectSummary.freshSessions`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectFreshSession {
    /// 2 for the first fresh session.
    pub n: u32,
    /// `harness`, `user` or `analyzer`.
    pub trigger: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// RFC 3339.
    pub started_at: String,
    pub restarted_process: bool,
    pub digest_bytes: u64,
    pub digest_sha256: String,
    pub at_attempt: u32,
}

// ---- the fold ----------------------------------------------------------------------------------

/// The question the screen shows: the `needs-input` event that is still open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRequest {
    pub request_id: String,
    /// The `ts` of the `needs-input` event.
    pub opened_ts: String,
    pub payload: NeedsInputPayload,
}

/// A question that is over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosedRequest {
    pub request_id: String,
    pub n: u32,
    pub reason: String,
    /// `answered:<action>`, `expired`, `cancelled`, `deadline`, `finish`, `superseded`, `recovered` or `ended`.
    pub how: String,
    /// The `ts` of the event that closed it.
    pub ts: String,
}

/// What the event log says about the correction path of a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputFold {
    /// A blocking question or a non-blocking notice that nothing has closed.
    pub open: Option<OpenRequest>,
    /// The last [`MAX_FOLD_CLOSED`] closed requests, oldest first.
    pub closed: Vec<ClosedRequest>,
    /// 1 plus the `new-session` events (never below the highest `session` a payload names).
    pub sessions: u32,
    /// The last `new-session` payload.
    pub last_session: Option<NewSessionPayload>,
}

impl Default for InputFold {
    fn default() -> Self {
        Self {
            open: None,
            closed: Vec::new(),
            sessions: 1,
            last_session: None,
        }
    }
}

/// The most closed requests [`InputFold::closed`] keeps.
pub const MAX_FOLD_CLOSED: usize = 8;

/// The agent id the fold reads (the one agent of a direct run).
const FOLD_AGENT: &str = crate::direct::DIRECT_AGENT_ID;

fn inline_value(e: &Envelope) -> Option<&serde_json::Value> {
    match &e.payload {
        Payload::Inline { value } => Some(value),
        Payload::ClaimCheck { .. } => None,
    }
}

fn progress_kind(e: &Envelope) -> Option<&str> {
    inline_value(e)?.get("kind")?.as_str()
}

fn ends_the_run(e: &Envelope) -> bool {
    match e.event_type {
        EventType::RunCompleted | EventType::RunAborted => true,
        EventType::GoalStatus => {
            e.task_id.is_none()
                && inline_value(e)
                    .and_then(|v| v.get("status"))
                    .and_then(|s| serde_json::from_value::<GoalStatus>(s.clone()).ok())
                    .is_some_and(GoalStatus::is_terminal)
        }
        _ => false,
    }
}

fn decode<T: serde::de::DeserializeOwned>(e: &Envelope) -> Option<T> {
    serde_json::from_value(inline_value(e)?.clone()).ok()
}

/// Folds the events of a run into the correction path's state.
///
/// Reads `turn.progress` events of the `direct` agent whose payload kind is `needs-input`, `input-received`, `input-ignored`,
/// `input-closed` or `new-session`, and the events that end the run. The events are put in ULID order first and a repeated `eventId`
/// counts once, so a page boundary or a repeated read does not change the result.
///
/// - `needs-input` opens a request; an open request is closed as `superseded` first. A request id already seen (open or closed) is
///   a duplicate and changes nothing.
/// - `input-received` for the open id closes it as `answered:<action>`, except `finish`: `input-closed how=finish` follows.
/// - `input-closed` for the open id closes it with its `how`.
/// - `input-ignored` changes nothing.
/// - `new-session` adds a session.
/// - `run.completed`, `run.aborted` and a terminal `goal.status` of the run close an open request as `ended`.
///
/// An event that does not decode is skipped.
pub fn fold_input<'a>(events: impl IntoIterator<Item = &'a Envelope>) -> InputFold {
    let mut evs: Vec<&Envelope> = events
        .into_iter()
        .filter(|e| {
            ends_the_run(e)
                || (e.event_type == EventType::TurnProgress
                    && e.agent_id.as_deref() == Some(FOLD_AGENT)
                    && matches!(
                        progress_kind(e),
                        Some(
                            PROGRESS_NEEDS_INPUT
                                | PROGRESS_INPUT_RECEIVED
                                | PROGRESS_INPUT_IGNORED
                                | PROGRESS_INPUT_CLOSED
                                | PROGRESS_NEW_SESSION
                        )
                    ))
        })
        .collect();
    evs.sort_by(|a, b| a.event_id.cmp(&b.event_id));
    evs.dedup_by(|a, b| a.event_id == b.event_id);

    let mut fold = InputFold::default();
    let mut seen: Vec<String> = Vec::new();
    let mut new_sessions: u32 = 0;
    let mut named_session: u32 = 1;

    fn close(fold: &mut InputFold, how: String, ts: &str) {
        if let Some(open) = fold.open.take() {
            fold.closed.push(ClosedRequest {
                request_id: open.request_id,
                n: open.payload.n,
                reason: open.payload.reason,
                how,
                ts: ts.to_string(),
            });
            if fold.closed.len() > MAX_FOLD_CLOSED {
                fold.closed.remove(0);
            }
        }
    }

    for e in evs {
        if ends_the_run(e) {
            close(&mut fold, "ended".into(), &e.ts);
            continue;
        }
        match progress_kind(e) {
            Some(PROGRESS_NEEDS_INPUT) => {
                let Some(p) = decode::<NeedsInputPayload>(e) else {
                    continue;
                };
                if p.request_id.is_empty() || seen.contains(&p.request_id) {
                    continue;
                }
                close(&mut fold, "superseded".into(), &e.ts);
                seen.push(p.request_id.clone());
                fold.open = Some(OpenRequest {
                    request_id: p.request_id.clone(),
                    opened_ts: e.ts.clone(),
                    payload: p,
                });
            }
            Some(PROGRESS_INPUT_RECEIVED) => {
                let Some(p) = decode::<InputReceivedPayload>(e) else {
                    continue;
                };
                let open_id = fold.open.as_ref().map(|o| o.request_id.as_str());
                if open_id == Some(p.request_id.as_str()) && p.action != InstructAction::Finish {
                    close(&mut fold, format!("answered:{}", p.action.as_str()), &e.ts);
                }
            }
            Some(PROGRESS_INPUT_CLOSED) => {
                let Some(p) = decode::<InputClosedPayload>(e) else {
                    continue;
                };
                let open_id = fold.open.as_ref().map(|o| o.request_id.as_str());
                if open_id == Some(p.request_id.as_str()) {
                    close(&mut fold, p.how, &e.ts);
                }
            }
            Some(PROGRESS_NEW_SESSION) => {
                let Some(p) = decode::<NewSessionPayload>(e) else {
                    continue;
                };
                new_sessions += 1;
                named_session = named_session.max(p.session);
                fold.last_session = Some(p);
            }
            _ => {}
        }
    }
    fold.sessions = (1 + new_sessions).max(named_session);
    fold
}
