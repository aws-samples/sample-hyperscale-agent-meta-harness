//! The commands beyond start, watch, pull, link, runs and doctor: `cancel` and `pr`, and the four that answer a run's "Cline
//! needs you" question: `say`, `new-session`, `try-model` and `finish`. Each writes a plain form or, with `--json`, one JSON
//! line; exit 0 on success, 1 on an error (the API's reason on stderr, prefixed `ham: `). API strings are sanitized before they are
//! written. Each writes to an [`Output`] (the shell's live one, or a captured one for the chat box) and returns the exit code.

use crate::api::Api;
use crate::api::ApiError;
use crate::http::ControlPlane;
use crate::output::Output;
use ham_core::exit::ERROR;
use ham_core::input::{InputRequest, check_text, ignored_phrase, outcome_phrase, resolve_offered};
use ham_core::models;
use ham_core::render::model_name;
use ham_core::sanitize::sanitize_line;
use ham_core::state::{RunState, RunView};
use harness_protocol::api::v1::{InputAccepted, InputAnswer};
use harness_protocol::direct::{
    DIRECT_AGENT_ID, InstructAction, PROGRESS_INPUT_CLOSED, PROGRESS_INPUT_IGNORED,
    PROGRESS_INPUT_RECEIVED,
};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::event_shard;
use harness_protocol::target::RunTarget;
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;

fn fail(out: &Output, msg: impl std::fmt::Display) -> i32 {
    out.err_line(format!("ham: {}", sanitize_line(&msg.to_string())));
    ERROR
}

fn word<T: serde::Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// `ham cancel <runId>`: on the direct stage Cline is asked to stop and the work is kept.
pub async fn cmd_cancel<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    json: bool,
    out: &Output,
) -> i32 {
    match api.cancel_run(run_id).await {
        Ok(r) if json => {
            out.out_line(json!({"runId": r.run_id, "status": r.status}));
            0
        }
        Ok(r) => {
            out.out_line(format!(
                "run {} {}",
                sanitize_line(&r.run_id),
                word(&r.status)
            ));
            0
        }
        Err(e) => fail(out, format!("cancel {run_id}: {e}")),
    }
}

/// Why a run shows no PR link. A complete `--s3-only` run, or a complete run whose `delivery.ready` carries no `prUrl`, has none
/// by design; any other run may still get
/// one.
async fn no_pr_text<P: ControlPlane>(
    api: &Api<P>,
    v: &RunView,
    run_id: &str,
    status: &str,
) -> String {
    let complete = v.status == Some(GoalStatus::Complete);
    let by_design = complete
        && (v.zip.is_some()
            || matches!(
                api.get_run(run_id).await.map(|d| d.summary.target),
                Ok(RunTarget::S3Only)
            ));
    if by_design {
        format!(
            "run {run_id} has no PR by design (status {status}): this run's delivery is the zip and a local git repository, with no pull request; get the code with `ham pull {run_id}` (the zip) or `ham clone {run_id}` (a git repository on this machine)"
        )
    } else {
        format!("run {run_id} has no PR yet (status {status})")
    }
}

/// `ham pr <runId>`: the PR link from the harness's delivery events (invariant 4).
pub async fn cmd_pr<P: ControlPlane>(api: &Api<P>, run_id: &str, json: bool, out: &Output) -> i32 {
    let mut state = RunState::new(run_id);
    if let Err(e) = api.poll(&mut state).await {
        return fail(out, format!("reading events: {e}"));
    }
    let v = state.view();
    let status = v
        .status
        .map(|s| word(&s))
        .unwrap_or_else(|| "unknown".into());
    let Some(url) = v.pr_url.clone() else {
        return fail(out, no_pr_text(api, &v, run_id, &status).await);
    };
    if json {
        out.out_line(json!({"runId": run_id, "prUrl": url, "status": status}));
    } else {
        out.out_line(format!("PR {url} (run {run_id} {status})"));
    }
    0
}

/// How long a command waits for Cline's acknowledgement of an answer.
pub const ACK_WAIT: Duration = Duration::from_secs(20);
/// When the command sends the same answer once more if no acknowledgement has come (the control path is at-least-once; the request id
/// makes a repeat one answer).
pub const RESEND_AFTER: Duration = Duration::from_secs(12);
/// The pause between reads of the acknowledgement.
pub const ACK_POLL: Duration = Duration::from_secs(1);

/// What the user answers with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    /// A message into the same Cline session.
    Say(String),
    /// A fresh Cline session on the same files, with the user's words if any.
    NewSession(Option<String>),
    /// Another model (an id or an alias), with the user's words if any.
    TryModel { model: String, text: Option<String> },
    /// Finish the run with what exists.
    Finish,
}

