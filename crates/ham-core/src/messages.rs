//! The client side of message control: what `ham` needs to send a message to a running Cline and to show what became of
//! it. The wire types are `harness_protocol::messages`; the calls are `ham::message_routes`. This module is pure: the id the client mints, the
//! confirmation of a held message, the words of a status and of a refusal, and the choice between an answer, a message and a follow-up.
//!
//! The words never say "not delivered" of a message whose delivery is unknown, and they never repeat a control character the server sent.

use crate::sanitize::display_line;
use harness_protocol::api::v1::NeedsInputBrief;
use harness_protocol::messages::{
    ConfirmHeld, MessageMode, MessageState, MessageView, RejectReason, SendMessageRequest,
};

pub use harness_protocol::messages::{
    Admission, MessageResponse, MessagesPage, SendMessageResponse, SendOutcome, validate_send,
};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A ULID: 48 bits of `ms` (the time) and 80 bits of `random`, 26 characters of Crockford base 32. A time past 48 bits is cut to 48 bits.
pub fn ulid_from(ms: u64, random: [u8; 10]) -> String {
    let mut v: u128 = u128::from(ms & ((1 << 48) - 1));
    for b in random {
        v = (v << 8) | u128::from(b);
    }
    let mut out = [b'0'; 26];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(v & 31) as usize];
        v >>= 5;
    }
    out.iter().map(|b| char::from(*b)).collect()
}

/// A message as the box sends it: the text goes as typed (no trimming, no wrapping). `message_id` is a ULID the client minted
/// ([`ulid_from`]); it is the idempotency key of the send.
pub fn plain(message_id: String, text: &str, mode: MessageMode) -> SendMessageRequest {
    SendMessageRequest {
        message_id,
        text: text.to_string(),
        mode,
        confirms: None,
    }
}

/// Why a message cannot be confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmError {
    NotConfirmable(MessageState),
}

/// A message the user can send again on purpose: one held after a session change, or one that may have reached Cline.
pub fn is_confirmable(m: &MessageView) -> bool {
    matches!(m.state, MessageState::Held | MessageState::DeliveryUnknown)
}

/// A message nobody has claimed.
pub fn is_withdrawable(m: &MessageView) -> bool {
    m.state.can_withdraw()
}

/// The confirmation of `original`: a new message with a new id that repeats its text and mode and names it and the session the user was
/// shown (`shown_epoch`, the current session of the run). The server refuses it when the message is no longer held or the session
/// changed again.
pub fn confirmation(
    new_id: String,
    original: &MessageView,
    shown_epoch: &str,
) -> Result<SendMessageRequest, ConfirmError> {
    if !is_confirmable(original) {
        return Err(ConfirmError::NotConfirmable(original.state));
    }
    Ok(SendMessageRequest {
        message_id: new_id,
        text: original.text.clone(),
        mode: original.mode,
        confirms: Some(ConfirmHeld {
            message_id: original.message_id.clone(),
            epoch: shown_epoch.to_string(),
        }),
    })
}

fn rejected_words(engine: Option<&str>, reason: Option<RejectReason>) -> String {
    let say = |t: &str| crate::engines::say_for(engine, t);
    match reason {
        Some(RejectReason::RunSealed) => "not sent: the run was ending",
        Some(RejectReason::RunCancelled) => "not sent: you stopped the run",
        Some(RejectReason::EngineBusy) => return say("not sent: {engine}'s input queue was full"),
        Some(RejectReason::NoMessageControl) => "not sent: this run cannot take messages",
        Some(RejectReason::ContinuationLimit) => {
            "not sent: the run reached its limit of turns after done"
        }
        Some(RejectReason::Limit) => "not sent: the run reached its message limit",
        None => "not sent",
    }
    .to_string()
}

/// What the pending row says of a message. `current_epoch_no` is the number of the run's current session (`MessagesPage::epoch_no`), for
/// the words of a held message. The words name the default engine; a run on another engine goes through [`status_line_for`].
pub fn status_line(m: &MessageView, current_epoch_no: u32) -> String {
    status_line_for(None, m, current_epoch_no)
}

