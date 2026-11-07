//! The "Cline needs you" card: what the client says about a run that waits for the user.
//!
//! The question is the `needs-input` event of the direct agent; its answer and its end are the `input-received`, `input-ignored`,
//! `input-closed` and `new-session` events. `harness_protocol::direct::fold_input` folds them (one fold for the client and
//! control-api); [`build`] sanitizes the result into the [`InputView`] a [`crate::state::RunView`] carries. Every string that came from
//! the agent's side passes `sanitize_line` (Cline's last words pass `sanitize_block`). The plain words for each reason code are
//! [`reason_words`]. Nothing here judges the work: the card says what happened to the run.

use crate::models;
use crate::sanitize::{sanitize_block, sanitize_line};
use crate::tables::group_digits;
use harness_protocol::common::parse_utc_unix;
use harness_protocol::direct::{
    InstructAction, MAX_INPUT_TEXT_BYTES, NeedsInputEvidence, NeedsInputModel, NeedsInputPayload,
    fold_input,
};
use harness_protocol::envelope::Envelope;

/// Lines and columns of Cline's last words on the card. 64 columns plus the label "Last words: " fill the 76 columns of the card.
pub const LAST_WORDS_LINES: usize = 12;
pub const LAST_WORDS_COLS: usize = 64;
/// The width of a card line: `ham watch` indents it by two and stays within 78 columns, the TUI panel inside its border.
pub const CARD_COLS: usize = 76;

/// The title and the explanation of one reason code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReasonWords {
    /// What happened, in one phrase with no full stop.
    pub title: &'static str,
    /// What it means and what the user can do, as one or two sentences.
    pub explain: &'static str,
}

/// The words of a reason code (the snake_case wire word); `None` for a code this client does not know, and the caller then shows the
/// event's own `title`.
pub fn reason_words(reason: &str) -> Option<ReasonWords> {
    REASONS
        .iter()
        .find(|(code, _)| *code == reason)
        .map(|(_, w)| *w)
}

const fn words(title: &'static str, explain: &'static str) -> ReasonWords {
    ReasonWords { title, explain }
}

const REASONS: [(&str, ReasonWords); 14] = [
    (
        "engine_stop",
        words(
            "Cline stopped its own prompt",
            "Cline ended the prompt itself, twice. Its guard stops after 5 identical tool calls; its mistake limit stops after 6 failed turns. The provider is fine. After this stop, the same chat stops again after one identical call. A fresh session starts both counts from zero.",
        ),
    ),
    (
        "no_changes",
        words(
            "Cline ended without changing files",
            "Cline ended normally with a message, but no file differs from the start. Read its message: it may answer a question or give up on an impossible task.",
        ),
    ),
    (
        "no_message",
        words(
            "Cline sent an empty message",
            "Cline ended its turn with an empty message. You can send another prompt.",
        ),
    ),
    (
        "output_limit",
        words(
            "The model reached its output limit",
            "The model reached its maximum output length for one answer. You can send another prompt.",
        ),
    ),
    (
        "request_limit",
        words(
            "Cline reached its request limit",
            "Cline reached its model-request limit for one prompt. You can start a fresh session.",
        ),
    ),
    (
        "refused",
        words(
            "The answer was refused or blocked",
            "The model refused, or the provider's filter blocked the answer. You can change your prompt.",
        ),
    ),
    (
        "replay_rejected",
        words(
            "The provider rejected the saved conversation",
            "The harness already tried a fresh session once. More prompts in the same session fail the same way; choose a fresh session.",
        ),
    ),
    (
        "context_full",
        words(
            "The conversation exceeds the context window",
            "The conversation no longer fits the model's context window. The harness already tried a fresh session once; you can request another.",
        ),
    ),
    (
        "provider_error",
        words(
            "The model provider failed",
            "Retries and route switches could not fix the provider failure. You can finish with the existing work.",
        ),
    ),
    (
        "cap_hit",
        words(
            "Automatic recovery reached its prompt cap",
            "The harness stopped after 12 prompts in a row without a new tool call or changed file. You can supply another prompt.",
        ),
    ),
    (
        "engine_died",
        words(
            "The Cline process stopped",
            "The automatic fresh session did not help. Continuing is impossible; a fresh session starts a new process on the same files.",
        ),
    ),
    (
        "silence",
        words(
            "Cline stopped sending output",
            "Cline sent no output for 15 minutes, again after one resume. The process may be stuck; a fresh session starts a new process on the same files.",
        ),
    ),
    (
        "other_stop",
        words(
            "Cline gave an unknown stop reason",
            "The harness does not know Cline's stop reason. You can finish with the existing work.",
        ),
    ),
    (
        "provider_outage",
        words(
            "Every model route is down",
            "All routes have been down for 30 minutes. No answer is needed; probing continues until the time limit. `ham cancel` stops the run and delivers what exists.",
        ),
    ),
];