impl Want {
    pub fn action(&self) -> InstructAction {
        match self {
            Want::Say(_) => InstructAction::ContinueWithPrompt,
            Want::NewSession(_) => InstructAction::NewSessionWithPrompt,
            Want::TryModel { .. } => InstructAction::TryModel,
            Want::Finish => InstructAction::Finish,
        }
    }

    fn text(&self) -> Option<&str> {
        match self {
            Want::Say(t) => Some(t),
            Want::NewSession(t) | Want::TryModel { text: t, .. } => t.as_deref(),
            Want::Finish => None,
        }
    }
}

/// The waits of an answer. The defaults are the three constants above; tests shorten them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub ack_wait: Duration,
    pub resend_after: Duration,
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ack_wait: ACK_WAIT,
            resend_after: RESEND_AFTER,
            poll: ACK_POLL,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnswerOptions {
    /// `--request <id>`: answer this question instead of the run's open one.
    pub request_id: Option<String>,
    /// `--no-wait`: return when the answer is queued.
    pub no_wait: bool,
    pub timing: Timing,
}

/// Why an answer was not sent or not accepted.
#[derive(Debug)]
pub enum AnswerError {
    /// The run's events could not be read.
    Read(ApiError),
    /// The run has no open question; the sentence says what happened to the last one.
    NotWaiting(String),
    /// The answer cannot be right for this question (not offered, a bad message, a model the run cannot switch to).
    Refused(String),
    /// The control plane refused the answer or failed; the server's own words.
    Api(ApiError),
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnswerError::Read(e) => write!(f, "reading events: {e}"),
            AnswerError::NotWaiting(s) | AnswerError::Refused(s) => f.write_str(s),
            AnswerError::Api(ApiError::Http { error, .. }) => f.write_str(&error.message),
            AnswerError::Api(e) => write!(f, "{e}"),
        }
    }
}

/// What came back after the answer was queued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ack {
    /// `input-received`: Cline took it. `text_bytes` is the length of the user's text (0 for none).
    Received { text_bytes: u64 },
    /// `input-ignored`: Cline did not take it; `why` is the event's code.
    Ignored { why: String },
    /// `input-closed` before any `input-received`: the question ended without taking the answer.
    Closed { how: String },
    /// Nothing came within [`ACK_WAIT`].
    Unacknowledged,
    /// `--no-wait`.
    NotWaited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answered {
    pub run_id: String,
    pub request_id: String,
    pub action: InstructAction,
    /// The route id of a model switch.
    pub model: Option<String>,
    /// `queued`, `already-queued` or `re-queued`: the server's word.
    pub state: String,
    pub ack: Ack,
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn action_word(a: InstructAction) -> &'static str {
    match a {
        InstructAction::ContinueWithPrompt => "continue_with_prompt",
        InstructAction::NewSessionWithPrompt => "new_session_with_prompt",
        InstructAction::TryModel => "try_model",
        InstructAction::Finish => "finish",
    }
}

fn action_name(a: InstructAction) -> &'static str {
    match a {
        InstructAction::ContinueWithPrompt => "a message to the same session",
        InstructAction::NewSessionWithPrompt => "a fresh session",
        InstructAction::TryModel => "another model",
        InstructAction::Finish => "finish",
    }
}

/// The sentence for a run that waits for nobody: what became of its last question, if it had one.
fn not_waiting(v: &RunView) -> String {
    let id = sanitize_line(&v.run_id);
    match v.input.history.last() {
        Some(last) => format!(
            "run {id} is not waiting for you. Its last question was {}.",
            last.phrase
        ),
        None => format!("run {id} is not waiting for you. Cline has not asked it a question."),
    }
}

fn not_offered(q: &InputRequest, a: InstructAction) -> String {
    let offered: Vec<&str> = q
        .options
        .iter()
        .filter(|o| q.offers(**o))
        .map(|o| action_name(*o))
        .collect();
    let why = if !q.can_continue
        && matches!(
            a,
            InstructAction::ContinueWithPrompt | InstructAction::TryModel
        ) {
        "its Cline session cannot take another prompt".to_string()
    } else {
        "this question does not offer it".to_string()
    };
    format!(
        "cannot answer with {}: {why}. It offers: {}",
        action_name(a),
        if offered.is_empty() {
            "nothing".to_string()
        } else {
            offered.join("; ")
        }
    )
}

