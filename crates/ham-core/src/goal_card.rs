//! The card of a goal-loop brake.
//!
//! agentd's goal module ends a run with one of six stopping rules and writes a `turn.progress` event of kind `goal-loop`, phase `brake`, with
//! the `rule` and the sentence ("The harness ended the run: ..."); the round events before it carry the `rounds` and the `status` of the last
//! one. [`from_events`] reads those events and nothing else and builds the [`GoalBrakeCard`] that `RunView::goal_brake` carries: which rule, how
//! far the loop got, Cline's own `BLOCKED:` line when there is one, and the command that starts a follow-up. The dashboard draws it as a card
//! and `ham watch` prints it once.
//!
//! The run is over when the card appears, so it asks for no answer in a window: it names the next instruction (`ham continue`). No line says
//! that the work was verified, tested, gated or evaluated; the label of the run stays the one of `run.completed`. Every string that
//! came from the agent's side passes the sanitizer before it reaches a screen.

use crate::notice::{NOTICE_MAX_CHARS, cut, goal_blocked_line};
use crate::redact::redact_tokens;
use crate::sanitize::{display_line, sanitize_block};
use harness_protocol::envelope::{Envelope, EventType, Payload};
use serde_json::Value;

/// Where a brake quotes the engine's last line (the sentence before it is the headline).
const QUOTE: &str = "Its words:";

/// What the card of a brake holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalBrakeCard {
    /// The event id of the brake event: the card is shown once per brake.
    pub request_id: String,
    /// The stopping rule's word (`blocked-twice`, `text-only-stall`, `no-progress`, `round-no-progress`, `dead-loop`, `unfinished-claim`,
    /// `goal-cleared`), as the
    /// event wrote it; empty when the event named none.
    pub rule: String,
    /// The rounds that ended, from the last `round` event.
    pub rounds: Option<u64>,
    /// The status of the last round that ended (`completed`, `aborted`, `failed` or `unknown`).
    pub status: Option<String>,
    /// The first sentence of the brake's words ("The harness ended the run: ...").
    pub headline: String,
    /// The engine's own `BLOCKED:` line, for the `blocked-twice` rule.
    pub blocked_line: Option<String>,
}

fn clean(text: &str) -> String {
    redact_tokens(&display_line(text))
}

fn inline(e: &Envelope) -> Option<&Value> {
    match &e.payload {
        Payload::Inline { value } => Some(value),
        Payload::ClaimCheck { .. } => None,
    }
}

fn goal_loop(e: &Envelope) -> Option<(&Value, &str)> {
    if e.event_type != EventType::TurnProgress {
        return None;
    }
    let v = inline(e)?;
    (v.get("kind")?.as_str()? == "goal-loop").then_some(())?;
    Some((v, v.get("phase")?.as_str()?))
}

/// The card of the last brake in `events` (in event-id order), or `None` for a run that no rule stopped.
pub fn from_events<'a>(events: impl IntoIterator<Item = &'a Envelope>) -> Option<GoalBrakeCard> {
    let mut brake: Option<&Envelope> = None;
    let mut rounds: Option<u64> = None;
    let mut status: Option<String> = None;
    for e in events {
        match goal_loop(e) {
            Some((_, "brake")) => brake = Some(e),
            Some((v, "round")) => {
                if let Some(n) = v.get("rounds").and_then(Value::as_u64) {
                    rounds = Some(n);
                    status = v
                        .get("status")
                        .and_then(Value::as_str)
                        .map(|s| clean(s).trim().to_string())
                        .filter(|s| !s.is_empty());
                }
            }
            _ => {}
        }
    }
    let brake = brake?;
    let (v, _) = goal_loop(brake)?;
    let rule = v
        .get("rule")
        .and_then(Value::as_str)
        .map(|r| clean(r).trim().to_string())
        .unwrap_or_default();
    let words = v.get("text").and_then(Value::as_str).unwrap_or("");
    let first = words.split(QUOTE).next().unwrap_or(words);
    let first = clean(first).trim().to_string();
    let headline = if !first.is_empty() {
        cut(&first, NOTICE_MAX_CHARS)
    } else if rule.is_empty() {
        "The harness ended the run.".to_string()
    } else {
        format!("The harness ended the run: rule {rule}.")
    };
    Some(GoalBrakeCard {
        request_id: brake.event_id.clone(),
        rule,
        rounds,
        status,
        headline,
        blocked_line: goal_blocked_line(brake),
    })
}

impl GoalBrakeCard {
    /// The border title of the card.
    pub fn title(&self) -> String {
        "goal loop stopped by the harness".to_string()
    }

    /// What the rule means, in one sentence. A rule this client has no words for is said so.
    pub fn explain(&self) -> String {
        if self.rule == "round-no-progress" {
            return crate::engines::say(
                "{engine} kept calling tools inside one round, but no file changed and no new text appeared, so the harness ended the run.",
            );
        }
        match self.rule.as_str() {
            "blocked-twice" => "Cline ended two rounds in a row with the same line that begins BLOCKED:, so the harness stopped the loop.",
            "text-only-stall" => "Several rounds in a row had no tool call and changed no file, so the harness stopped the loop.",
            "no-progress" => "Several rounds in a row changed no file, so the harness stopped the loop.",
            "dead-loop" => "The goal loop stopped and the harness's continues did not start it again.",
            "unfinished-claim" => "Cline called its goal-complete tool, but the round did not end with a message.",
            "goal-cleared" => "The goal left the loop's state and Cline never completed it.",
            _ => "The harness stopped the goal loop by a rule this client has no words for.",
        }
        .to_string()
    }

    /// The counter line, when the round events gave one.
    fn counter(&self) -> Option<String> {
        let n = self.rounds?;
        let noun = if n == 1 { "round" } else { "rounds" };
        let last = match self.status.as_deref() {
            Some(s @ ("completed" | "aborted" | "failed")) => format!("the last one {s}"),
            _ => "the last one's status is unknown".to_string(),
        };
        Some(format!("The goal loop ran {n} {noun}; {last}."))
    }

    /// The lines of the card for a screen `width` columns wide: the headline, what the rule means, how far the loop got, Cline's own line,
    /// and the command that starts a follow-up from the code the run delivered.
    pub fn lines(&self, run_id: &str, width: usize) -> Vec<String> {
        let w = width.max(1);
        let wrap = |text: &str| sanitize_block(text, 8, w);
        let mut out = wrap(&self.headline);
        out.extend(wrap(&self.explain()));
        if let Some(c) = self.counter() {
            out.extend(wrap(&c));
        }
        if let Some(b) = &self.blocked_line {
            out.extend(wrap(&format!("Last words: {b}")));
        }
        out.extend(wrap(
            "Cline decides when its work is done; the harness ended this run by one of its stopping rules.",
        ));
        out.extend(wrap("To go on from the code this run delivered:"));
        out.extend(wrap(&format!(
            "ham continue {run_id} '<what Cline should do next>'"
        )));
        out
    }
}