/// What the run waits for, or the notice it raised: a [`NeedsInputPayload`] with every string sanitized.
#[derive(Clone, Debug, PartialEq)]
pub struct InputRequest {
    /// The id of the question: the answer names it, and it is the event id of the `needs-input` event.
    pub request_id: String,
    /// The question's number in this run (1 for the first).
    pub n: u32,
    /// The reason code (`engine_stop`, ...); an unknown code is shown with the event's own title.
    pub reason: String,
    /// The headline: the event's title, else the title of the reason's words.
    pub title: String,
    /// The explanation: [`reason_words`]`.explain` for a known code, else the title.
    pub words: String,
    /// The wire word of how Cline's prompt ended, when the event names it.
    pub stop_outcome: Option<String>,
    pub evidence: NeedsInputEvidence,
    /// Cline's last words: at most [`LAST_WORDS_LINES`] lines of at most [`LAST_WORDS_COLS`] columns joined with `\n`.
    pub last_message: Option<String>,
    /// The first changed files.
    pub files: Vec<String>,
    pub files_total: u32,
    /// False only for the provider-outage notice: no keys, no window.
    pub blocking: bool,
    pub options: Vec<InstructAction>,
    /// The routes `try-model` may name.
    pub models: Vec<NeedsInputModel>,
    pub question: Option<String>,
    pub recommended: Option<InstructAction>,
    pub can_continue: bool,
    pub wait_s: u32,
    pub expires_at: Option<String>,
    /// Seconds left to answer when the view was built with a clock; [`InputRequest::secs_left_at`] for a later moment.
    pub secs_left: Option<i64>,
}

/// How an earlier question ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputOutcome {
    pub request_id: String,
    pub n: u32,
    pub reason: String,
    /// `answered:<action>`, `expired`, `cancelled`, `deadline`, `finish`, `superseded`, `recovered` or `ended`.
    pub how: String,
    pub ts: String,
    /// [`outcome_phrase`] of `how`.
    pub phrase: String,
}

/// The question the run waits on and the last few that ended.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputView {
    pub open: Option<InputRequest>,
    /// Oldest first, at most the last 8.
    pub history: Vec<InputOutcome>,
}

impl InputView {
    /// The open question that waits for an answer (not the outage notice).
    pub fn waiting(&self) -> Option<&InputRequest> {
        self.open.as_ref().filter(|r| r.blocking)
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_none() && self.history.is_empty()
    }
}

/// A fresh Cline session started on the same files: the dashboard says "restarted from the work tree".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartNote {
    /// The session's number (2 for the first fresh session).
    pub session: u32,
    /// `harness`, `user` or `analyzer`.
    pub trigger: String,
    pub reason: Option<String>,
}

impl RestartNote {
    /// `Session 2 \u{b7} restarted from the work tree`, the line of the dashboard's status block.
    pub fn line(&self) -> String {
        format!(
            "Session {} \u{b7} restarted from the work tree",
            self.session
        )
    }

    /// `restarted from the work tree: session 2`, the line of `ham watch`.
    pub fn watch_line(&self) -> String {
        format!("restarted from the work tree: session {}", self.session)
    }
}

/// What [`build`] reads out of a run's events.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Built {
    pub input: InputView,
    /// 1, plus one for every fresh session.
    pub sessions: u32,
    pub restarted: Option<RestartNote>,
}

/// Folds the accepted events (in event-id order) into the input state. `now` is Unix seconds on the client's clock, for `secs_left`;
/// `None` leaves it unset.
pub fn build<'a>(events: impl IntoIterator<Item = &'a Envelope>, now: Option<i64>) -> Built {
    let fold = fold_input(events);
    let open = fold
        .open
        .as_ref()
        .map(|o| InputRequest::of(&o.payload, now));
    let history = fold
        .closed
        .iter()
        .map(|c| InputOutcome {
            request_id: sanitize_line(&c.request_id),
            n: c.n,
            reason: sanitize_line(&c.reason),
            how: sanitize_line(&c.how),
            ts: sanitize_line(&c.ts),
            phrase: outcome_phrase(&sanitize_line(&c.how)),
        })
        .collect();
    let restarted = fold.last_session.as_ref().map(|s| RestartNote {
        session: s.session,
        trigger: sanitize_line(&s.trigger),
        reason: Some(sanitize_line(&s.reason)),
    });
    Built {
        input: InputView { open, history },
        sessions: fold.sessions.max(1),
        restarted,
    }
}