/// The acknowledgement of `request_id` among `events` (the `input-received`, `input-ignored` and `input-closed` events of the direct
/// agent), whatever order the page lists them in: an `input-received` wins over an `input-ignored`, which wins over an `input-closed`.
///
/// Ids are read in order, but two events of one moment can come in either order (a finish is received and then its question closed
/// in the same millisecond; a page that listed the close first made the command exit 1 for a finish that
/// had been taken). A close with no receipt is still the end of the question without the answer, and stays an error.
fn find_ack(events: &[Envelope], request_id: &str) -> Option<Ack> {
    let mut ignored = None;
    let mut closed = None;
    for e in events {
        if e.event_type != EventType::TurnProgress {
            continue;
        }
        let Payload::Inline { value } = &e.payload else {
            continue;
        };
        if value.get("requestId").and_then(|v| v.as_str()) != Some(request_id) {
            continue;
        }
        let str_of = |k: &str| value.get(k).and_then(|v| v.as_str()).map(sanitize_line);
        match value.get("kind").and_then(|v| v.as_str()) {
            Some(PROGRESS_INPUT_RECEIVED) => {
                return Some(Ack::Received {
                    text_bytes: value.get("textBytes").and_then(|v| v.as_u64()).unwrap_or(0),
                });
            }
            Some(PROGRESS_INPUT_IGNORED) => {
                ignored.get_or_insert_with(|| Ack::Ignored {
                    why: str_of("why").unwrap_or_default(),
                });
            }
            Some(PROGRESS_INPUT_CLOSED) => {
                closed.get_or_insert_with(|| Ack::Closed {
                    how: str_of("how").unwrap_or_default(),
                });
            }
            _ => {}
        }
    }
    ignored.or(closed)
}

/// Reads the direct agent's shard after `after` until Cline acknowledges `answer`, sending it once more at `resend_after`.
async fn wait_for_ack<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    answer: &InputAnswer,
    after: &str,
    timing: &Timing,
) -> Ack {
    let shard = event_shard(Authority::Agent, Some(DIRECT_AGENT_ID));
    let started = Instant::now();
    let mut resent = false;
    loop {
        // A failed read is tried again until the bound; the answer is already queued.
        if let Ok(page) = api.events(run_id, shard, Some(after)).await
            && let Some(ack) = find_ack(&page.events, &answer.request_id)
        {
            return ack;
        }
        let elapsed = started.elapsed();
        if elapsed >= timing.ack_wait {
            return Ack::Unacknowledged;
        }
        if !resent && elapsed >= timing.resend_after {
            resent = true;
            // The same answer again is one answer (the request id is the nonce); a refusal here changes nothing the reads will not show.
            let _ = api.answer_input(run_id, answer).await;
        }
        tokio::time::sleep(timing.poll.min(timing.ack_wait.saturating_sub(elapsed))).await;
    }
}

/// Answers the run's open question with `want`: finds the question from the run's events, checks the answer against what the question
/// offers, sends it, and (unless `--no-wait`) waits for Cline's acknowledgement.
pub async fn answer<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    want: &Want,
    opts: &AnswerOptions,
) -> Result<Answered, AnswerError> {
    let action = want.action();
    // A message is required for `say`; the other actions take one only if given.
    if let Some(t) = want
        .text()
        .filter(|t| matches!(want, Want::Say(_)) || !t.trim().is_empty())
    {
        check_text(t).map_err(AnswerError::Refused)?;
    }
    let mut state = RunState::new(run_id);
    api.poll(&mut state).await.map_err(AnswerError::Read)?;
    let view = state.view_at(Some(now_unix()));
    let open = view.input.waiting();
    let request_id = match (&opts.request_id, open) {
        (Some(id), _) => id.clone(),
        (None, Some(q)) => q.request_id.clone(),
        (None, None) => return Err(AnswerError::NotWaiting(not_waiting(&view))),
    };
    let question = open.filter(|q| q.request_id == request_id);
    if let Some(q) = question
        && !q.offers(action)
    {
        return Err(AnswerError::Refused(not_offered(q, action)));
    }
    let model = match (want, question) {
        (Want::TryModel { model, .. }, Some(q)) => Some(
            resolve_offered(model, &q.models)
                .map_err(AnswerError::Refused)?
                .id
                .clone(),
        ),
        (Want::TryModel { model, .. }, None) => Some(
            models::builtin()
                .find(model)
                .map_or_else(|| model.trim().to_string(), |m| m.id.clone()),
        ),
        _ => None,
    };
    let answer = InputAnswer {
        request_id: request_id.clone(),
        action,
        text: want
            .text()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string),
        model: model.clone(),
    };
    // Only events after everything read so far can acknowledge this answer (an older `input-ignored` of the same question is not it).
    // The direct agent's events share one clock (agentd's), so its own newest event is the safe mark; another writer's clock may differ.
    let after = state
        .accepted()
        .filter(|e| {
            e.agent_id.as_deref() == Some(DIRECT_AGENT_ID) && e.authority == Authority::Agent
        })
        .map(|e| e.event_id.clone())
        .max()
        .unwrap_or_else(|| request_id.clone());
    let InputAccepted { state: queued, .. } = api
        .answer_input(run_id, &answer)
        .await
        .map_err(AnswerError::Api)?;
    let ack = if opts.no_wait {
        Ack::NotWaited
    } else {
        wait_for_ack(api, run_id, &answer, &after, &opts.timing).await
    };
    Ok(Answered {
        run_id: run_id.to_string(),
        request_id,
        action,
        model,
        state: queued,
        ack,
    })
}

