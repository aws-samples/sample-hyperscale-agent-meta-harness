//! The wait for the person's answer.
//! A stage has ended in a way agentd cannot mend and a `needs-input` question is open. This module is the one place that decides what
//! happens to it: it selects, in ONE task, over the instruction channel, the user's cancel, the engine's pipe and the window's expiry,
//! so there is exactly one winner for every race (answer against expiry, cancel against answer, an engine that dies during the wait).
//!
//! The pure rules (who may answer, with what, how often) are [`Window`]'s; this module owns the clock, the channels and the events:
//! `input-received` when an answer is taken, `input-ignored` with the reason when one is not, and the payload words of the closing
//! events the driver writes. It never judges the work, and it sends nothing to the engine.

use crate::acp::AcpClient;
use crate::direct::collector::DirectCollector;
use crate::direct::input::{Accepted, IgnoreWhy, Verdict, Window};
use crate::events::{EventRecorder, Instruct};
use harness_protocol::direct::{
    DIRECT_TURN, InputClosedPayload, InputIgnoredPayload, InputReceivedPayload, InstructAction,
    NeedsInputModel, PROGRESS_INPUT_CLOSED, PROGRESS_INPUT_IGNORED, PROGRESS_INPUT_RECEIVED,
};
use harness_protocol::envelope::EventType;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// How a question ended.
#[derive(Debug, PartialEq, Eq)]
pub enum WaitEnd {
    /// An answer was taken (the acknowledgement event is written).
    Answered(Accepted),
    /// The window ran out with no answer.
    Expired,
    /// The user's cancel (alone or in the same batch as an answer; the cancel wins).
    Cancelled,
    /// The run's own deadline came first.
    Deadline,
    /// The engine's output closed while the question was open.
    EngineDied,
}

/// What the wait needs from the run.
#[derive(Debug)]
pub struct WaitCtx<'a> {
    pub rec: &'a EventRecorder,
    pub instructions: &'a mut mpsc::Receiver<Instruct>,
    pub cancel: &'a mut watch::Receiver<bool>,
    pub request_id: &'a str,
    /// How the engine is named in the texts of the events ([`crate::engine::direct::engine_label`]).
    pub who: &'a str,
    /// The ids of the questions an accepted answer already closed.
    pub answered: &'a [String],
    /// The routes the question offered (their names are in the acknowledgement of a model switch).
    pub models: &'a [NeedsInputModel],
    pub opened: Instant,
    /// False when the engine's output is already closed: there is nothing to read.
    pub pipe_open: bool,
    /// The run's absolute deadline.
    pub deadline: Instant,
}

/// The plain words after `Your message did not reach the engine:`.
pub fn ignore_words(why: IgnoreWhy) -> &'static str {
    match why {
        IgnoreWhy::WindowClosed => "the window is closed",
        IgnoreWhy::UnknownRequest => "that is not the open question",
        IgnoreWhy::AlreadyAnswered => "the question was already answered",
        IgnoreWhy::SessionDead => "the session cannot take it",
        IgnoreWhy::BadAction => "that action is not offered now",
        IgnoreWhy::BadModel => "that model is not one of this run's routes",
        IgnoreWhy::Limit => "this run reached its limit for that action",
    }
}

/// `input-ignored`: an answer that was not taken, and why.
pub fn ignored_event(request_id: &str, why: IgnoreWhy, who: &str) -> Value {
    let p = InputIgnoredPayload {
        turn: DIRECT_TURN,
        kind: PROGRESS_INPUT_IGNORED.to_string(),
        text: format!("Your message did not reach {who}: {}.", ignore_words(why)),
        request_id: request_id.to_string(),
        why: why.as_str().to_string(),
    };
    serde_json::to_value(&p).unwrap_or(Value::Null)
}

/// `input-received`: an answer that was taken. `waited_s` is how long the question had been open.
pub fn received_event(
    a: &Accepted,
    request_id: &str,
    waited_s: u64,
    models: &[NeedsInputModel],
    who: &str,
) -> Value {
    let text = match a.action {
        InstructAction::ContinueWithPrompt => {
            format!("Your message reached {who} (same session).")
        }
        InstructAction::NewSessionWithPrompt => {
            "Starting a fresh session on the same files.".to_string()
        }
        InstructAction::TryModel => {
            let name = a
                .model
                .as_deref()
                .and_then(|id| models.iter().find(|m| m.id == id))
                .map(|m| m.name.as_str())
                .or(a.model.as_deref())
                .unwrap_or("the other model");
            format!("Switching to {name} and sending your message.")
        }
        InstructAction::Finish => "The run will finish with what exists.".to_string(),
    };
    let p = InputReceivedPayload {
        turn: DIRECT_TURN,
        kind: PROGRESS_INPUT_RECEIVED.to_string(),
        text,
        request_id: request_id.to_string(),
        action: a.action,
        model: a.model.clone(),
        origin: a.origin,
        text_bytes: a.text.as_ref().map_or(0, |t| t.len() as u64),
        waited_s,
    };
    serde_json::to_value(&p).unwrap_or(Value::Null)
}

