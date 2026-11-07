//! The send-message experience. A child module of
//! `app`. What Enter does with the text in the pinned box follows the state of the run, and the box's title says which:
//!
//! | The run | Enter |
//! | --- | --- |
//! | has an open question | answers it through `POST /v1/runs/{id}/input` (the existing route, unchanged) |
//! | works, on an image with message control | sends a message that is delivered after the turn (`POST /v1/runs/{id}/messages`) |
//! | works, on an image without it | keeps the text and says that the agent is not waiting; `/cancel` stops the run |
//! | is finished and delivered | prepares a follow-up in the box (the digest of `ham_core::followup`), then asks for a confirmation |
//! | ended blocked | offers to start it again with the request it was started with, keeps the text in the box, and starts the new run on the second Enter |
//!
//! `/now <text>` asks for a second Enter and then sends the message as an interrupt. A **pending row** above the box shows each message
//! with the words of its state (`ham_core::messages::status_line`); `w` withdraws one nobody has claimed; `x` confirms a message held
//! after a session change (or one that may have reached the agent) with a **new** message that names it and the session the user was shown.
//! A refusal the server states gives the text back; a send with no answer keeps its message id, shows "sending" and is asked again by
//! that id (the id is the idempotency key, and the answer to a repeat is the message as the server holds it) for as long as the service is
//! silent, with longer pauses; the text is not put back as a fresh message, and the row is never called "not delivered". An answer to an
//! open question typed in the box is a sent message too: a pending row while it is on its way, and the user's own block in the conversation
//! once the service has it. When two messages are held, one is **chosen** at a time (`h` moves the choice) and `x` and `w` act on that
//! one.
//!
//! Enter is bound to what the box shows: a change of the run's state, question or session less than 300 ms before the key blocks it (the
//! title flashes "changed just now"), and a changed state never retargets the text. The server owns every state; the client shows the
//! list it last read and the sends it has on their way.

use super::*;
use crate::message_routes::mint_message_id;
use ham_core::chat::{Block, ChatModel, Origin, Role};
use ham_core::engines;
use ham_core::followup::{self, ChangedFiles};
use ham_core::head::{Changes, DeliveredHead};
use ham_core::messages::{
    self, MessagesPage, explain_refusal, is_confirmable, is_withdrawable, status_line,
};
use ham_core::retry;
use ham_core::stored::UserItem;
use harness_protocol::api::v1::InputAccepted;
use harness_protocol::messages::{MessageMode, MessageState, MessageView, SendMessageRequest};

use crate::tui::chat::{PendingRow, PendingStatus};
use crate::tui::ext::{Done, Job, SendFailure};

/// Asks of an uncertain send made at the first pace; after them the pause doubles.
const FAST_TRIES: u8 = 5;
/// Seconds before an uncertain send is asked again, at the first pace.
const RETRY_AFTER: Duration = Duration::from_secs(4);
/// The longest pause between two asks of an uncertain send.
const RETRY_MAX: Duration = Duration::from_secs(60);
/// How long the title flashes after Enter was blocked by a change of context.
const FLASH: Duration = Duration::from_secs(2);
/// Seconds between reads of the message list while a message is on its way.
const LIST_EVERY: Duration = Duration::from_secs(2);
/// How long a settled message stays on the pending rows.
const SHOW_SETTLED: Duration = Duration::from_secs(20);
/// Pending rows the box shows.
const MAX_ROWS: usize = 3;
/// Messages a run holds open at once.
const MAX_OPEN: usize = harness_protocol::messages::MAX_OUTSTANDING_MESSAGES as usize;
/// A confirming key within this long of the preview is ignored.
const CONFIRM_DELAY_MS: u64 = 300;
/// A change of the context this long before Enter blocks it.
const CONTEXT_GUARD_MS: u64 = 300;

pub(crate) const NOT_WAITING_TEXT: &str = engines::NOT_WAITING;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// The request is on its way.
    Sending,
    /// It got no answer; it is asked again by its id for as long as the service is silent.
    Uncertain,
}

/// The pause before an uncertain send is asked again after `tries` asks: the first pace for [`FAST_TRIES`] asks, then twice as long each
/// time, never over [`RETRY_MAX`].
fn retry_pause(tries: u8) -> Duration {
    let slow = u32::from(tries.saturating_sub(FAST_TRIES)) + 1;
    if tries < FAST_TRIES {
        RETRY_AFTER
    } else {
        RETRY_AFTER
            .saturating_mul(1u32.checked_shl(slow).unwrap_or(u32::MAX))
            .min(RETRY_MAX)
    }
}

/// Where an answer typed in the box stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnswerPhase {
    /// The request is on its way.
    Sending,
    /// The service has it (`queued`); the run's events show when the agent takes it.
    Taken,
}

/// An answer to an open question that was typed in the box (the input route; the question's request id is its key).
#[derive(Clone, Debug)]
struct AnswerRow {
    request_id: String,
    text: String,
    /// When the service took it (RFC 3339 UTC, from the app's clock; empty when the clock is not set), to place it among the stored records.
    at: String,
    phase: AnswerPhase,
}

/// A send of this process that the server's list does not hold yet.
#[derive(Clone, Debug)]
struct Local {
    request: SendMessageRequest,
    phase: Phase,
    tries: u8,
    retry_at: Duration,
    inflight: bool,
}

/// What Enter does now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Answer,
    Next,
    FollowUp,
    /// The run ended blocked: nothing can answer it or take a message, and the offer is to start it again.
    Retry,
    Unavailable(String),
}

/// A preview that waits for its second Enter.
#[derive(Clone, Debug)]
enum Confirm {
    /// `/now <text>`.
    Now { text: String, shown_ms: u64 },
    /// `x` on a held message or one that may have reached the agent.
    Held {
        message: Box<MessageView>,
        epoch: String,
        shown_ms: u64,
    },
    /// The follow-up prepared in the box with its confirmation on the screen: the next Enter looks for a follow-up that is already going
    /// (unless `checked`: the person saw the offer, or the look failed and they chose to go on) and then starts it.
    FollowUp { shown_ms: u64, checked: bool },
    /// A follow-up from the same source is already going: Enter opens it, anything else keeps the text and brings the start back.
    FollowUpRunning { existing: String, shown_ms: u64 },
    /// The offer to start a blocked run again: Enter, or `r` in browse mode, 300 ms or more later starts it; the text in the box is kept.
    Retry { shown_ms: u64 },
}

/// The look for the delivered head of a finished run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum HeadLookup {
    /// The timer asks as soon as the run is finished and delivered.
    #[default]
    NotAsked,
    Asking {
        tries: u8,
    },
    Known,
    /// No answer yet. `tries` asks have been made; a refusal that will not change (`definitive`: the server's code and words) is not asked again.
    Failed {
        tries: u8,
        retry_at: Duration,
        definitive: Option<(String, String)>,
    },
}

