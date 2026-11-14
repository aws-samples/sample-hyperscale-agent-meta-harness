//! The harness notices of a run: one plain line for each thing agentd tells the event stream besides the conversation.
//!
//! agentd writes them as `turn.progress` events whose payload `kind` is `provider-retry`, `provider-switch`, `engine-stop`, `notify`,
//! `provider-wait` or `provider-outage`, as `permission.requested` events, and, for the
//! correction path, as `needs-input`, `input-received`, `input-ignored`, `input-closed` and `new-session`. [`notice_of`] reads
//! one event and returns its [`Notice`]; the run view lists the run notices and the chat view shows all of them. A line says what
//! happened to the run and never judges the work. Every line is one line, terminal safe ([`display_line`]), redacted and at most
//! [`NOTICE_MAX_CHARS`] characters.
//!
//! These events carry agent authority (agentd posts them from inside the microVM), so their text is as trusted as the "Cline needs
//! you" card built from the same stream; the numbers and words here come from fixed formats, and only an error's `detail` is free text.

use crate::input::InputRequest;
use crate::redact::redact_tokens;
use crate::render::model_name;
use crate::sanitize::display_line;
use harness_protocol::direct::{NeedsInputPayload, PROGRESS_INPUT_CLOSED, PROGRESS_INPUT_IGNORED};
use harness_protocol::envelope::{Envelope, EventType, Payload};
use serde_json::Value;

/// The longest notice line, in characters.
pub const NOTICE_MAX_CHARS: usize = 160;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    ProviderRetry,
    ProviderSwitch,
    EngineStop,
    Notify,
    ProviderWait,
    ProviderOutage,
    Permission,
    NeedsInput,
    InputReceived,
    InputIgnored,
    InputClosed,
    NewSession,
    /// The run is blocked or ended early (written by the chat view from the harness's own events).
    RunStatus,
    /// A line of the goal loop (`kind: goal-loop`): the loop is on, a continue was sent, Cline called its goal-complete tool.
    GoalLoop,
    /// The goal loop's round counter: one line that counts, so a new round event replaces the earlier one.
    GoalRound,
    /// A quiet note of the goal loop (off, fallback, rearm): drawn dim.
    GoalQuiet,
    /// The brake: the harness ended the run by one of its stopping rules.
    GoalBrake,
    /// The engine's own `BLOCKED:` line, shown apart from the brake line that quotes it.
    GoalBlocked,
    /// A `turn.progress` kind this client has no name for.
    Other,
}

impl NoticeKind {
    /// The kinds the run view lists in `RunView::notices`: what happens to the provider and the engine. The question and answer kinds
    /// have their card (`RunView::input`).
    pub fn is_run_notice(self) -> bool {
        matches!(
            self,
            Self::ProviderRetry
                | Self::ProviderSwitch
                | Self::EngineStop
                | Self::Notify
                | Self::ProviderWait
                | Self::ProviderOutage
                | Self::Permission
        )
    }
}

/// One notice line, with the event it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub event_id: String,
    pub ts: String,
    pub kind: NoticeKind,
    pub text: String,
}

fn clean(text: &str) -> String {
    redact_tokens(&display_line(text))
}

/// `text` cut to `max` characters with an ellipsis in place of the rest.
pub(crate) fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

fn string<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

fn number(v: &Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}

/// How a failure class reads in a sentence; a class this client does not know is shown as agentd wrote it.
fn class_words(class: &str) -> String {
    match class {
        "transient" => "temporary".into(),
        "permanent" => "permanent".into(),
        "unavailable" => "route unavailable".into(),
        other => clean(other),
    }
}

/// `detail` after a headline, cut so the whole line fits [`NOTICE_MAX_CHARS`]. Nothing when there is no detail.
fn with_detail(headline: String, detail: Option<&str>) -> String {
    let detail = detail.map(clean).filter(|d| !d.trim().is_empty());
    match detail {
        None => headline,
        Some(d) => {
            let room = NOTICE_MAX_CHARS.saturating_sub(headline.chars().count() + 2);
            if room < 8 {
                return headline;
            }
            format!("{headline}. {}", cut(&d, room))
        }
    }
}