/// `input-closed`: how a question ended, with the words the dashboard shows.
pub fn closed_event(request_id: &str, how: &str, waited_s: u64) -> Value {
    let mins = waited_s.div_ceil(60).max(1);
    let text = match how {
        "expired" => {
            format!("Nobody answered within {mins} min, so the run finishes with what exists.")
        }
        "cancelled" => "You stopped the run.".to_string(),
        "deadline" => "The run's time limit came first.".to_string(),
        "finish" => "You chose to finish.".to_string(),
        "superseded" => "The engine process stopped while the question was open.".to_string(),
        "recovered" => "A model route answered again.".to_string(),
        other => format!("The question closed: {other}."),
    };
    let p = InputClosedPayload {
        turn: DIRECT_TURN,
        kind: PROGRESS_INPUT_CLOSED.to_string(),
        text,
        request_id: request_id.to_string(),
        how: how.to_string(),
        waited_s,
    };
    serde_json::to_value(&p).unwrap_or(Value::Null)
}

fn progress(rec: &EventRecorder, value: Value) {
    rec.record(EventType::TurnProgress, value);
}

/// Waits until the question ends. Every answer is offered to the [`Window`], which decides; the first one it accepts ends the wait.
/// The user's cancel is looked at first, so it wins against an answer that arrives in the same batch (and every answer that was queued
/// behind it is acknowledged as ignored).
pub async fn wait_for_answer<R, W>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    ctx: WaitCtx<'_>,
    window: &mut Window,
) -> WaitEnd
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let WaitCtx {
        rec,
        instructions,
        cancel,
        request_id,
        who,
        answered,
        models,
        opened,
        pipe_open,
        deadline,
    } = ctx;
    let mut instructions_open = true;
    let mut cancel_open = true;
    loop {
        if *cancel.borrow() {
            while let Ok(i) = instructions.try_recv() {
                progress(
                    rec,
                    ignored_event(&i.request_id, IgnoreWhy::WindowClosed, who),
                );
            }
            return WaitEnd::Cancelled;
        }
        let now = Instant::now();
        if now >= deadline {
            return WaitEnd::Deadline;
        }
        tokio::select! {
            biased;
            changed = async {
                if cancel_open { cancel.changed().await } else { std::future::pending().await }
            } => {
                // A closed signal can never fire again: stop listening to it.
                if changed.is_err() {
                    cancel_open = false;
                }
            }
            next = async {
                if instructions_open { instructions.recv().await } else { std::future::pending().await }
            } => {
                let Some(i) = next else {
                    instructions_open = false;
                    continue;
                };
                // The cancel wins, also against an answer that came in the same batch as it.
                if i.cancelled_in_batch || *cancel.borrow() {
                    progress(rec, ignored_event(&i.request_id, IgnoreWhy::WindowClosed, who));
                    continue;
                }
                if answered.contains(&i.request_id) {
                    progress(rec, ignored_event(&i.request_id, IgnoreWhy::AlreadyAnswered, who));
                    continue;
                }
                match window.offer(&i, Instant::now()) {
                    Verdict::Accept(a) => {
                        let waited = Instant::now().saturating_duration_since(opened).as_secs();
                        progress(rec, received_event(&a, request_id, waited, models, who));
                        return WaitEnd::Answered(a);
                    }
                    Verdict::Ignore(why) => progress(rec, ignored_event(&i.request_id, why, who)),
                }
            }
            pumped = async {
                if pipe_open { client.pump_idle(collector).await } else { std::future::pending().await }
            } => {
                if pumped.is_err() {
                    return WaitEnd::EngineDied;
                }
            }
            () = tokio::time::sleep_until(window.expires_at()) => return WaitEnd::Expired,
            () = tokio::time::sleep_until(deadline) => return WaitEnd::Deadline,
        }
    }
}

/// Acknowledges, as ignored, every answer waiting in `rx`: a repeated answer is `already-answered` (its id is in `answered`), any other is
/// `unknown-request` while no question is open and `window-closed` once the run has ended. Returns how many there were.
fn acknowledge_waiting(
    rx: &mut mpsc::Receiver<Instruct>,
    answered: &[String],
    rec: &EventRecorder,
    who: &str,
    otherwise: IgnoreWhy,
) -> usize {
    let mut n = 0;
    while let Ok(i) = rx.try_recv() {
        let why = if answered.contains(&i.request_id) {
            IgnoreWhy::AlreadyAnswered
        } else {
            otherwise
        };
        progress(rec, ignored_event(&i.request_id, why, who));
        n += 1;
    }
    n
}

/// Answers that reach a run while no question is open (a second delivery of an answer, an answer for a question that closed).
pub fn drain_stale(
    rx: &mut mpsc::Receiver<Instruct>,
    answered: &[String],
    rec: &EventRecorder,
    who: &str,
) -> usize {
    acknowledge_waiting(rx, answered, rec, who, IgnoreWhy::UnknownRequest)
}

/// The answers that can still reach a run after its last stage ended (the end sequence takes seconds, and the events response keeps coming
/// until the final flush). The driver hands the channel back so the end sequence can say, before the final flush, that the run is over:
/// without it a person who answers late waits for an acknowledgement that never comes.
#[derive(Debug)]
pub struct LateAnswers {
    rx: mpsc::Receiver<Instruct>,
    answered: Vec<String>,
    who: String,
    rec: Arc<EventRecorder>,
}

impl LateAnswers {
    pub fn new(
        rx: mpsc::Receiver<Instruct>,
        answered: Vec<String>,
        who: String,
        rec: Arc<EventRecorder>,
    ) -> Self {
        Self {
            rx,
            answered,
            who,
            rec,
        }
    }

    /// Acknowledges every answer that arrived since the last call as ignored (`window-closed`; `already-answered` for a repeat).
    pub fn drain(&mut self) -> usize {
        acknowledge_waiting(
            &mut self.rx,
            &self.answered,
            &self.rec,
            &self.who,
            IgnoreWhy::WindowClosed,
        )
    }
}