impl InputRequest {
    /// The sanitized request of an event payload.
    pub fn of(p: &NeedsInputPayload, now: Option<i64>) -> Self {
        let reason = sanitize_line(&p.reason);
        let known = reason_words(&reason);
        let event_title = sanitize_line(&p.title);
        let title = if event_title.trim().is_empty() {
            known.map_or_else(String::new, |w| w.title.to_string())
        } else {
            event_title
        };
        let words = known.map_or_else(|| title.clone(), |w| w.explain.to_string());
        let mut evidence = p.evidence.clone();
        evidence.last_error = evidence.last_error.as_deref().map(sanitize_line);
        let lines = p
            .last_message
            .as_deref()
            .map(|m| sanitize_block(m, LAST_WORDS_LINES, LAST_WORDS_COLS))
            .filter(|l| !l.is_empty());
        let mut r = Self {
            request_id: sanitize_line(&p.request_id),
            n: p.n,
            reason,
            title,
            words,
            stop_outcome: p.stop_outcome.as_deref().map(sanitize_line),
            evidence,
            last_message: lines.map(|l| l.join("\n")),
            files: p.files.iter().map(|f| sanitize_line(f)).collect(),
            files_total: p.files_total,
            blocking: p.blocking,
            options: p.options.clone(),
            models: p
                .models
                .iter()
                .map(|m| NeedsInputModel {
                    id: sanitize_line(&m.id),
                    name: sanitize_line(&m.name),
                    context_tokens: m.context_tokens,
                })
                .collect(),
            question: p.question.as_deref().map(sanitize_line),
            recommended: p.recommended,
            can_continue: p.can_continue,
            wait_s: p.wait_s,
            expires_at: p.expires_at.as_deref().map(sanitize_line),
            secs_left: None,
        };
        r.secs_left = now.and_then(|n| r.secs_left_at(n));
        r
    }

    /// Seconds from `now` (Unix seconds) to the end of the answer window, at least 0; `None` for a notice or an unreadable time.
    pub fn secs_left_at(&self, now: i64) -> Option<i64> {
        let end = parse_utc_unix(self.expires_at.as_deref()?)?;
        Some((end - now).max(0))
    }

    /// `about 27 min left to answer`, from [`InputRequest::secs_left`].
    pub fn left_text(&self) -> Option<String> {
        self.secs_left.map(left_text)
    }

    /// The headline with a full stop: `Cline stopped its own prompt.`
    pub fn headline(&self) -> String {
        format!("{}.", self.title.trim_end_matches('.'))
    }

    /// `41 tool calls, 3 files changed`.
    pub fn numbers(&self) -> String {
        let e = &self.evidence;
        let (calls, files) = (e.tool_calls, u64::from(e.files_changed));
        format!(
            "{calls} tool call{}, {files} file{} changed",
            if calls == 1 { "" } else { "s" },
            if files == 1 { "" } else { "s" },
        )
    }