fn retry(v: &Value) -> String {
    let class = class_words(string(v, "class").unwrap_or("failure"));
    let wait = number(v, "backoffS");
    let tries = match (number(v, "attempt"), number(v, "of"), wait) {
        (Some(a), Some(o), Some(w)) => format!("try {a} of {o} in {w} s"),
        (Some(a), Some(o), None) => format!("try {a} of {o} follows"),
        (_, _, Some(w)) => format!("another try in {w} s"),
        _ => "another try follows".into(),
    };
    with_detail(
        format!("Provider error ({class}): {tries}"),
        string(v, "detail"),
    )
}

fn switch(v: &Value) -> String {
    let n = number(v, "n").unwrap_or(1);
    let from = clean(string(v, "from").unwrap_or("?"));
    let to = clean(string(v, "to").unwrap_or("?"));
    // Two routes of one model have the same name: show the ids then.
    let (a, b) = {
        let (na, nb) = (model_name(&from), model_name(&to));
        if na == nb { (from, to) } else { (na, nb) }
    };
    let kind = string(v, "switchKind").unwrap_or("");
    let cause = match string(v, "class") {
        Some("transient") => ", after a temporary failure".to_string(),
        Some("permanent") => ", after a permanent failure".to_string(),
        Some("unavailable") => ", because the route was unavailable".to_string(),
        Some(other) => format!(", after a failure ({})", class_words(other)),
        None => String::new(),
    };
    match kind {
        "route" => format!("Switched route (switch {n}): {a} to {b}{cause}."),
        "model" => format!("Switched to another model (switch {n}): {a} to {b}{cause}."),
        "return" => format!("Back on the model you chose (switch {n}): {a} to {b}."),
        _ => format!("Switched (switch {n}): {a} to {b}{cause}."),
    }
}

fn engine_stop(v: &Value) -> String {
    let hint = string(v, "hint").unwrap_or("");
    let words = match hint {
        "loop-guard" => "repeated the same tool call".to_string(),
        "mistake-limit" => "too many failed turns".to_string(),
        "unexplained" => "no cause found".to_string(),
        "" => "no reason given".to_string(),
        other => clean(other),
    };
    let next = match (number(v, "attempt"), number(v, "of"), number(v, "backoffS")) {
        (Some(a), Some(o), Some(w)) => format!("try {a} of {o} follows in {w} s"),
        (_, _, Some(w)) => format!("another try follows in {w} s"),
        _ => "another try follows".to_string(),
    };
    format!("The engine stopped its own prompt ({words}): {next}.")
}

fn notify(v: &Value) -> String {
    match string(v, "reason").unwrap_or("") {
        "outage" => match number(v, "outageMinutes") {
            Some(m) => {
                format!("Every model route has been down for {m} min; the run keeps waiting.")
            }
            None => "Every model route is down; the run keeps waiting.".to_string(),
        },
        "history_unusable" => {
            "The provider rejects the saved conversation; a fresh session starts it over."
                .to_string()
        }
        // The service's own sentence, once per streak of copies that could not be saved.
        "copy_not_saved" => match string(v, "detail").map(clean).filter(|d| !d.is_empty()) {
            Some(detail) => detail,
            None => "A copy of the work could not be saved.".to_string(),
        },
        other => with_detail(
            format!("Notice from the run ({})", clean(other)),
            string(v, "detail"),
        ),
    }
}

fn provider_wait(v: &Value) -> String {
    let pass = number(v, "pass").map_or_else(String::new, |p| format!(" (pass {p})"));
    match number(v, "waitS") {
        Some(w) => format!("All model routes failed{pass}; waiting {w} s before the next try."),
        None => format!("All model routes failed{pass}; waiting before the next try."),
    }
}

fn provider_outage(v: &Value) -> String {
    let minutes = number(v, "outageMinutes").or_else(|| number(v, "waitedS").map(|s| s / 60));
    match minutes {
        Some(m) => format!(
            "Every model route has been down for {m} min; the run keeps trying until its time limit."
        ),
        None => "Every model route is down; the run keeps trying until its time limit.".to_string(),
    }
}

fn permission(v: &Value) -> String {
    let title = string(v, "title")
        .map(|t| cut(&clean(t), 80))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "a tool call".to_string());
    let allowed = v.get("allowed").and_then(Value::as_bool).unwrap_or(false);
    let auto = v.get("auto").and_then(Value::as_bool).unwrap_or(false);
    let verdict = match (allowed, auto) {
        (true, true) => "allowed automatically",
        (true, false) => "allowed",
        (false, _) => "refused",
    };
    format!("Permission for {title} was {verdict}.")
}