/// [`status_line`] for a run on the engine `engine` names (an id of `crate::engines`; `None` and an id this build lacks name the default engine).
pub fn status_line_for(engine: Option<&str>, m: &MessageView, current_epoch_no: u32) -> String {
    let say = |t: &str| crate::engines::say_for(engine, t);
    match m.state {
        MessageState::Pending if m.mode == MessageMode::Now => say("waiting to interrupt {engine}"),
        MessageState::Pending => say("waiting for {engine} to finish its turn"),
        MessageState::Held => format!(
            "held: written to session {}, now session {}; confirm to send it to the new session",
            m.epoch_no, current_epoch_no
        ),
        MessageState::Dispatching => say("{engine} is taking it now"),
        MessageState::Delivered => match m.delivered_turn {
            Some(t) => format!("delivered in turn {t}"),
            None => "delivered".into(),
        },
        MessageState::Withdrawn => "withdrawn".into(),
        MessageState::Superseded => "replaced by a confirmed copy".into(),
        MessageState::Rejected => rejected_words(engine, m.reason),
        MessageState::Failed => match m.detail.as_deref().map(display_line) {
            Some(d) if !d.trim().is_empty() => format!("could not be sent: {d}"),
            _ => "could not be sent".into(),
        },
        MessageState::DeliveryUnknown => say(
            "may have reached {engine} (the worker was lost before it said); confirm to send it again",
        ),
    }
}

/// How the box takes a refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// What the box shows.
    pub text: String,
    /// The text goes back into the box: nothing was stored, and the user may edit it.
    pub restore_text: bool,
    /// The server answered: the send is settled. `false` for an answer that is not one (a gateway error, no answer): the message id is
    /// kept, the row stays as "sending", and the send is reconciled by reading the message with that id; the text is not restored as a
    /// fresh message.
    pub definitive: bool,
}

/// The words of an HTTP answer to a send (`status` 0 stands for no answer at all). `message` is the server's own text, used for the codes
/// this client has no name for.
pub fn explain_refusal(status: u16, code: &str, message: &str) -> Refusal {
    explain_refusal_for(None, status, code, message)
}

/// [`explain_refusal`] for a run on the engine `engine` names (`None` and an id this build lacks name the default engine).
pub fn explain_refusal_for(
    engine: Option<&str>,
    status: u16,
    code: &str,
    message: &str,
) -> Refusal {
    let say = |t: &str| crate::engines::say_for(engine, t);
    let definitive = |text: &str| Refusal {
        text: text.to_string(),
        restore_text: true,
        definitive: true,
    };
    if status == 0 || status >= 500 {
        return Refusal {
            text: "sending; no answer yet, the message is checked again by its id".into(),
            restore_text: false,
            definitive: false,
        };
    }
    match code {
        "no-message-control" => definitive(&say(
            "{engine} is not waiting for an answer; /cancel stops the run and keeps the work",
        )),
        "question-open" => definitive(&say(
            "{engine} is waiting for your answer to a question; answer it (Enter sends the text as the answer)",
        )),
        "run-finished" | "no-live-session" => definitive(
            "the run is finished; Enter prepares a follow-up run from its delivered result",
        ),
        "run-sealed" => definitive(
            "the run is ending and takes no more messages; its work is delivered, start a follow-up run",
        ),
        "too-many-pending" => definitive(&format!(
            "{} {}",
            harness_protocol::messages::MAX_OUTSTANDING_MESSAGES,
            say(
                "messages are waiting for {engine} already; wait for one to be delivered or withdraw one"
            )
        )),
        "rate-limited" => definitive("too many messages in the last minute; wait a moment"),
        "run-limit" => {
            definitive("this run has taken all the messages it can; start a follow-up run")
        }
        "epoch-changed" | "not-held" | "confirm-mismatch" => definitive(
            "the session changed again while you were looking; read the messages and confirm again",
        ),
        "not-found" if status == 404 => definitive("that run or message is no longer there"),
        _ => definitive(&display_line(message)),
    }
}

/// Where Enter goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendRoute {
    /// A question waits: the text is its answer (`POST /v1/runs/{runId}/input`, `continue_with_prompt`).
    Answer { request_id: String },
    /// Cline is working: the text is a message (`POST /v1/runs/{runId}/messages`).
    Message,
    /// The run is finished: the text prepares a follow-up run.
    FollowUp,
}

/// The route Enter takes for a run that is `active` (not complete or cleared) with `open_question` (the run list row's `needsInput`).
pub fn route_for(active: bool, open_question: Option<&NeedsInputBrief>) -> SendRoute {
    match (active, open_question) {
        (false, _) => SendRoute::FollowUp,
        (true, Some(q)) => SendRoute::Answer {
            request_id: q.request_id.clone(),
        },
        (true, None) => SendRoute::Message,
    }
}