/// Times the head route is asked when it gets no answer, and the waits between them.
const HEAD_TRIES: u8 = 4;
const HEAD_RETRY_AFTER: [Duration; 3] = [
    Duration::from_secs(3),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

#[derive(Debug, Default)]
pub(crate) struct SendState {
    run_id: String,
    page: Option<MessagesPage>,
    local: Vec<Local>,
    /// When a settled message was first on the rows (the app's clock).
    settled_seen: HashMap<String, Duration>,
    /// The server said this run takes no messages.
    no_control: bool,
    /// What Enter did at the last look: the plan, the open question and the session.
    seen: Option<(Plan, Option<String>, Option<u32>)>,
    changed_at: Option<Instant>,
    confirm: Option<Confirm>,
    list_inflight: bool,
    next_list: Duration,
    /// The delivered head of the finished run, when known (the follow-up's "from commit"): the full commit id of the head route's answer.
    head: Option<String>,
    /// The engine's last message once the person pressed the key that adds it to the draft (it is then in the box, fenced).
    final_text: Option<String>,
    /// How the head route is doing: asked by the timer once a finished run is on screen.
    lookup: HeadLookup,
    /// The checked answer of the head route: the commit, the engine's last message and the files the delivery changed.
    delivered: Option<DeliveredHead>,
    /// The random word that fences the engine's own text in a follow-up draft (made when the draft is prepared).
    fence: String,
    /// The look for a follow-up that is already going is out.
    looking: bool,
    /// Answers typed in the box, newest last; one per question.
    answers: Vec<AnswerRow>,
    /// The request id of the answer from the box that is on its way (the answer to `Msg::Answered` names no text).
    answering: Option<String>,
    /// The held message the person chose (`h`), when two or more are held: `x` and `w` act on it.
    choice: Option<String>,
    /// The title flashes until this time of the app's clock.
    flash_until: Option<Duration>,
    /// The settings hash of the engine file as it is now, asked when a follow-up is prepared (`Job::EngineSettings`): `None` until the answer
    /// comes, `Some(None)` for a file that sets no key.
    engine_now: Option<Option<String>>,
    /// The retry of this run is on its way: a second Enter or `r` waits for the answer.
    retrying: bool,
}

fn open_state(m: &MessageView) -> bool {
    m.state.is_open()
}

impl App {
    /// The run is open: its messages are read.
    pub(crate) fn send_open(&mut self, run_id: &str) -> Vec<Effect> {
        self.send = SendState {
            run_id: run_id.to_string(),
            list_inflight: true,
            ..SendState::default()
        };
        vec![Effect::Ext(Job::Messages {
            run_id: run_id.to_string(),
        })]
    }

    /// The delivered head the follow-up header names. The head route fills it; this setter lets a test stand in for the route.
    #[doc(hidden)]
    pub fn set_delivered_head(&mut self, head: Option<String>) {
        self.send.head = head;
    }

    // ---- what Enter does ----------------------------------------------------------------------------------------------------

    /// The plan for the focused run, from its state.
    pub(crate) fn send_plan(&self) -> Plan {
        let Some(v) = self.cached.as_ref() else {
            return Plan::Unavailable("no run is open".into());
        };
        // A blocked run has ended without a session: it takes no answer, no message and no cancel. The offer is the retry.
        if v.status == Some(harness_protocol::goal::GoalStatus::Blocked) {
            return Plan::Retry;
        }
        if self.question().is_some() {
            return Plan::Answer;
        }
        if v.status.is_some_and(|s| s.is_terminal()) {
            return if v.zip.is_some() || v.result.is_some() {
                Plan::FollowUp
            } else {
                Plan::Unavailable(
                    "this run delivered nothing, so there is nothing to follow up from".into(),
                )
            };
        }
        if !matches!(
            v.status,
            Some(harness_protocol::goal::GoalStatus::Active) | None
        ) {
            return Plan::Unavailable(NOT_WAITING_TEXT.into());
        }
        if !self.caps.offers(engines::NEXT_CONTROL) || self.send.no_control {
            return Plan::Unavailable(NOT_WAITING_TEXT.into());
        }
        if self
            .send
            .page
            .as_ref()
            .is_some_and(|p| p.admission == harness_protocol::messages::Admission::Closed)
        {
            return Plan::Unavailable(
                "the run is ending and takes no more messages; its work is delivered, start a follow-up run".into(),
            );
        }
        Plan::Next
    }

    /// The words after the box's name in its title: what Enter will do.
    pub(crate) fn send_title(&self) -> Option<String> {
        let id = self.focused_run()?;
        let short: String = id.chars().take(8).collect();
        let draft = self.chat_text().unwrap_or_default();
        let now = draft.starts_with("/now ") || draft == "/now";
        let title = match self.send_plan() {
            Plan::Answer => "Enter: Answer question".to_string(),
            Plan::Next if now => format!("Enter: Interrupt run {short} (/now)"),
            Plan::Next => format!("Enter: Send next to run {short}"),
            Plan::FollowUp => format!("Enter: {}", followup::header(self.send.head.as_deref())),
            Plan::Retry => format!("Enter: {}", retry::header(id)),
            Plan::Unavailable(_) => engines::NOT_WAITING_TITLE.to_string(),
        };
        // A blocked Enter makes the title say so for a moment, so the person reads it again before pressing Enter once more.
        let flashing = self
            .send
            .flash_until
            .is_some_and(|until| self.clock < until);
        Some(if flashing {
            format!("{title} (changed just now)")
        } else {
            title
        })
    }

    /// Notes a change of what Enter would do; a key within [`CONTEXT_GUARD_MS`] of it is blocked.
    pub(crate) fn send_context_changed(&mut self) {
        let key = (
            self.send_plan(),
            self.question().map(|q| q.request_id.clone()),
            self.send.page.as_ref().map(|p| p.epoch_no),
        );
        if self.send.seen.as_ref() != Some(&key) {
            self.send.changed_at = Some(Instant::now());
            self.send.seen = Some(key);
        }
    }

    /// Enter in the box for a plain line (not a command): `None` when the existing route answers it (an open question).
    pub(crate) fn send_enter(&mut self, line: &str) -> Option<Vec<Effect>> {
        let plan = self.send_plan();
        // Enter is bound to what the box showed, an answer included: a question that closed or was replaced in the last moments blocks it,
        // and the text is never answered to a question the person did not see.
        if let Some(at) = self.send.changed_at {
            let changed_ms = u64::try_from(at.saturating_duration_since(self.epoch).as_millis())
                .unwrap_or(u64::MAX);
            if self.key_ms.saturating_sub(changed_ms) < CONTEXT_GUARD_MS {
                self.send.flash_until = Some(self.clock + FLASH);
                self.notice = Some(
                    "the run changed just now: check the title of the box, then press Enter again"
                        .into(),
                );
                return Some(vec![]);
            }
        }
        Some(match plan {
            Plan::Answer => return None,
            Plan::Next => self.send_text(line, MessageMode::Next),
            Plan::FollowUp => self.follow_up_enter(line),
            Plan::Retry => self.retry_offer(),
            Plan::Unavailable(why) => {
                self.notice = Some(if why == NOT_WAITING_TEXT {
                    format!("{NOT_WAITING_TEXT}. The text stays in the box.")
                } else {
                    format!("{why}. The text stays in the box.")
                });
                vec![]
            }
        })
    }

    /// Sends `text` as a message in `mode` now (the plan has been checked): the box is emptied, a pending row shows "sending".
    pub(crate) fn send_text(&mut self, text: &str, mode: MessageMode) -> Vec<Effect> {
        let req = messages::plain(mint_message_id(), text, mode);
        self.send_request(req, true)
    }

    /// Starts the send of `req`; `clear` empties the box (the text was the box's).
    fn send_request(&mut self, req: SendMessageRequest, clear: bool) -> Vec<Effect> {
        if let Err(e) = messages::validate_send(&req) {
            self.say(&e.to_string());
            return vec![];
        }
        let open = self
            .send
            .page
            .as_ref()
            .map_or(0, |p| p.messages.iter().filter(|m| open_state(m)).count())
            + self.send.local.len();
        if open >= MAX_OPEN {
            self.say(&engines::say(
                "3 messages are waiting for {engine} already; wait for one to be delivered or withdraw one (w)",
            ));
            return vec![];
        }
        let Some(run_id) = self.focused_run().map(str::to_string) else {
            return vec![];
        };
        self.send.local.push(Local {
            request: req.clone(),
            phase: Phase::Sending,
            tries: 1,
            retry_at: Duration::ZERO,
            inflight: true,
        });
        if clear {
            self.clear_chat();
        }
        self.panel = None;
        vec![Effect::Ext(Job::SendMessage {
            run_id,
            request: req,
        })]
    }

    // ---- /now ------------------------------------------------------------------------------------------------------------------

    /// `/now <text>`: a preview, and the interrupt on the second Enter.
    pub(crate) fn send_now(&mut self, text: &str) -> Vec<Effect> {
        if !self.caps.offers(engines::NOW_CONTROL) {
            self.panel = Some(Panel::not_run(
                "Nothing was sent",
                "This session does not take /now, an interrupt. Enter sends the text as a message for after the turn.",
            ));
            return vec![];
        }
        match self.send_plan() {
            Plan::Next => {}
            Plan::Answer => {
                self.panel = Some(Panel::not_run(
                    "Nothing was sent",
                    &engines::say(
                        "{engine} is waiting for your answer to a question; Enter sends the text as the answer.",
                    ),
                ));
                return vec![];
            }
            Plan::Retry => {
                self.panel = Some(Panel::not_run("Nothing was sent", retry::HINT));
                return vec![];
            }
            Plan::FollowUp | Plan::Unavailable(_) => {
                self.panel = Some(Panel::not_run("Nothing was sent", NOT_WAITING_TEXT));
                return vec![];
            }
        }
        let run = self.focused_run().unwrap_or_default().to_string();
        // The title is the Confirm box's, like every line that changes a run and waits for a second Enter.
        self.panel = Some(Panel::info(
            "Confirm",
            &engines::say(&format!(
                "Interrupt {{engine}}?\nRun {run}\nThis stops what {{engine}} is doing now and sends:\n{text}\nA cancelled turn leaves no partial text in {{engine}}'s memory, so restate what your message refers to.\nEnter sends it now \u{b7} Esc cancels"
            )),
        ));
        self.send.confirm = Some(Confirm::Now {
            text: text.to_string(),
            shown_ms: self.key_ms,
        });
        vec![]
    }

    // ---- keys ------------------------------------------------------------------------------------------------------------------

    /// A key while a preview waits (typing mode): Enter after the delay confirms, Esc cancels, anything else cancels and is the box's.
    pub(crate) fn send_typing_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        if let Some(Confirm::Retry { shown_ms }) = self.send.confirm {
            return self.retry_confirm_key(key, shown_ms, false);
        }
        let Some(Confirm::Now { text, shown_ms }) = self.send.confirm.clone() else {
            return None;
        };
        match key.code {
            KeyCode::Enter => {
                if !self.calm || self.key_ms.saturating_sub(shown_ms) < CONFIRM_DELAY_MS {
                    return Some(vec![]);
                }
                self.send.confirm = None;
                self.panel = None;
                let req = messages::plain(mint_message_id(), &text, MessageMode::Now);
                Some(self.send_request(req, true))
            }
            KeyCode::Esc => {
                self.send.confirm = None;
                self.panel = None;
                Some(vec![])
            }
            _ => {
                self.send.confirm = None;
                self.panel = None;
                None
            }
        }
    }

    /// A key in browse mode while a confirmation of a held message waits (typing mode never has one): Enter after the delay sends it, Esc
    /// cancels, `x` again cancels it, any other key cancels it and is the screen's. The keys `w`, `x` and `h` themselves are rows of the
    /// key table (`keys::TABLE`) and run through `browse_action`, after the search bar and the result panel have had their turn.
    pub(crate) fn send_browse_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        if let Some(Confirm::Retry { shown_ms }) = self.send.confirm {
            return self.retry_confirm_key(key, shown_ms, true);
        }
        let Some(Confirm::Held {
            message,
            epoch,
            shown_ms,
        }) = self.send.confirm.clone()
        else {
            return None;
        };
        Some(match key.code {
            KeyCode::Enter => {
                if !self.calm || self.key_ms.saturating_sub(shown_ms) < CONFIRM_DELAY_MS {
                    return Some(vec![]);
                }
                self.send.confirm = None;
                self.panel = None;
                // The confirmation names the session the person was shown. A session that changed again, or a message that is no longer
                // waiting, sends nothing: the person reads the rows again (the service refuses it as well; this saves the round trip).
                let page = self.send.page.as_ref();
                let now_epoch = page.and_then(|p| p.epoch.clone()).unwrap_or_default();
                let still_waiting = page
                    .and_then(|p| {
                        p.messages
                            .iter()
                            .find(|m| m.message_id == message.message_id)
                    })
                    .is_some_and(is_confirmable);
                if now_epoch != epoch {
                    self.say(
                        "the session changed again while you were looking: read the messages and confirm again",
                    );
                    return Some(vec![]);
                }
                if !still_waiting {
                    self.say("that message is no longer waiting for you");
                    return Some(vec![]);
                }
                // The message was confirmable when the preview opened and is still waiting, so the request is always made.
                match messages::confirmation(mint_message_id(), &message, &epoch) {
                    Ok(req) => self.send_request(req, false),
                    Err(_) => vec![],
                }
            }
            KeyCode::Esc => {
                self.send.confirm = None;
                self.panel = None;
                vec![]
            }
            _ => {
                self.send.confirm = None;
                self.panel = None;
                // `x` again only cancels; it does not open a second preview.
                if keys::lookup(Mode::Browse, key) == Some(Action::ConfirmHeld) {
                    return Some(vec![]);
                }
                return None;
            }
        })
    }

    /// The messages the person can send again on purpose (held, or that may have reached the agent), oldest first.
    fn confirmables(&self) -> Vec<MessageView> {
        self.send.page.as_ref().map_or_else(Vec::new, |p| {
            p.messages
                .iter()
                .filter(|m| is_confirmable(m))
                .cloned()
                .collect()
        })
    }

    /// The id of the chosen row, when two or more messages are waiting for a confirmation and the person has chosen one that still is.
    fn chosen(&self) -> Option<String> {
        let list = self.confirmables();
        let id = self.send.choice.as_deref()?;
        (list.len() > 1 && list.iter().any(|m| m.message_id == id)).then(|| id.to_string())
    }

    /// `w`: withdraws the chosen message, else the newest one nobody has claimed.
    pub(crate) fn send_withdraw_action(&mut self) -> Vec<Effect> {
        if !self.calm {
            return vec![];
        }
        let chosen = self.chosen();
        let messages: &[MessageView] = self.send.page.as_ref().map_or(&[], |p| &p.messages);
        // The chosen row is the one `w` acts on; a chosen message that cannot be withdrawn is never swapped for another one.
        let chosen_row = chosen
            .as_deref()
            .and_then(|id| messages.iter().find(|m| m.message_id == id));
        let target = match chosen_row {
            Some(m) if is_withdrawable(m) => Ok(m.message_id.clone()),
            Some(_) => Err(engines::say(
                "the chosen message may have reached {engine}, so it cannot be withdrawn (x sends it again)",
            )),
            None => messages
                .iter()
                .rev()
                .find(|m| is_withdrawable(m))
                .map(|m| m.message_id.clone())
                .ok_or_else(|| {
                    if self.send.local.is_empty() {
                        "no message is waiting to be withdrawn".to_string()
                    } else {
                        "the message is still on its way: w can withdraw it once the service has it"
                            .to_string()
                    }
                }),
        };
        match target {
            Ok(message_id) => {
                let run_id = self.focused_run().unwrap_or_default().to_string();
                vec![Effect::Ext(Job::Withdraw { run_id, message_id })]
            }
            Err(why) => {
                self.say(&why);
                vec![]
            }
        }
    }

    /// `h`: the choice moves to the next held message (the first when none is chosen), and wraps. One message needs no choice.
    pub(crate) fn send_choose_action(&mut self) -> Vec<Effect> {
        let list = self.confirmables();
        match list.len() {
            0 => self.say("no message is held"),
            1 => self.say("one message is held: x shows it, then Enter sends it again"),
            n => {
                let at = self
                    .send
                    .choice
                    .as_deref()
                    .and_then(|id| list.iter().position(|m| m.message_id == id));
                let next = at.map_or(0, |i| (i + 1) % n);
                self.send.choice = Some(list[next].message_id.clone());
                self.say(&format!(
                    "{n} messages are held: the one marked chosen is the one x and w act on; h chooses the next"
                ));
            }
        }
        vec![]
    }

    /// `x`: previews the confirmation of the chosen held message, or of the only one. With two or more held and none chosen it chooses the
    /// first and shows no preview: a confirmation is bound to a row the person has seen marked.
    pub(crate) fn send_confirm_action(&mut self) -> Vec<Effect> {
        if !self.calm {
            return vec![];
        }
        let list = self.confirmables();
        let m = match (list.len(), self.chosen()) {
            (0, _) => {
                self.say("no message is waiting for your confirmation");
                return vec![];
            }
            (1, _) => list[0].clone(),
            (_, Some(id)) => match list.iter().find(|m| m.message_id == id) {
                Some(m) => m.clone(),
                None => return vec![],
            },
            (n, None) => {
                self.send.choice = Some(list[0].message_id.clone());
                self.say(&format!(
                    "{n} messages are held: the first is chosen. h chooses another, then x shows the chosen one"
                ));
                return vec![];
            }
        };
        let (epoch, now_no) = self.send.page.as_ref().map_or((String::new(), 0), |p| {
            (p.epoch.clone().unwrap_or_default(), p.epoch_no)
        });
        let run = self.focused_run().unwrap_or_default().to_string();
        let mut lines = vec![
            format!("Run {run}"),
            format!("Written to session {}, now session {now_no}.", m.epoch_no),
            format!("Text: {}", m.text),
        ];
        if m.state == MessageState::DeliveryUnknown {
            lines.push(engines::say(
                "It may already have run: {engine} could have taken it before the worker was lost. This sends it again.",
            ));
        }
        lines.push("Enter sends it as a new message \u{b7} Esc cancels".to_string());
        self.panel = Some(Panel::info("Send this message again?", &lines.join("\n")));
        self.send.confirm = Some(Confirm::Held {
            message: Box::new(m),
            epoch,
            shown_ms: self.key_ms,
        });
        vec![]
    }

    /// The footer with the message keys that work now: while a message can be withdrawn or confirmed the footer says which key does it (the
    /// letters work in browse mode only, so in typing mode the hint names the way there).
    pub(crate) fn send_footer(&self, base: String, typing: bool) -> String {
        // A blocked run: browse mode's footer already names `r retry` in the place of `c cancel` (`usual_help`); typing mode names the way there.
        if typing && self.run_is_blocked() {
            return format!("{base} \u{b7} Esc, then r retry");
        }
        let Some(page) = self.send.page.as_ref() else {
            return base;
        };
        let confirmable = page.messages.iter().filter(|m| is_confirmable(m)).count();
        let mut hints: Vec<&str> = Vec::new();
        if page.messages.iter().any(is_withdrawable) {
            hints.push("w withdraw");
        }
        if confirmable > 0 {
            hints.push("x confirm");
        }
        if confirmable > 1 {
            hints.push("h choose");
        }
        if hints.is_empty() {
            return base;
        }
        if typing {
            // One part (commas, not the footer's separator): a part of its own that starts with a letter would be a click target that types it.
            format!("{base} \u{b7} Esc, then {}", hints.join(", "))
        } else {
            format!("{} \u{b7} {base}", hints.join(" \u{b7} "))
        }
    }

    // ---- an answer typed in the box ---------------------------------------------------------------------------------------------------

    /// An answer to the open question `request_id` goes out from the box: a pending row shows it as "sending". A second answer to the same
    /// question (the first was never taken) replaces the first row.
    pub(crate) fn send_answer_started(&mut self, request_id: &str, text: &str) {
        self.send.answers.retain(|a| a.request_id != request_id);
        self.send.answers.push(AnswerRow {
            request_id: request_id.to_string(),
            text: text.to_string(),
            at: String::new(),
            phase: AnswerPhase::Sending,
        });
        self.send.answering = Some(request_id.to_string());
    }

    /// The service's answer to an answer. An answer it took stands in the conversation from now on (placed by this moment); one it refused
    /// leaves no row and gives the text back. An answer that was not typed in the box (the reply screen's) is not this one's business.
    pub(crate) fn send_answer_done(&mut self, result: &Result<InputAccepted, String>) {
        let Some(request_id) = self.send.answering.take() else {
            return;
        };
        let Some(at) = self
            .send
            .answers
            .iter()
            .position(|a| a.request_id == request_id)
        else {
            return;
        };
        match result {
            Ok(_) => {
                let taken = self.now.map(|now| {
                    let (y, mo, d, h, mi, s) = utc_civil(now);
                    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
                });
                let a = &mut self.send.answers[at];
                a.phase = AnswerPhase::Taken;
                a.at = taken.unwrap_or_default();
                self.rebuild_chat();
            }
            Err(_) => {
                let a = self.send.answers.remove(at);
                self.restore_text(&a.text);
            }
        }
    }

    // ---- answers and the timer -------------------------------------------------------------------------------------------

    /// An answer to a send, a withdrawal or a read of the list.
    pub(crate) fn send_done(&mut self, done: Done) -> Vec<Effect> {
        match done {
            Done::Messages { run_id, result } if run_id == self.send.run_id => {
                self.send.list_inflight = false;
                self.send.next_list = self.clock + LIST_EVERY;
                if let Ok(page) = result {
                    // A send of ours that the list now holds is the list's.
                    self.send.local.retain(|l| {
                        !page
                            .messages
                            .iter()
                            .any(|m| m.message_id == l.request.message_id)
                    });
                    self.send.page = Some(page);
                }
            }
            Done::Sent {
                run_id,
                message_id,
                result,
            } if run_id == self.send.run_id => self.sent(&message_id, result),
            Done::Withdrawn {
                run_id,
                message_id,
                result,
            } if run_id == self.send.run_id => {
                match result {
                    Ok(r) => self.upsert(r.message),
                    Err(f) => {
                        let why = match f.code.as_str() {
                            "already-dispatching" => engines::say(
                                "{engine} has already taken that message, so it cannot be withdrawn",
                            ),
                            _ => ham_core::sanitize::sanitize_line(&f.message),
                        };
                        self.say(&why);
                        // The real state is read again at once.
                        self.send.next_list = self.clock;
                        let _ = message_id;
                    }
                }
            }
            Done::FollowUpFound { run_id, result } => {
                return self.follow_up_found(&run_id, result);
            }
            Done::FollowUpStarted { run_id, result } => {
                return self.follow_up_done(&run_id, result);
            }
            Done::RetryStarted { run_id, result } => {
                return self.retry_done(&run_id, result);
            }
            Done::Resumed { outcomes } => {
                return self.resume_done(&outcomes);
            }
            Done::Head { run_id, result } => {
                return self.head_done(&run_id, result);
            }
            Done::EngineSettings { result } => {
                return self.engine_settings_done(result);
            }
            _ => return vec![],
        }
        self.send_note_settled();
        self.send_context_changed();
        self.rebuild_chat();
        vec![]
    }

    fn upsert(&mut self, m: MessageView) {
        let page = self.send.page.get_or_insert_with(|| MessagesPage {
            run_id: self.send.run_id.clone(),
            messages: Vec::new(),
            admission: harness_protocol::messages::Admission::Open,
            epoch: m.epoch.clone(),
            epoch_no: m.epoch_no,
            watermark: 0,
        });
        match page
            .messages
            .iter_mut()
            .find(|x| x.message_id == m.message_id)
        {
            Some(x) => *x = m,
            None => page.messages.push(m),
        }
        page.messages.sort_by_key(|x| x.seq);
    }

    fn sent(
        &mut self,
        message_id: &str,
        result: Result<messages::SendMessageResponse, SendFailure>,
    ) {
        match result {
            Ok(r) => {
                self.send
                    .local
                    .retain(|l| l.request.message_id != message_id);
                self.upsert(r.message);
                self.send.next_list = self.clock + LIST_EVERY;
            }
            Err(f) => {
                if f.code == "no-message-control" {
                    self.send.no_control = true;
                }
                let r = explain_refusal(f.status, &f.code, &f.message);
                let Some(at) = self
                    .send
                    .local
                    .iter()
                    .position(|l| l.request.message_id == message_id)
                else {
                    return;
                };
                if r.definitive {
                    let local = self.send.local.remove(at);
                    if r.restore_text {
                        self.restore_text(&local.request.text);
                    }
                    self.say(&r.text);
                } else {
                    // No answer is not a refusal: the id stays, the row stays "sending", and the send is asked again by that id (the service
                    // answers a repeat with the message it holds). The text is not put back as a fresh message.
                    let l = &mut self.send.local[at];
                    l.inflight = false;
                    l.phase = Phase::Uncertain;
                    l.retry_at = self.clock + retry_pause(l.tries);
                }
            }
        }
    }

    /// Puts a message's text back in the box (before whatever has been typed since).
    fn restore_text(&mut self, text: &str) {
        let now = self.chat_text().unwrap_or_default();
        if now.is_empty() {
            self.set_chat_text(text);
        } else if !now.contains(text) {
            self.set_chat_text(&format!("{text}\n{now}"));
        }
    }

    /// The 1 s timer: uncertain sends again, the list while a message is on its way.
    pub(crate) fn send_tick(&mut self) -> Vec<Effect> {
        if self.screen != Screen::Dashboard || self.send.run_id.is_empty() {
            return vec![];
        }
        let mut effects = Vec::new();
        let run_id = self.send.run_id.clone();
        let clock = self.clock;
        effects.extend(self.head_lookup_tick(&run_id));
        for l in &mut self.send.local {
            if l.phase == Phase::Uncertain && !l.inflight && clock >= l.retry_at {
                l.inflight = true;
                l.tries = l.tries.saturating_add(1);
                effects.push(Effect::Ext(Job::SendMessage {
                    run_id: run_id.clone(),
                    request: l.request.clone(),
                }));
            }
        }
        let open = self
            .send
            .page
            .as_ref()
            .is_some_and(|p| p.messages.iter().any(open_state))
            || self.send.local.iter().any(|l| l.phase == Phase::Sending);
        let wanted = open
            || self.send.page.as_ref().is_some_and(|p| {
                p.messages
                    .iter()
                    .any(|m| m.state == MessageState::Dispatching)
            });
        if wanted && !self.send.list_inflight && clock >= self.send.next_list {
            self.send.list_inflight = true;
            effects.push(Effect::Ext(Job::Messages { run_id }));
        }
        effects
    }

    // ---- what the screen shows ----------------------------------------------------------------------------------------------

    /// The pending rows above the box.
    pub(crate) fn pending_rows(&self) -> Vec<PendingRow> {
        let mut rows: Vec<(u32, PendingRow, bool)> = Vec::new();
        let epoch_no = self.send.page.as_ref().map_or(1, |p| p.epoch_no);
        let many = self
            .send
            .page
            .as_ref()
            .is_some_and(|p| p.messages.iter().filter(|m| is_confirmable(m)).count() > 1);
        let choice = self.chosen();
        if let Some(p) = &self.send.page {
            for m in &p.messages {
                let chosen = choice.as_deref() == Some(m.message_id.as_str());
                let keep = match self.send.settled_seen.get(&m.message_id) {
                    _ if open_state(m) => true,
                    _ if matches!(m.state, MessageState::DeliveryUnknown) => true,
                    Some(first) => self.clock.saturating_sub(*first) < SHOW_SETTLED,
                    None => true,
                };
                if !keep {
                    continue;
                }
                let status = match (m.state, m.mode) {
                    (MessageState::Pending, MessageMode::Next) => PendingStatus::WaitingForTurn,
                    // A message on its way that is not waiting for the turn to end starts with the word `sending` (the battery reads the
                    // status of a pending row from a fixed vocabulary), and the words after it say what is happening.
                    (MessageState::Pending, MessageMode::Now) => {
                        PendingStatus::Words(engines::say("sending: waiting to interrupt {engine}"))
                    }
                    (MessageState::Dispatching, _) => {
                        PendingStatus::Words(engines::say("sending: {engine} is taking it now"))
                    }
                    (MessageState::Delivered, _) => match m.delivered_turn {
                        Some(turn) => PendingStatus::Delivered { turn },
                        None => PendingStatus::Words("delivered".into()),
                    },
                    (MessageState::Rejected, _) => {
                        let words = status_line(m, epoch_no);
                        let why = words.strip_prefix("not sent: ").unwrap_or(&words);
                        PendingStatus::Failed(why.to_string())
                    }
                    // With two or more messages waiting for a confirmation, one is chosen at a time: its row says so, the others say how to
                    // choose them.
                    (MessageState::Held, _) => PendingStatus::Words(match (many, chosen) {
                        (false, _) => format!(
                            "held: written to session {}, now session {epoch_no} (x confirms, w withdraws)",
                            m.epoch_no
                        ),
                        (true, true) => format!(
                            "held, chosen: written to session {}, now session {epoch_no} (x confirms, w withdraws)",
                            m.epoch_no
                        ),
                        (true, false) => format!(
                            "held: written to session {}, now session {epoch_no} (h chooses)",
                            m.epoch_no
                        ),
                    }),
                    (MessageState::DeliveryUnknown, _) => {
                        PendingStatus::Words(engines::say(match (many, chosen) {
                            (false, _) => "may have reached {engine} (x sends it again)",
                            (true, true) => "may have reached {engine}, chosen (x sends it again)",
                            (true, false) => "may have reached {engine} (h chooses)",
                        }))
                    }
                    _ => PendingStatus::Words(status_line(m, epoch_no)),
                };
                rows.push((
                    m.seq,
                    PendingRow {
                        text: m.text.clone(),
                        status,
                    },
                    true,
                ));
            }
        }
        for (n, l) in self.send.local.iter().enumerate() {
            let status = match &l.phase {
                Phase::Sending => PendingStatus::Sending,
                // No answer yet: still "sending", and the row says why and that the id is asked about again.
                Phase::Uncertain => PendingStatus::Words(explain_refusal(0, "", "").text),
            };
            rows.push((
                u32::MAX - 100 + u32::try_from(n).unwrap_or(0),
                PendingRow {
                    text: l.request.text.clone(),
                    status,
                },
                true,
            ));
        }
        // The answers typed in the box: on their way, or taken by the service while the question they answer is still open.
        let open_question = self.question().map(|q| q.request_id.as_str());
        for (n, a) in self.send.answers.iter().enumerate() {
            let status = match a.phase {
                AnswerPhase::Sending => PendingStatus::Sending,
                AnswerPhase::Taken if open_question == Some(a.request_id.as_str()) => {
                    PendingStatus::Words(engines::say("sending: waiting for {engine} to take it"))
                }
                AnswerPhase::Taken => continue,
            };
            rows.push((
                u32::MAX - 50 + u32::try_from(n).unwrap_or(0),
                PendingRow {
                    text: a.text.clone(),
                    status,
                },
                true,
            ));
        }
        rows.sort_by_key(|(seq, _, _)| *seq);
        let skip = rows.len().saturating_sub(MAX_ROWS);
        rows.into_iter().skip(skip).map(|(_, r, _)| r).collect()
    }

    /// Notes when each settled message was first on the rows, so it can leave them after [`SHOW_SETTLED`].
    pub(crate) fn send_note_settled(&mut self) {
        let clock = self.clock;
        if let Some(p) = &self.send.page {
            for m in &p.messages {
                if !open_state(m) {
                    self.send
                        .settled_seen
                        .entry(m.message_id.clone())
                        .or_insert(clock);
                }
            }
        }
    }

    /// The user's own items of the conversation: the messages that the agent has been given, placed by the time the server accepted them.
    pub(crate) fn send_user_items(&self) -> Vec<UserItem> {
        let mut items: Vec<UserItem> = self.send.page.as_ref().map_or_else(Vec::new, |p| {
            p.messages
                .iter()
                .filter(|m| m.state == MessageState::Delivered)
                .map(|m| UserItem {
                    at: m.accepted_at.clone(),
                    block: Block::message(
                        &format!("msg:{}", m.message_id),
                        Role::You,
                        Origin::User,
                        &m.text,
                    ),
                })
                .collect()
        });
        // An answer typed in the box stands in the conversation from the moment the service took it.
        items.extend(
            self.send
                .answers
                .iter()
                .filter(|a| a.phase == AnswerPhase::Taken)
                .map(|a| UserItem {
                    at: a.at.clone(),
                    block: Block::message(
                        &format!("ans:{}", a.request_id),
                        Role::You,
                        Origin::User,
                        &a.text,
                    ),
                }),
        );
        items
    }

    /// The user's own items placed in `model`, a progress feed (the run has no stored conversation to compose them into, so the store's
    /// `compose` does not run): each stands before the first block of the feed that is later than it, else at the end.
    pub(crate) fn send_with_user_items(&self, model: ChatModel) -> ChatModel {
        let mut items = self.send_user_items();
        if items.is_empty() {
            return model;
        }
        items.sort_by_key(|i| harness_protocol::common::parse_utc_unix(&i.at).unwrap_or(i64::MAX));
        let mut items = items.into_iter().peekable();
        let mut blocks: Vec<Block> = Vec::with_capacity(model.blocks.len() + items.len());
        for b in model.blocks {
            // A progress-feed id is `<kind>:<ULID>`; the prompt and the cards have no time and keep their place.
            let at =
                b.id.split_once(':')
                    .and_then(|(_, rest)| ham_core::stored::ulid_secs(rest));
            while let Some(bt) = at
                && let Some(item) = items.peek()
                && harness_protocol::common::parse_utc_unix(&item.at).is_some_and(|t| t < bt)
            {
                if let Some(item) = items.next() {
                    blocks.push(item.block);
                }
            }
            blocks.push(b);
        }
        blocks.extend(items.map(|i| i.block));
        ChatModel { blocks, ..model }
    }

    // ---- the follow-up ------------------------------------------------------------------------------------------------------

    fn digest_inputs<'a>(&'a self, said: &'a [String]) -> followup::Inputs<'a> {
        let view = self.cached.as_ref();
        followup::Inputs {
            run_id: self.focused_run().unwrap_or_default(),
            objective: self.detail.as_ref().and_then(|d| d.objective.as_deref()),
            outcome_label: view
                .and_then(|v| v.result.as_ref())
                .map(|r| r.label.as_str()),
            // What the delivery changed comes from the validator's record, through the head route; a run with no record says so.
            changed: match self.send.delivered.as_ref().map(|d| &d.changes) {
                Some(Changes::Recorded(list)) => ChangedFiles::Listed(list),
                _ => ChangedFiles::Unavailable,
            },
            pr_url: view.and_then(|v| v.pr_url.as_deref()),
            head_sha: self.send.head.as_deref(),
            said,
            harness_prompts: view.map_or(0, |v| v.harness_prompts as usize),
            // The engine's last message is not an input of the digest: the key adds it to the box after the digest (`add_cline_message`).
            cline_final: None,
            nonce: &self.send.fence,
        }
    }

    /// The offer of a finished run, drawn as a panel above the box as soon as the box holds text and nothing else is in that place (T60: the
    /// two sentences of the plan).
    pub(crate) fn follow_up_hint(&self) -> Option<Panel> {
        if self.panel.is_some()
            || self.send.confirm.is_some()
            || self.screen != Screen::Dashboard
            || self
                .chat_text()
                .is_none_or(|t| t.trim().is_empty() || t.trim_start().starts_with('/'))
        {
            return None;
        }
        match self.send_plan() {
            Plan::FollowUp => Some(Panel::not_run(
                "Follow-up",
                &format!("{}\n{}", followup::OFFER, followup::NOTE),
            )),
            // A blocked run says what happened and what Enter does as soon as the box holds text.
            Plan::Retry => Some(Panel::not_run("Blocked run", retry::HINT)),
            _ => None,
        }
    }

    /// The panel of a prepared follow-up: the offer, then what the second Enter starts (the commit, the source run and its target, the
    /// target of the new run, that it is billable) and the keys.
    fn follow_up_panel(&self) -> Panel {
        let run = self.focused_run().unwrap_or_default().to_string();
        let source_target = self
            .detail
            .as_ref()
            .map(|d| ham_core::run_index::target_text(&d.summary.target))
            .map(|t| ham_core::sanitize::sanitize_line(&t));
        let waiting = match self.send.lookup {
            HeadLookup::NotAsked | HeadLookup::Asking { .. } if self.send.head.is_none() => {
                "\nLooking up the delivered commit: the panel names it in a moment."
            }
            _ => "",
        };
        let has_message = self
            .send
            .delivered
            .as_ref()
            .and_then(DeliveredHead::message)
            .is_some();
        let keys = if has_message {
            "Enter starts it \u{b7} Ctrl-L adds {engine}'s last message (untrusted, cut to 1,500 characters) \u{b7} Esc keeps editing"
        } else {
            "Enter starts it \u{b7} Esc keeps editing"
        };
        let changed = self.engine_settings_line(&run);
        Panel::not_run(
            "Follow-up",
            &engines::say(&format!(
                "{}\n{}\n{}{waiting}{changed}\n{keys}",
                followup::OFFER,
                followup::NOTE,
                followup::confirmation(&run, self.send.head.as_deref(), source_target.as_deref()),
            )),
        )
    }

    /// The line of the confirmation about the engine settings: empty while the file's hash is not known and when it equals the hash of the run's
    /// own card (or neither has settings); else that the settings changed since the run, with what the new run sends. The follow-up reads the
    /// file afresh and does not copy the source run's settings.
    fn engine_settings_line(&self, run: &str) -> String {
        let Some(now) = &self.send.engine_now else {
            return String::new();
        };
        let then = self
            .cached
            .as_ref()
            .and_then(|v| v.engine_config.as_ref())
            .map(|c| c.sha256.clone());
        if *now == then {
            return String::new();
        }
        let run = ham_core::sanitize::sanitize_line(run);
        match now {
            Some(hash) => format!(
                "\nEngine settings changed since run {run}: the new run sends the file as it is now (settings hash {}).",
                hash.get(..12).unwrap_or("?")
            ),
            None => format!(
                "\nEngine settings changed since run {run}: the file now sets none, so the new run uses the engine's own."
            ),
        }
    }

    /// The answer to [`Job::EngineSettings`]: the line about changed settings joins the confirmation if it is on the screen. A file the check
    /// refuses adds nothing here; the start refuses it with the words.
    fn engine_settings_done(&mut self, result: Result<Option<String>, String>) -> Vec<Effect> {
        let Ok(hash) = result else {
            return vec![];
        };
        self.send.engine_now = Some(hash);
        if self.panel.is_some() && matches!(self.send.confirm, Some(Confirm::FollowUp { .. })) {
            self.panel = Some(self.follow_up_panel());
        }
        vec![]
    }

    /// The job the second Enter starts: a finished repository run is followed on its own repository, so the new run opens a pull
    /// there; an `s3-only` run, or one whose summary has not arrived (the confirmation then said s3-only), is followed by a zip alone. The target
    /// is read from the same summary as the confirmation's words, so the run that starts is the one that was confirmed.
    fn follow_up_job(&self, run_id: String, text: String) -> Job {
        let head = self.send.head.clone();
        let repo = self.detail.as_ref().and_then(|d| match &d.summary.target {
            harness_protocol::target::RunTarget::Repo { repo }
            | harness_protocol::target::RunTarget::NewRepo { repo } => Some(repo.clone()),
            harness_protocol::target::RunTarget::S3Only => None,
        });
        match repo {
            Some(repo) => Job::StartRepoFollowUp {
                run_id,
                text,
                head,
                repo,
            },
            None => Job::StartFollowUp { run_id, text, head },
        }
    }

    fn show_follow_up_confirm(&mut self, checked: bool) {
        self.panel = Some(self.follow_up_panel());
        self.send.confirm = Some(Confirm::FollowUp {
            shown_ms: self.key_ms,
            checked,
        });
    }

    // ---- the delivered head ---------------------------------------------------------------------------------------------------

    /// The timer's part of the head lookup: once a finished, delivered run is on screen the head route is asked, and asked
    /// again after a failure that may pass (no answer, a 5xx), up to [`HEAD_TRIES`] times. A refusal that will not change is not asked again.
    fn head_lookup_tick(&mut self, run_id: &str) -> Vec<Effect> {
        if self.screen != Screen::Dashboard || self.send_plan() != Plan::FollowUp {
            return vec![];
        }
        let tries = match &self.send.lookup {
            HeadLookup::NotAsked => 0,
            HeadLookup::Failed {
                tries,
                retry_at,
                definitive: None,
            } if *tries < HEAD_TRIES && self.clock >= *retry_at => *tries,
            _ => return vec![],
        };
        self.send.lookup = HeadLookup::Asking { tries: tries + 1 };
        vec![Effect::Ext(Job::Head {
            run_id: run_id.to_string(),
        })]
    }

    /// The answer of the head route: the commit and what comes with it are kept, and a panel that is open is written again with the commit.
    fn head_done(
        &mut self,
        run_id: &str,
        result: Result<DeliveredHead, SendFailure>,
    ) -> Vec<Effect> {
        if run_id != self.send.run_id {
            return vec![];
        }
        let tries = match self.send.lookup {
            HeadLookup::Asking { tries } => tries,
            _ => 1,
        };
        match result {
            Ok(head) if head.check(run_id).is_ok() => {
                self.send.head = Some(head.head_sha.clone());
                self.send.delivered = Some(head);
                self.send.lookup = HeadLookup::Known;
                // The panel that names the commit says it now. Its time of showing stays, so the 300 ms guard of the second Enter holds.
                if matches!(self.send.confirm, Some(Confirm::FollowUp { .. }))
                    && self
                        .panel
                        .as_ref()
                        .is_some_and(|p| p.title() == "Follow-up")
                {
                    self.panel = Some(self.follow_up_panel());
                }
            }
            Ok(_) => {
                self.send.lookup = HeadLookup::Failed {
                    tries,
                    retry_at: self.clock,
                    definitive: Some((
                        "unusable-answer".into(),
                        "the head lookup answered about another run or with a head that is not a commit".into(),
                    )),
                };
            }
            Err(f) => {
                // A refusal the server states does not change by asking again; no answer, a throttle or a 5xx does.
                let transient = f.status == 0 || f.status == 429 || f.status >= 500;
                let wait = HEAD_RETRY_AFTER
                    .get(usize::from(tries).saturating_sub(1))
                    .copied()
                    .unwrap_or(Duration::from_secs(60));
                self.send.lookup = HeadLookup::Failed {
                    tries,
                    retry_at: self.clock + wait,
                    definitive: (!transient).then(|| {
                        (
                            f.code.clone(),
                            ham_core::sanitize::sanitize_line(&f.message),
                        )
                    }),
                };
            }
        }
        vec![]
    }

    /// The key that adds the engine's last message to the follow-up draft. It works while the confirmation is on the screen,
    /// appends the message in its untrusted, nonce-fenced block after everything in the box, and a second press changes nothing. The key also
    /// repaints the screen on every screen (`App::key_at` asks for it before any screen sees the key), so outside a prepared follow-up the
    /// notice says so first.
    pub(crate) fn add_cline_message(&mut self) -> Vec<Effect> {
        if !matches!(self.send.confirm, Some(Confirm::FollowUp { .. })) {
            self.say(&engines::say(
                "repainted the screen · Ctrl-L adds {engine}'s last message to a follow-up draft: press Enter on a finished run first",
            ));
            return vec![];
        }
        let Some(message) = self
            .send
            .delivered
            .as_ref()
            .and_then(DeliveredHead::message)
        else {
            let words = match self.send.lookup {
                HeadLookup::NotAsked | HeadLookup::Asking { .. } => {
                    "still looking up {engine}'s last message: try again in a moment"
                }
                _ => "{engine} left no last message to add",
            };
            self.say(&engines::say(words));
            return vec![];
        };
        let text = self.chat_text().unwrap_or_default();
        let mut attempt = followup::add_cline_message(&text, &message, &self.send.fence);
        if attempt == Err(followup::AddRefused::NonceInMessage) {
            // The message holds this draft's random word: another word closes no fence of it.
            self.send.fence = crate::start::new_nonce();
            attempt = followup::add_cline_message(&text, &message, &self.send.fence);
        }
        match attempt {
            Ok(boxed) => {
                self.set_chat_text(&boxed);
                self.send.final_text = Some(message);
                self.say(&engines::say(
                    "added {engine}'s last message at the end of the draft (untrusted, cut to 1,500 characters)",
                ));
            }
            Err(followup::AddRefused::Already) => {
                self.say(&engines::say(
                    "{engine}'s last message is already in the draft",
                ));
            }
            Err(_) => self.say(&engines::say("{engine} left no last message to add")),
        }
        vec![]
    }

    /// The answer to the look for a follow-up that is already going: none starts the run, one is offered, a failure asks again.
    fn follow_up_found(
        &mut self,
        source: &str,
        result: Result<Option<(String, harness_protocol::goal::GoalStatus)>, String>,
    ) -> Vec<Effect> {
        if !self.send.looking || self.focused_run() != Some(source) {
            return vec![];
        }
        self.send.looking = false;
        match result {
            Ok(None) => {
                let text = self.chat_text().unwrap_or_default();
                if text.trim().is_empty() {
                    self.say("write the text for the new run first");
                    return vec![];
                }
                self.send.confirm = None;
                self.panel = None;
                self.say("starting the follow-up run");
                vec![Effect::Ext(self.follow_up_job(source.to_string(), text))]
            }
            Ok(Some((existing, status))) => {
                let word = serde_json::to_value(status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                self.panel = Some(Panel::not_run(
                    "Follow-up",
                    &format!(
                        "{}\nRun {} is {}. Enter opens it \u{b7} Esc keeps your text, and the start comes back for you to confirm.",
                        followup::RUNNING_OFFER,
                        ham_core::sanitize::sanitize_line(&existing),
                        word.replace('_', " ")
                    ),
                ));
                self.send.confirm = Some(Confirm::FollowUpRunning {
                    existing,
                    shown_ms: self.key_ms,
                });
                vec![]
            }
            Err(why) => {
                self.panel = Some(Panel::not_run(
                    "Follow-up",
                    &format!(
                        "could not look for a follow-up that is already running ({}). Enter starts it anyway \u{b7} Esc keeps editing",
                        ham_core::sanitize::sanitize_line(&why)
                    ),
                ));
                self.send.confirm = Some(Confirm::FollowUp {
                    shown_ms: self.key_ms,
                    checked: true,
                });
                vec![]
            }
        }
    }

    /// The answer to the start of a follow-up.
    fn follow_up_done(&mut self, source: &str, result: Result<String, String>) -> Vec<Effect> {
        match result {
            Ok(new_run) => {
                self.send.confirm = None;
                self.panel = None;
                if self.focused_run() == Some(source) {
                    self.open_run(&new_run)
                } else {
                    self.say(&format!("started the follow-up run {new_run}"));
                    vec![]
                }
            }
            Err(why) => {
                if why.contains("head-mismatch") {
                    // The delivery is another commit than the one shown: the shown one is dropped and asked for again, and the person
                    // confirms the new one.
                    self.send.head = None;
                    self.send.delivered = None;
                    self.send.lookup = HeadLookup::NotAsked;
                }
                self.say(&format!(
                    "the follow-up run did not start: {}; your text is kept",
                    ham_core::sanitize::sanitize_line(&why)
                ));
                vec![]
            }
        }
    }

    /// Enter on a finished run: the first Enter prepares the follow-up in the box and shows its confirmation, the second starts it with the
    /// text that stands in the box then.
    fn follow_up_enter(&mut self, line: &str) -> Vec<Effect> {
        match self.send.confirm.clone() {
            Some(Confirm::FollowUp { shown_ms, checked }) => {
                if !self.calm || self.key_ms.saturating_sub(shown_ms) < CONFIRM_DELAY_MS {
                    return vec![];
                }
                if self.panel.is_none() {
                    // The confirmation was dismissed: it is asked again, nothing starts.
                    self.show_follow_up_confirm(checked);
                    return vec![];
                }
                if line.trim().is_empty() {
                    self.say("write the text for the new run first");
                    return vec![];
                }
                let run_id = self.focused_run().unwrap_or_default().to_string();
                if matches!(self.send.lookup, HeadLookup::Asking { .. }) {
                    // The commit the person is shown is the commit the run starts from: the start waits for the lookup.
                    self.say("looking up the delivered commit: press Enter again in a moment");
                    return vec![];
                }
                if self.send.looking {
                    // The look is out; its answer decides.
                    return vec![];
                }
                if !checked {
                    // Before anything starts, a follow-up from this run that is still going is looked for.
                    self.send.looking = true;
                    self.say("looking for a follow-up that is already running");
                    return vec![Effect::Ext(Job::FindFollowUp { run_id })];
                }
                self.send.confirm = None;
                self.panel = None;
                self.say("starting the follow-up run");
                vec![Effect::Ext(self.follow_up_job(run_id, line.to_string()))]
            }
            Some(Confirm::FollowUpRunning { existing, shown_ms }) => {
                if !self.calm || self.key_ms.saturating_sub(shown_ms) < CONFIRM_DELAY_MS {
                    return vec![];
                }
                if self.panel.is_none() {
                    // The offer was dismissed: the person has seen it, so the start is confirmed once more and then goes.
                    self.show_follow_up_confirm(true);
                    return vec![];
                }
                self.send.confirm = None;
                self.panel = None;
                self.open_run(&existing)
            }
            _ => {
                if let HeadLookup::Failed {
                    definitive: Some((code, _)),
                    ..
                } = &self.send.lookup
                    && code == "no-delivered-head"
                {
                    self.say("this run delivered no code, so there is nothing to follow up from. The text stays in the box.");
                    return vec![];
                }
                let said: Vec<String> = self
                    .send
                    .page
                    .as_ref()
                    .map(|p| {
                        p.messages
                            .iter()
                            .filter(|m| m.state == MessageState::Delivered)
                            .map(|m| m.text.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                self.send.fence = crate::start::new_nonce();
                let inputs = self.digest_inputs(&said);
                let d = followup::digest(&inputs);
                let text = followup::compose(line, &d, &inputs);
                self.set_chat_text(&text);
                // The settings of the engine file as it is now are asked for the confirmation's line about changed settings.
                self.send.engine_now = None;
                self.show_follow_up_confirm(false);
                vec![Effect::Ext(Job::EngineSettings)]
            }
        }
    }

    // ---- the retry of a blocked run ----------------------------------------------------------------------------------

    /// Whether the run on screen ended blocked (the box offers the retry, and `c`, `/cancel` and the answer keys have nothing to act on).
    pub(crate) fn run_is_blocked(&self) -> bool {
        self.send_plan() == Plan::Retry
    }

    /// Shows the offer to start the blocked run again and waits for the second Enter (typing mode) or the second `r` (browse mode): what
    /// starts, the blocked run's target when its detail has arrived, and that the text in the box is kept and not sent. Nothing starts here.
    pub(crate) fn retry_offer(&mut self) -> Vec<Effect> {
        let run = self.focused_run().unwrap_or_default().to_string();
        let target = self
            .detail
            .as_ref()
            .map(|d| ham_core::run_index::target_text(&d.summary.target));
        let has_text = self.chat_text().is_some_and(|t| !t.trim().is_empty());
        self.panel = Some(Panel::info(
            retry::CONFIRM_TITLE,
            &retry::confirmation(&run, target.as_deref(), has_text),
        ));
        self.send.confirm = Some(Confirm::Retry {
            shown_ms: self.key_ms,
        });
        vec![]
    }

    /// `/retry`: the offer on a blocked run, the words of the refusal on any other. The line was sent, so the box is emptied.
    pub(crate) fn retry_word(&mut self, args: &[String]) -> Vec<Effect> {
        self.clear_chat();
        if !args.is_empty() {
            self.panel = Some(Panel::not_run(
                "ham /retry",
                "usage: /retry\n/retry starts the blocked run on screen again with the request it was started with.",
            ));
            return vec![];
        }
        if !self.run_is_blocked() {
            self.panel = Some(Panel::not_run("Nothing was sent", retry::NOT_BLOCKED));
            return vec![];
        }
        self.retry_offer()
    }

    /// `r` in browse mode (the key table's `Action::Retry`): the offer on a blocked run; on any other run `r` opens the Runs list, as it did.
    pub(crate) fn retry_action(&mut self) -> Vec<Effect> {
        if self.run_is_blocked() {
            return self.retry_offer();
        }
        self.screen = Screen::Runs;
        vec![Effect::LoadRuns]
    }

    /// A key while the offer waits. Enter (either mode) or `r` (browse mode) 300 ms or more after the offer started the retry; an earlier one
    /// is ignored and the offer stays. Esc withdraws it and the text stays. Any other key withdraws it and is the screen's own.
    fn retry_confirm_key(
        &mut self,
        key: &KeyEvent,
        shown_ms: u64,
        browse: bool,
    ) -> Option<Vec<Effect>> {
        // A billable start needs the plain Enter: Shift-Enter (a new line) and Alt-Enter (the literal send) are keys of the box. In browse mode
        // Esc and `r` arriving together read as Alt-`r`, which is `r`.
        let confirms = match key.code {
            KeyCode::Enter => key.modifiers.is_empty(),
            KeyCode::Char('r') => {
                browse && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::ALT)
            }
            _ => false,
        };
        if confirms {
            if !self.calm || self.key_ms.saturating_sub(shown_ms) < CONFIRM_DELAY_MS {
                return Some(vec![]);
            }
            return Some(self.retry_start());
        }
        self.send.confirm = None;
        self.panel = None;
        (key.code == KeyCode::Esc).then(Vec::new)
    }

    /// The second Enter or `r`: the run is checked once more (it may have changed under the offer), and the service is asked to start it.
    fn retry_start(&mut self) -> Vec<Effect> {
        self.send.confirm = None;
        self.panel = None;
        if !self.run_is_blocked() {
            self.say(
                "the run changed while you looked: it is not blocked now, so nothing was started",
            );
            return vec![];
        }
        if self.send.retrying {
            self.say("the retry is being started: wait for the answer");
            return vec![];
        }
        self.send.retrying = true;
        self.say("starting the retry");
        let run_id = self.focused_run().unwrap_or_default().to_string();
        vec![Effect::Ext(Job::RetryRun { run_id })]
    }

    /// The answer to the retry. The new run opens when the blocked run is still on screen, with the text that was never sent in its box; a
    /// refusal leaves the run, the text and the service's words in a notice.
    fn retry_done(&mut self, blocked: &str, result: Result<String, String>) -> Vec<Effect> {
        // The call is over whatever is on screen now; the flag only stops a second press while it is out.
        self.send.retrying = false;
        match result {
            Ok(new_run) => {
                if self.focused_run() != Some(blocked) {
                    self.say(&retry::started(blocked, &new_run, false));
                    return vec![];
                }
                let text = self.chat_text().unwrap_or_default();
                let kept = !text.trim().is_empty();
                let effects = self.open_run(&new_run);
                if kept {
                    self.set_chat_text(&text);
                }
                self.say(&retry::started(blocked, &new_run, kept));
                effects
            }
            Err(why) => {
                if self.focused_run() == Some(blocked) {
                    self.say(&format!(
                        "the retry did not start: {}; your text is kept",
                        ham_core::sanitize::sanitize_line(&why)
                    ));
                }
                vec![]
            }
        }
    }
}