fn needs_input(v: &Value) -> String {
    match serde_json::from_value::<NeedsInputPayload>(v.clone()) {
        Ok(p) => {
            let r = InputRequest::of(&p, None);
            if r.blocking {
                format!("The run needs you: {}", r.title)
            } else {
                r.title
            }
        }
        Err(_) => match string(v, "title") {
            Some(t) if !t.trim().is_empty() => format!("The run needs you: {}", clean(t)),
            _ => "The run needs you.".to_string(),
        },
    }
}

/// The event's own `text` when it has one (agentd writes the words of the question and answer events), else `fallback`.
fn own_text(v: &Value, fallback: &str) -> String {
    match string(v, "text") {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => fallback.to_string(),
    }
}

/// Where a brake quotes the engine's last line.
const BRAKE_QUOTE: &str = "Its words:";

/// How the status of the last round reads in the counter's sentence.
fn round_status(status: &str) -> String {
    match status {
        "completed" | "aborted" | "failed" => status.to_string(),
        "unknown" => "has an unknown status".to_string(),
        other => clean(other),
    }
}

/// A line of the goal loop. Every sentence is built from fixed words and the
/// event's own fields; none of them says that the work was checked, only what the loop did.
fn goal_loop(v: &Value, engine: &crate::engines::Engine) -> (NoticeKind, String) {
    let phase = string(v, "phase").unwrap_or("");
    match phase {
        "on" => (
            NoticeKind::GoalLoop,
            own_text(
                v,
                "Goal loop on: the engine checks the goal after each round.",
            ),
        ),
        "round" => match number(v, "rounds") {
            Some(n) => {
                let noun = if n == 1 { "round" } else { "rounds" };
                let last = round_status(string(v, "status").unwrap_or("unknown"));
                (
                    NoticeKind::GoalRound,
                    format!("Goal loop: {n} {noun} ended, the last one {last}."),
                )
            }
            None => (
                NoticeKind::GoalRound,
                own_text(v, "Goal loop: a round ended."),
            ),
        },
        "kick" => (
            NoticeKind::GoalLoop,
            "Goal loop: continue sent.".to_string(),
        ),
        "done" => {
            let base = engine.say("Goal loop: {engine} called the goal-complete tool.");
            let text = match string(v, "summary")
                .map(clean)
                .filter(|s| !s.trim().is_empty())
            {
                Some(summary) => format!("{base} {} {summary}", engine.say("{engine}'s summary:")),
                None => base,
            };
            (NoticeKind::GoalLoop, text)
        }
        "brake" => {
            let text = match string(v, "text").filter(|t| !t.trim().is_empty()) {
                // The engine's quoted line is shown apart (`goal_blocked_line`).
                Some(t) => t.split(BRAKE_QUOTE).next().unwrap_or(t).trim().to_string(),
                None => match string(v, "rule") {
                    Some(rule) => format!("The harness ended the run: rule {}.", clean(rule)),
                    None => "The harness ended the run.".to_string(),
                },
            };
            (NoticeKind::GoalBrake, text)
        }
        "off" => (
            NoticeKind::GoalQuiet,
            own_text(v, "Goal loop off: the run goes on as a plain run."),
        ),
        "fallback" => (
            NoticeKind::GoalQuiet,
            own_text(
                v,
                "Goal loop: the round log is missing; the loop is watched without it.",
            ),
        ),
        "rearm" => (
            NoticeKind::GoalQuiet,
            own_text(v, "Goal loop: the goal is set again for the new session."),
        ),
        other => (
            NoticeKind::GoalLoop,
            match string(v, "text").filter(|t| !t.trim().is_empty()) {
                Some(t) => t.to_string(),
                None if other.is_empty() => "Goal loop.".to_string(),
                None => format!("Goal loop: {}.", clean(other)),
            },
        ),
    }
}