/// The text, then the exit code, of an answered command: success on stdout, anything else on stderr.
pub fn answered_text(a: &Answered) -> (String, i32) {
    let model = a.model.as_deref().map(model_name);
    match &a.ack {
        Ack::Received { text_bytes } => (
            match a.action {
                InstructAction::ContinueWithPrompt => {
                    "Cline took your message (same session).".to_string()
                }
                InstructAction::NewSessionWithPrompt => {
                    "Cline took your request: a fresh session starts on the same files.".to_string()
                }
                InstructAction::TryModel => format!(
                    "Switched to {}. Cline took {}.",
                    model.as_deref().unwrap_or("the other model"),
                    if *text_bytes > 0 {
                        "your message"
                    } else {
                        "the fixed continue message"
                    }
                ),
                InstructAction::Finish => "The run will finish with what exists.".to_string(),
            },
            0,
        ),
        Ack::NotWaited => (
            format!(
                "Queued {} for Cline; not waiting for its acknowledgement. ham watch {} shows what it does.",
                action_name(a.action),
                a.run_id
            ),
            0,
        ),
        Ack::Ignored { why } => (
            format!(
                "Cline did not take {}: {}.",
                action_name(a.action),
                ignored_phrase(why)
            ),
            ERROR,
        ),
        Ack::Closed { how } => (
            format!(
                "The question closed before Cline took your answer ({}).",
                outcome_phrase(how)
            ),
            ERROR,
        ),
        Ack::Unacknowledged => (
            format!(
                "queued, not acknowledged yet: the run may have ended; run: ham watch {}",
                a.run_id
            ),
            ERROR,
        ),
    }
}

/// The `--json` line of an answered command.
pub fn answered_json(a: &Answered) -> serde_json::Value {
    let (result, extra) = match &a.ack {
        Ack::Received { text_bytes } => ("received", json!({"textBytes": text_bytes})),
        Ack::Ignored { why } => ("ignored", json!({"why": why})),
        Ack::Closed { how } => ("closed", json!({"how": how})),
        Ack::Unacknowledged => ("unacknowledged", json!({})),
        Ack::NotWaited => ("queued", json!({})),
    };
    let mut v = json!({"runId": a.run_id, "requestId": a.request_id, "action": action_word(a.action),
        "state": a.state, "result": result});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in e {
            o.insert(k.clone(), val.clone());
        }
        if let Some(m) = &a.model {
            o.insert("model".into(), json!(m));
        }
    }
    v
}

/// Runs [`answer`] and writes its result: the plain sentence (stdout on success, stderr with `ham: ` otherwise) or one JSON line.
pub async fn cmd_answer<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    want: &Want,
    opts: &AnswerOptions,
    json: bool,
    out: &Output,
) -> i32 {
    match answer(api, run_id, want, opts).await {
        Ok(a) => {
            let (text, code) = answered_text(&a);
            if json {
                out.out_line(answered_json(&a));
            } else if code == 0 {
                out.out_line(text);
            } else {
                out.err_line(format!("ham: {}", sanitize_line(&text)));
            }
            code
        }
        Err(e) => fail(out, e),
    }
}

/// `ham say <runId> '<text>'`.
pub async fn cmd_say<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    text: &str,
    opts: &AnswerOptions,
    json: bool,
    out: &Output,
) -> i32 {
    cmd_answer(api, run_id, &Want::Say(text.to_string()), opts, json, out).await
}

/// `ham new-session <runId> ['<text>']`.
pub async fn cmd_new_session<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    text: Option<&str>,
    opts: &AnswerOptions,
    json: bool,
    out: &Output,
) -> i32 {
    let want = Want::NewSession(text.map(str::to_string));
    cmd_answer(api, run_id, &want, opts, json, out).await
}

/// `ham try-model <runId> <model> ['<text>']`.
pub async fn cmd_try_model<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    model: &str,
    text: Option<&str>,
    opts: &AnswerOptions,
    json: bool,
    out: &Output,
) -> i32 {
    let want = Want::TryModel {
        model: model.to_string(),
        text: text.map(str::to_string),
    };
    cmd_answer(api, run_id, &want, opts, json, out).await
}

/// `ham finish <runId>`.
pub async fn cmd_finish<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    opts: &AnswerOptions,
    json: bool,
    out: &Output,
) -> i32 {
    cmd_answer(api, run_id, &Want::Finish, opts, json, out).await
}