    /// Cline's last words as lines (at most [`LAST_WORDS_LINES`]).
    pub fn last_words(&self) -> Vec<String> {
        self.last_message
            .as_deref()
            .map(|m| m.split('\n').map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// `Files: PROGRESS.md src/app.py (+3 more)`, `None` when no file changed.
    pub fn files_line(&self) -> Option<String> {
        if self.files.is_empty() {
            return None;
        }
        let more = (self.files_total as usize).saturating_sub(self.files.len());
        let mut line = format!("Files: {}", self.files.join(" "));
        if more > 0 {
            line.push_str(&format!(" (+{more} more)"));
        }
        Some(line)
    }

    /// The card without its keys: the headline, the explanation, the numbers, the last error, Cline's last words and the files.
    pub fn card_lines(&self) -> Vec<String> {
        let wrap = |text: &str| sanitize_block(text, LAST_WORDS_LINES, CARD_COLS);
        let mut out = vec![self.headline()];
        out.extend(wrap(&self.words));
        out.extend(wrap(&format!("{}.", self.numbers())));
        if let Some(q) = &self.question {
            out.extend(wrap(q));
        }
        if let Some(e) = self
            .evidence
            .last_error
            .as_deref()
            .filter(|e| !e.is_empty())
        {
            out.extend(wrap(&format!("Error: {e}")));
        }
        let words = self.last_words();
        if let Some((first, rest)) = words.split_first() {
            out.push(format!("Last words: {first}"));
            out.extend(rest.iter().map(|l| format!("            {l}")));
        }
        if let Some(files) = self.files_line() {
            out.extend(wrap(&files));
        }
        out
    }

    /// Whether `action` is one the question offers now: it is in `options`, and a message or a model switch also needs a live session.
    pub fn offers(&self, action: InstructAction) -> bool {
        self.options.contains(&action)
            && (self.can_continue
                || !matches!(
                    action,
                    InstructAction::ContinueWithPrompt | InstructAction::TryModel
                ))
    }

    /// The commands that answer this question, one per offered action.
    pub fn commands(&self, run_id: &str) -> Vec<String> {
        let mut out = Vec::new();
        if self.offers(InstructAction::ContinueWithPrompt) {
            out.push(format!(
                "ham say {run_id} '<message>'            send a message to the same Cline session"
            ));
        }
        if self.offers(InstructAction::NewSessionWithPrompt) {
            out.push(format!(
                "ham new-session {run_id} ['<message>']  start a fresh Cline session on the same files"
            ));
        }
        if self.offers(InstructAction::TryModel) && !self.models.is_empty() {
            let names: Vec<String> = self.models.iter().map(|m| model_handle(&m.id)).collect();
            out.push(format!(
                "ham try-model {run_id} <model> ['<message>']  switch model ({})",
                names.join(", ")
            ));
        }
        if self.offers(InstructAction::Finish) {
            out.push(format!(
                "ham finish {run_id}                    finish with what exists"
            ));
        }
        out
    }
}

/// The short name for a route id: the alias of the listed model it belongs to, else the id itself.
pub fn model_handle(id: &str) -> String {
    let list = models::builtin();
    list.find(id)
        .or_else(|| list.find_route(id))
        .and_then(|m| m.alias.clone())
        .unwrap_or_else(|| id.to_string())
}

/// `about 27 min left to answer`.
pub fn left_text(secs: i64) -> String {
    match secs {
        s if s <= 0 => "no time left to answer".to_string(),
        s if s < 60 => "less than a minute left to answer".to_string(),
        s => format!("about {} min left to answer", (s + 30) / 60),
    }
}

/// How a question ended, in words that read after "Its last question was": the `how` of an [`InputOutcome`].
pub fn outcome_phrase(how: &str) -> String {
    match how {
        "answered:continue_with_prompt" => "answered with a message",
        "answered:new_session_with_prompt" => "answered with a fresh session",
        "answered:try_model" => "answered with another model",
        "finish" | "answered:finish" => "answered with finish",
        "expired" => "closed when nobody answered in time",
        "cancelled" => "closed by your cancel",
        "deadline" => "closed at the run's time limit",
        "superseded" => "replaced by a newer question",
        "recovered" => "closed when a model route answered again",
        "ended" => "closed when the run ended",
        other => return format!("closed ({other})"),
    }
    .to_string()
}

/// Why Cline did not take an answer (`input-ignored`'s `why`), in plain words.
pub fn ignored_phrase(why: &str) -> String {
    match why {
        "window-closed" => "the answer window is closed",
        "unknown-request" => "that question is not open",
        "already-answered" => "that question was already answered",
        "session-dead" => "its session is gone",
        "bad-action" => "it cannot do that now",
        "bad-model" => "that model is not one of this run's routes",
        "limit" => "the limit of questions or fresh sessions is reached",
        other => return format!("refused ({other})"),
    }
    .to_string()
}

/// The text of an answer is some text of at most 262,144 bytes (the server's rule; there is no limit in characters), checked before anything is sent.
pub fn check_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("the message is empty; write some text for Cline".to_string());
    }
    if text.len() > MAX_INPUT_TEXT_BYTES {
        return Err(format!(
            "the message is {} bytes; Cline takes at most {} bytes (256 KiB)",
            group_digits(text.len() as u64),
            group_digits(MAX_INPUT_TEXT_BYTES as u64),
        ));
    }
    Ok(())
}

/// The route of `offered` that the user's `typed` model names: an offered id exactly, else the listed model an alias or id names, on
/// whichever of its routes the question offers (its own first, then the alternates). The error says what to type instead.
pub fn resolve_offered<'a>(
    typed: &str,
    offered: &'a [NeedsInputModel],
) -> Result<&'a NeedsInputModel, String> {
    let t = typed.trim();
    let choices = || {
        offered
            .iter()
            .map(|m| format!("{} ({})", model_handle(&m.id), m.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if offered.is_empty() {
        return Err("this question offers no other model to switch to".into());
    }
    if let Some(m) = offered.iter().find(|m| m.id == t) {
        return Ok(m);
    }
    let list = models::builtin();
    let Some(row) = list.find(t).or_else(|| list.find_route(t)) else {
        return Err(format!(
            "{t:?} is not a model this run can switch to; choose one of: {}",
            choices()
        ));
    };
    std::iter::once(row.id.as_str())
        .chain(row.alternate_routes.iter().map(|a| a.id.as_str()))
        .find_map(|id| offered.iter().find(|m| m.id == id))
        .ok_or_else(|| {
            format!(
                "{} is not one this run can switch to now; choose one of: {}",
                row.name,
                choices()
            )
        })
}