/// `1 search`, `3 searches`: the noun after a count.
fn counted(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A line of the research module: at startup, `phase: "off"` says the user
/// switched research off for the run; at the end the event carries the counts of the run's calls. The counts line is built from the numbers
/// alone, with fixed words, and never says anything about the work.
fn research(v: &Value) -> String {
    if string(v, "phase") == Some("off") {
        return own_text(v, "Research off: you switched it off for this run.");
    }
    let Some(searches) = number(v, "searches") else {
        return "Research.".to_string();
    };
    let n = |key: &str| number(v, key).unwrap_or(0);
    format!(
        "Research: {}, {}, {}, {} stored, {}.",
        counted(searches, "search", "searches"),
        counted(n("fetches"), "page fetch", "page fetches"),
        counted(n("repos"), "repository download", "repository downloads"),
        counted(n("placed"), "item", "items"),
        counted(n("errors"), "error", "errors"),
    )
}

fn progress_notice(v: &Value, engine: &crate::engines::Engine) -> Option<(NoticeKind, String)> {
    let kind = string(v, "kind")?;
    Some(match kind {
        "" | "message" => return None,
        "provider-retry" => (NoticeKind::ProviderRetry, retry(v)),
        "provider-switch" => (NoticeKind::ProviderSwitch, switch(v)),
        "engine-stop" => (NoticeKind::EngineStop, engine_stop(v)),
        "notify" => (NoticeKind::Notify, notify(v)),
        "provider-wait" => (NoticeKind::ProviderWait, provider_wait(v)),
        "provider-outage" => (NoticeKind::ProviderOutage, provider_outage(v)),
        "goal-loop" => goal_loop(v, engine),
        "research" => (NoticeKind::Notify, research(v)),
        "review-round" => (NoticeKind::Notify, own_text(v, "Review round.")),
        "skills" => (NoticeKind::Notify, crate::skills_card::notice_line(v)),
        "engine-config" => (
            NoticeKind::Notify,
            crate::engine_config_card::notice_line(v),
        ),
        harness_protocol::direct::PROGRESS_REPORT => {
            (NoticeKind::Notify, crate::report_progress::notice_line(v))
        }
        "needs-input" => (NoticeKind::NeedsInput, needs_input(v)),
        "input-received" => (
            NoticeKind::InputReceived,
            own_text(v, "Your message reached the engine."),
        ),
        PROGRESS_INPUT_IGNORED => (
            NoticeKind::InputIgnored,
            match string(v, "text") {
                Some(t) if !t.trim().is_empty() => t.to_string(),
                _ => format!(
                    "The engine did not take the answer: {}.",
                    crate::input::ignored_phrase(string(v, "why").unwrap_or(""))
                ),
            },
        ),
        PROGRESS_INPUT_CLOSED => (NoticeKind::InputClosed, own_text(v, "The question closed.")),
        "new-session" => (
            NoticeKind::NewSession,
            own_text(v, "Restarted from the work tree."),
        ),
        other => (
            NoticeKind::Other,
            format!("Event {}", cut(&clean(other), 60)),
        ),
    })
}

/// The engine's `BLOCKED:` line that a `blocked-twice` brake quotes after `Its words:`, shown on a line of its own. `None` for any other
/// event, for a brake that quotes nothing and for a quoted line that does not begin `BLOCKED:`.
pub fn goal_blocked_line(e: &Envelope) -> Option<String> {
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    if e.event_type != EventType::TurnProgress
        || string(value, "kind") != Some("goal-loop")
        || string(value, "phase") != Some("brake")
        || string(value, "rule") != Some("blocked-twice")
    {
        return None;
    }
    let text = string(value, "text")?;
    let (_, quoted) = text.split_once(BRAKE_QUOTE)?;
    let line = quoted
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '\u{201c}' | '\u{201d}'))
        .trim();
    let line = clean(line);
    line.starts_with("BLOCKED:")
        .then(|| cut(&line, NOTICE_MAX_CHARS))
}

/// The notice an event carries, or `None` for an event that is not one (a message tail, a tool update, a status event). The words that name an
/// engine name the default engine; a run on another engine goes through [`notice_of_for`].
pub fn notice_of(e: &Envelope) -> Option<Notice> {
    notice_of_for(e, crate::engines::default_engine())
}

/// [`notice_of`] for a run on `engine`.
pub fn notice_of_for(e: &Envelope, engine: &crate::engines::Engine) -> Option<Notice> {
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    let (kind, text) = match e.event_type {
        EventType::PermissionRequested => (NoticeKind::Permission, permission(value)),
        EventType::TurnProgress => progress_notice(value, engine)?,
        _ => return None,
    };
    Some(Notice {
        event_id: e.event_id.clone(),
        ts: e.ts.clone(),
        kind,
        text: cut(&clean(&text), NOTICE_MAX_CHARS),
    })
}
