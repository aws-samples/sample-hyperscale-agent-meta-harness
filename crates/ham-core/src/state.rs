//! Run state from the event log.
//!
//! [`RunState`] keeps every accepted envelope keyed by its ULID and derives [`RunView`] by folding
//! them in ULID order, so duplicate, out-of-order and gapped delivery across the 32 shard cursors
//! converge to one view. An envelope that fails [`harness_protocol::envelope::validate`] is
//! rejected: an agent-authored `goal.status`, `goal.blocked`, `delivery.*` or `run.completed`
//! (stored with `authority=agent` by `/internal/events`) never changes status or shows a link.
//! Agent text is shown only after [`crate::sanitize::sanitize_line`].
//!
//! Payload fields read (inline JSON; `harness_protocol::orchestration` holds the harness payload
//! types; the phase changes only by `goal.status`): `goal.status` {status, phase?}; `goal.blocked`
//! {reason}; `delivery.pr` {url}; `delivery.ready` {artifactId, sha256, sizeBytes, prUrl?};
//! `run.completed` {status?, outcome?, label?, detail?}; `run.aborted` {status, error?, cause?,
//! stoppedAt}; `session.started` {agentId?, engine, modelId, contextTokens?}; `tool.summary`
//! {toolCallId?, title, kind?, status?}; a tool call's update, a `turn.progress` with {toolCallId,
//! status} and no `kind`; `turn.progress` {text | summary}, whose `kind` `provider-retry`, `provider-switch`, `engine-stop`, `notify`,
//! `provider-wait` and `provider-outage` (and `permission.requested`) are the harness notices of [`crate::notice`];
//! `turn.ended` {text | summary}. The direct line writes no other event type that the client reads: an event of any
//! other type is accepted and changes nothing.

use crate::input::{InputView, RestartNote};
use crate::notice::{Notice, NoticeKind, notice_of};
use crate::render::{model_name, model_text};
use crate::sanitize::{display_line, sanitize_line};
use harness_protocol::api::v1::EventsPage;
use harness_protocol::direct::{DIRECT_AGENT_ID, DirectOutcome};
use harness_protocol::direct_input::PROGRESS_NEW_SESSION;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, validate};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    Accepted,
    Duplicate,
    Rejected(String),
    OtherRun,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub event_id: String,
    pub event_type: EventType,
    pub authority: Authority,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZipLink {
    pub artifact_id: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentLine {
    pub agent_id: String,
    pub engine: Option<String>,
    pub model_id: Option<String>,
    /// Latest agent summary, sanitized to one line.
    pub last_summary: String,
    pub turns: u32,
    /// The session's effective context window in tokens.
    /// `None` when the engine's own window applies.
    pub context_tokens: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedView {
    pub reason: String,
}

/// Why a run ended early: its `run` execution ended ABORTED, FAILED or
/// TIMED_OUT and the harness cleared it. Every field is sanitized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbortedView {
    /// The execution's end: `ABORTED`, `FAILED` or `TIMED_OUT`.
    pub status: String,
    pub error: Option<String>,
    pub cause: Option<String>,
    pub stopped_at: Option<String>,
}

impl AbortedView {
    /// `<status>[ <error>][: <cause>]`, the line the TUI, `ham watch` and `ham runs` show.
    pub fn line(&self) -> String {
        let mut s = self.status.clone();
        if let Some(e) = &self.error {
            s.push(' ');
            s.push_str(e);
        }
        if let Some(c) = &self.cause {
            s.push_str(": ");
            s.push_str(c);
        }
        s
    }

    /// The view of a Run item's or a summary's `aborted`.
    pub fn of(p: &harness_protocol::orchestration::RunAbortedPayload) -> Self {
        Self {
            status: p.status.as_str().to_string(),
            error: p.error.as_deref().map(sanitize_line),
            cause: p.cause.as_deref().map(sanitize_line),
            stopped_at: Some(sanitize_line(&p.stopped_at)),
        }
    }
}

/// How a direct run's Cline session ended, as the harness wrote it on `run.completed` (protocol 1.30.0): the product's whole claim
/// about the work. "Cline reported done" or "Cline stopped: <reason>" is shown exactly as delivered, and nothing here says the
/// harness checked anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    /// The wire word (`reported_done`, `stopped_silence`, ...), sanitized; `None` when the event carried a label alone.
    pub outcome: Option<String>,
    /// The sentence `ham` shows. The event's `label` as delivered (a label a later harness adds is shown as it is); when the event
    /// names a known outcome and no label, the label of that outcome and detail (`DirectOutcome::label_for`).
    pub label: String,
    /// The specifics (the silence seconds, why no pull request was opened), sanitized.
    pub detail: Option<String>,
}

/// A fact the harness read at the end of the run and wrote down as it found it. It says
/// nothing about whether the work is good: the card shows it beside the label, one line each, and never rebuilds the sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The kind word as the event wrote it (`last_log_entry_red`); a kind this client does not know is kept.
    pub kind: String,
    /// The sentence the harness wrote, cleaned of control characters and of anything that looks like a token.
    pub text: String,
}

impl Observation {
    /// The kinds that wear the screen's warning marker: the last entry of the run log is a red run.
    pub fn warns(&self) -> bool {
        self.kind == "last_log_entry_red"
    }
}

/// At most this many observations are kept (the protocol lists at most four).
const MAX_OBSERVATIONS: usize = 4;

/// One tool call of a run: the `tool.summary` event that names it (`title`, `kind`, `status`) and the later `turn.progress`
/// updates that carry its newer `status`, joined by `toolCallId`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRow {
    /// The engine's call id, sanitized; empty when the event named none (such a row is never joined to a later one).
    pub call_id: String,
    /// What the call does (`run_commands: pytest -q`), terminal safe; empty until the summary arrives.
    pub title: String,
    /// `read`, `edit`, `execute`, `search`, ... as the engine sent it.
    pub kind: Option<String>,
    /// `pending`, `in_progress`, `completed` or `failed`: the furthest one reached (see [`merge_status`]).
    pub status: Option<String>,
    /// The id of the first event that named the call: where the call sits in the log.
    pub event_id: String,
}

/// The status a call has after `new` arrives: a call only moves forward (`pending`, `in_progress`, then `completed` or `failed`), and
/// a later finished status replaces an earlier one. In the recorded stream the summary (`pending`) arrives after the `in_progress`
/// update, and a repeated update can arrive late.
pub fn merge_status(old: Option<&str>, new: Option<&str>) -> Option<String> {
    fn rank(s: &str) -> u8 {
        match s {
            "pending" => 0,
            "completed" | "failed" => 2,
            _ => 1,
        }
    }
    match (old, new) {
        (None, n) => n.map(str::to_string),
        (o, None) => o.map(str::to_string),
        (Some(o), Some(n)) => Some(if rank(n) >= rank(o) { n } else { o }.to_string()),
    }
}

/// What [`ToolTracker::apply`] did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolTouch {
    /// The index of the row the event created or changed.
    pub index: usize,
    /// The row is new.
    pub created: bool,
}

/// Folds `tool.summary` events and tool-call updates into rows, one per call, in the order the calls began. The run view and the chat
/// view use the same tracker, so they never disagree about a call.
#[derive(Clone, Debug, Default)]
pub struct ToolTracker {
    rows: Vec<ToolRow>,
    index: HashMap<String, usize>,
}

impl ToolTracker {
    pub fn rows(&self) -> &[ToolRow] {
        &self.rows
    }

    pub fn into_rows(self) -> Vec<ToolRow> {
        self.rows
    }

    /// Applies one event (in event-id order). `None` for an event that is neither a tool summary nor a tool-call update.
    pub fn apply(&mut self, e: &Envelope) -> Option<ToolTouch> {
        let v = inline(e)?;
        let text = |k: &str| v.get(k).and_then(Value::as_str);
        let call_id = text("toolCallId")
            .map(sanitize_line)
            .filter(|c| !c.is_empty());
        match e.event_type {
            EventType::ToolSummary => {
                let title = text("title")
                    .map(display_line)
                    .filter(|t| !t.trim().is_empty());
                // The old `text` member was never written; a summary with no title and no call id says nothing.
                if title.is_none() && call_id.is_none() {
                    return None;
                }
                let kind = text("kind").map(sanitize_line).filter(|k| !k.is_empty());
                let status = text("status").map(sanitize_line).filter(|s| !s.is_empty());
                Some(self.upsert(e, call_id, title, kind, status))
            }
            // A tool call's update: {toolCallId, status} and no `kind` (the message and notice progress events all carry one).
            EventType::TurnProgress if v.get("kind").is_none() => {
                let id = call_id?;
                let status = text("status")
                    .map(sanitize_line)
                    .filter(|s| !s.is_empty())?;
                Some(self.upsert(e, Some(id), None, None, Some(status)))
            }
            _ => None,
        }
    }

    fn upsert(
        &mut self,
        e: &Envelope,
        call_id: Option<String>,
        title: Option<String>,
        kind: Option<String>,
        status: Option<String>,
    ) -> ToolTouch {
        let existing = call_id.as_ref().and_then(|id| self.index.get(id).copied());
        match existing {
            Some(i) => {
                let row = &mut self.rows[i];
                if let Some(t) = title {
                    row.title = t;
                }
                if kind.is_some() {
                    row.kind = kind;
                }
                row.status = merge_status(row.status.as_deref(), status.as_deref());
                ToolTouch {
                    index: i,
                    created: false,
                }
            }
            None => {
                let i = self.rows.len();
                if let Some(id) = &call_id {
                    self.index.insert(id.clone(), i);
                }
                self.rows.push(ToolRow {
                    call_id: call_id.unwrap_or_default(),
                    title: title.unwrap_or_default(),
                    kind,
                    status,
                    event_id: e.event_id.clone(),
                });
                ToolTouch {
                    index: i,
                    created: true,
                }
            }
        }
    }
}

/// Where the run's model calls stand: the route in use after switches, and a retry or wait in progress. The status line shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteStatus {
    /// The model id after the latest `provider-switch`; `None` while the run is on the model it started with.
    pub model: Option<String>,
    /// How many `provider-switch` events the run has.
    pub switches: u32,
    /// `(try, of)` of the latest `provider-retry`, until the engine works again.
    pub retry: Option<(u32, u32)>,
    /// Every route failed and agentd waits before it probes (`provider-wait`).
    pub waiting: bool,
    /// Minutes every route has been down (`provider-outage`).
    pub outage_minutes: Option<u64>,
}

impl RouteStatus {
    fn working_again(&mut self) {
        self.retry = None;
        self.waiting = false;
        self.outage_minutes = None;
    }
}

/// What `ham` shows for one run.
#[derive(Clone, Debug, PartialEq)]
pub struct RunView {
    pub run_id: String,
    pub status: Option<GoalStatus>,
    pub phase: Option<GoalPhase>,
    pub pr_url: Option<String>,
    pub zip: Option<ZipLink>,
    pub agents: BTreeMap<String, AgentLine>,
    pub blocked: Option<BlockedView>,
    /// Why the run ended early, once `run.aborted` is read.
    pub aborted: Option<AbortedView>,
    /// Rejected events that claimed a harness-only type (for example a forged completion).
    pub untrusted: u32,
    /// Accepted events.
    pub events: usize,
    pub completed: bool,
    /// How the Cline session ended, once `run.completed` carries it. `None` for a run the harness has not finished.
    pub result: Option<RunResult>,
    /// The question the run waits on and the last few that ended; empty for a run that never asked.
    pub input: InputView,
    /// The Cline sessions of the run: 1, plus one for every fresh session.
    pub sessions: u32,
    /// The latest fresh session, once there is one.
    pub restarted: Option<RestartNote>,
    /// The facts the harness read at the end of the run, in the order `run.completed` lists them; empty for a run without any.
    pub observations: Vec<Observation>,
    /// The run's tool calls, one row each, in the order they began.
    pub tools: Vec<ToolRow>,
    /// The provider and engine notices in event order; the question and answer events are `input`.
    pub notices: Vec<Notice>,
    /// The route in use and a retry or wait in progress.
    pub route: RouteStatus,
    /// The card of a goal-loop brake: which stopping rule ended the run, how far the loop got and the follow-up command.
    pub goal_brake: Option<crate::goal_card::GoalBrakeCard>,
    /// How many prompts the run's own events say the harness sent to the engine on its own: a goal-loop continue
    /// (`goal-loop`, phase `kick`), a provider retry (agentd sends a fixed resume sentence after each) and a fresh session (its first prompt
    /// is harness-written). Each event counts once, whatever order it arrived in. It is a count of events the client read, not a
    /// verified count of sends; the first prompt, a person's message and the engine's own goal rounds are not in it.
    pub harness_prompts: u32,
    /// The skills the run was given: names, digests, sizes and whether the engine's own list showed them, from the `skills`
    /// event of the direct agent; `None` for a run that carried none.
    pub skills: Option<crate::skills_card::SkillsCard>,
    /// The engine settings placed for the run (the configuration program): the settings hash and the keys, from the `engine-config` event agentd
    /// posts after it wrote them where the engine reads them and read them back; `None` for a run that carried none.
    pub engine_config: Option<crate::engine_config_card::EngineConfigCard>,
    /// The newest `report` event of the clean-checkout test report: what the report was doing when the run last said so. `None` for a run
    /// that wrote none (a stage without the report, a report switched off, an agentd older than the events).
    pub report: Option<crate::report_progress::ReportNow>,
}

impl RunView {
    /// The clean-checkout test report is going on now: its newest event says it began or a command runs, the run is not over, the stored status
    /// is `active` and delivery has not begun. A lost end event cannot hold the words past delivery or the end of the run.
    pub fn report_running(&self) -> bool {
        !self.is_over()
            && self.status == Some(GoalStatus::Active)
            && self.phase != Some(GoalPhase::Delivering)
            && self
                .report
                .as_ref()
                .is_some_and(crate::report_progress::ReportNow::is_running)
    }

    /// The phase the screens of a run that goes on show: `running the repository's tests` while the report runs ([`RunView::report_running`]),
    /// else the stored phase as [`crate::render::phase_shown`] says it (`None` for a run that is over). The stored `phase` is not changed.
    pub fn phase_now(&self) -> Option<&'static str> {
        if self.report_running() {
            return Some(crate::report_progress::PHASE_WORDS);
        }
        crate::render::phase_shown(self.status, self.phase)
    }

    /// The status line's route part: `running on GPT-6.1 Sol after 2 switches`, `provider error, try 2 of 4`,
    /// `waiting on the model provider`, `every model route down for 30 min`; parts joined with `; `. `None` when there is nothing to say.
    pub fn route_line(&self) -> Option<String> {
        let r = &self.route;
        let mut parts: Vec<String> = Vec::new();
        if let Some(m) = &r.model {
            let name = if crate::models::builtin().find(m).is_some() {
                model_name(m)
            } else {
                model_text(m)
            };
            let n = r.switches;
            parts.push(format!(
                "running on {name} after {n} switch{}",
                if n == 1 { "" } else { "es" }
            ));
        }
        parts.extend(self.trouble_parts());
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    /// The parts of the route line that say something is going wrong with the model calls: a retry in progress, a wait, an outage.
    fn trouble_parts(&self) -> Vec<String> {
        let r = &self.route;
        let mut parts: Vec<String> = Vec::new();
        if let Some((a, o)) = r.retry {
            parts.push(format!("provider error, try {a} of {o}"));
        }
        if r.waiting {
            parts.push("waiting on the model provider".to_string());
        }
        if let Some(m) = r.outage_minutes {
            parts.push(format!("every model route down for {m} min"));
        }
        parts
    }

    /// The run's last word is read: `run.completed` or `run.aborted`, or a terminal goal status.
    fn is_over(&self) -> bool {
        self.completed || self.aborted.is_some() || self.status.is_some_and(GoalStatus::is_terminal)
    }

    /// The trouble part of the route line (`provider error, try 2 of 4`; `waiting on the model provider`; `every model route down for 30 min`)
    /// while the run goes on. The header of the chat view names the model in use itself and adds only this. `None` when the model
    /// calls work and once the run is over: a retry that was the last event before the end is no longer going on.
    pub fn route_trouble(&self) -> Option<String> {
        if self.is_over() {
            return None;
        }
        let parts = self.trouble_parts();
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    /// [`RunView::route_line`] while the run goes on, for the status block's Route row. A run that is over says where it ended in its
    /// `Ran on` row ([`crate::render::ended_on`]) and has no live route.
    pub fn route_now(&self) -> Option<String> {
        if self.is_over() {
            return None;
        }
        self.route_line()
    }

    /// The model id the direct session ran on, as the harness wrote it on `session.started` (sanitized); `None` before the
    /// session starts.
    pub fn direct_model_id(&self) -> Option<&str> {
        self.agents
            .get(DIRECT_AGENT_ID)
            .and_then(|a| a.model_id.as_deref())
            .filter(|m| !m.trim().is_empty())
    }

    /// Each model's effective context window as its agents report it: the largest of
    /// its sessions', sorted by model id. Models whose sessions report none are left out.
    pub fn windows(&self) -> Vec<(String, u32)> {
        let mut by_model: BTreeMap<&str, u32> = BTreeMap::new();
        for a in self.agents.values() {
            if let (Some(m), Some(t)) = (a.model_id.as_deref(), a.context_tokens) {
                let w = by_model.entry(m).or_insert(0);
                *w = (*w).max(t);
            }
        }
        by_model
            .into_iter()
            .map(|(m, t)| (m.to_string(), t))
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct RunState {
    run_id: String,
    events: BTreeMap<String, Envelope>,
    rejected: BTreeMap<String, Rejection>,
    cursors: BTreeMap<u8, String>,
    /// An accepted `session.started` named the model its session runs on.
    model_seen: bool,
    /// An accepted harness `run.completed` or `run.aborted`: the run's last event was read (EVT1).
    ended: bool,
}

impl RunState {
    pub fn new(run_id: &str) -> Self {
        Self {
            run_id: run_id.to_string(),
            events: BTreeMap::new(),
            rejected: BTreeMap::new(),
            cursors: BTreeMap::new(),
            model_seen: false,
            ended: false,
        }
    }

    /// Whether the harness's `run.completed` or `run.aborted` has been read. Only a harness event counts (the envelope check refuses the
    /// types to anyone else): the poll plan reads the harness shard from its start on every cycle until it is true, and for a few cycles after.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Whether an accepted `session.started` named the model: until it does, the poll plan reads the harness shard again from its start now
    /// and then, because the event may have been written with an id below a cursor that has already passed it.
    pub fn model_seen(&self) -> bool {
        self.model_seen
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The accepted envelopes in event-id order (ULIDs, so the order the harness wrote them).
    pub fn accepted(&self) -> impl Iterator<Item = &Envelope> {
        self.events.values()
    }

    pub fn apply(&mut self, e: Envelope) -> Applied {
        if e.run_id != self.run_id {
            return Applied::OtherRun;
        }
        if self.events.contains_key(&e.event_id) || self.rejected.contains_key(&e.event_id) {
            return Applied::Duplicate;
        }
        if let Err(err) = validate(&e) {
            let reason = err.to_string();
            self.rejected.insert(
                e.event_id.clone(),
                Rejection {
                    event_id: e.event_id.clone(),
                    event_type: e.event_type,
                    authority: e.authority,
                    reason: reason.clone(),
                },
            );
            return Applied::Rejected(reason);
        }
        if e.event_type == EventType::SessionStarted
            && inline(&e)
                .and_then(|v| v.get("modelId"))
                .and_then(Value::as_str)
                .is_some_and(|m| !m.trim().is_empty())
        {
            self.model_seen = true;
        }
        if matches!(
            e.event_type,
            EventType::RunCompleted | EventType::RunAborted
        ) {
            self.ended = true;
        }
        self.events.insert(e.event_id.clone(), e);
        Applied::Accepted
    }

    /// Applies one shard page and advances that shard's cursor.
    pub fn apply_page(&mut self, shard: u8, page: EventsPage) {
        let last = page.events.iter().map(|e| e.event_id.clone()).max();
        for e in page.events {
            self.apply(e);
        }
        if let Some(next) = page.next_after.or(last) {
            let keep = self.cursors.get(&shard).is_some_and(|c| *c >= next);
            if !keep {
                self.cursors.insert(shard, next);
            }
        }
    }

    /// How many events the state holds (cheap: no view is built).
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub fn cursor(&self, shard: u8) -> Option<&str> {
        self.cursors.get(&shard).map(String::as_str)
    }

    pub fn rejected(&self) -> Vec<&Rejection> {
        self.rejected.values().collect()
    }

    pub fn view(&self) -> RunView {
        self.view_at(None)
    }

    /// [`RunState::view`] with the client's clock (Unix seconds), which fills the seconds left to answer a question.
    pub fn view_at(&self, now: Option<i64>) -> RunView {
        let mut v = RunView {
            run_id: self.run_id.clone(),
            status: None,
            phase: None,
            pr_url: None,
            zip: None,
            agents: BTreeMap::new(),
            blocked: None,
            aborted: None,
            untrusted: self
                .rejected
                .values()
                .filter(|r| r.event_type.required_authority() == Authority::Harness)
                .count() as u32,
            events: self.events.len(),
            completed: false,
            result: None,
            input: InputView::default(),
            observations: Vec::new(),
            sessions: 1,
            restarted: None,
            tools: Vec::new(),
            notices: Vec::new(),
            route: RouteStatus::default(),
            goal_brake: None,
            harness_prompts: 0,
            skills: None,
            engine_config: None,
            report: None,
        };
        let mut tools = ToolTracker::default();
        for e in self.events.values() {
            fold(&mut v, e, &mut tools);
        }
        v.tools = tools.into_rows();
        let built = crate::input::build(self.events.values(), now);
        v.input = built.input;
        v.sessions = built.sessions;
        // Every fresh session starts with a prompt the harness wrote.
        v.harness_prompts = v
            .harness_prompts
            .saturating_add(built.sessions.saturating_sub(1));
        v.restarted = built.restarted;
        v.goal_brake = crate::goal_card::from_events(self.events.values());
        v.skills = crate::skills_card::from_events(self.events.values());
        v.engine_config = crate::engine_config_card::from_events(self.events.values());
        v
    }
}

fn inline(e: &Envelope) -> Option<&Value> {
    match &e.payload {
        Payload::Inline { value } => Some(value),
        Payload::ClaimCheck { .. } => None,
    }
}

/// The `outcome`, `label` and `detail` of a `run.completed` (1.30.0). Nothing without a label or a known outcome.
fn run_result(e: &Envelope) -> Option<RunResult> {
    let clean = |k: &str| {
        str_field(e, k)
            .map(|x| sanitize_line(&x))
            .filter(|x| !x.trim().is_empty())
    };
    let outcome = clean("outcome");
    let detail = clean("detail");
    let label = clean("label").or_else(|| {
        let word = outcome.as_deref()?;
        DirectOutcome::ALL
            .iter()
            .find(|o| o.as_str() == word)
            .map(|o| o.label_for(detail.as_deref()))
    })?;
    Some(RunResult {
        outcome,
        label,
        detail,
    })
}

/// The `observations` of a `run.completed`: entries with a text, in order, at most [`MAX_OBSERVATIONS`].
fn observations(e: &Envelope) -> Vec<Observation> {
    let Some(list) = inline(e)
        .and_then(|v| v.get("observations"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|o| {
            let text = crate::redact::redact_tokens(&sanitize_line(o.get("text")?.as_str()?));
            let text = text.trim().to_string();
            (!text.is_empty()).then(|| Observation {
                kind: o
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(|k| sanitize_line(k).trim().to_string())
                    .unwrap_or_default(),
                text,
            })
        })
        .take(MAX_OBSERVATIONS)
        .collect()
}

fn str_field(e: &Envelope, k: &str) -> Option<String> {
    inline(e)?.get(k)?.as_str().map(str::to_string)
}

fn typed<T: DeserializeOwned>(e: &Envelope, k: &str) -> Option<T> {
    serde_json::from_value(inline(e)?.get(k)?.clone()).ok()
}

fn agent<'a>(v: &'a mut RunView, id: &str) -> &'a mut AgentLine {
    v.agents.entry(id.to_string()).or_insert_with(|| AgentLine {
        agent_id: id.to_string(),
        ..AgentLine::default()
    })
}

fn set_status(v: &mut RunView, s: GoalStatus, phase: Option<GoalPhase>) {
    if v.status.is_some_and(GoalStatus::is_terminal) {
        return;
    }
    v.status = Some(s);
    if phase.is_some() {
        v.phase = phase;
    }
}

/// The parts of a `turn.progress` event beyond its text: a tool call's update and a provider or engine notice.
fn fold_progress(v: &mut RunView, e: &Envelope, tools: &mut ToolTracker) {
    if tools.apply(e).is_some() {
        v.route.working_again();
        return;
    }
    let kind = inline(e)
        .and_then(|p| p.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind == "message" {
        v.route.working_again();
        return;
    }
    // A fresh session begins on the route the user chose, whatever the ladder did before (agentd, `crates/agentd/src/direct/fresh.rs`): the model calls
    // start over there, and the route in use is the chosen one until the next switch.
    if kind == PROGRESS_NEW_SESSION {
        v.route.working_again();
        v.route.model = None;
    }
    // The clean-checkout test report: the newest event says what it is doing; an event of a state this client does not know changes nothing.
    if kind == harness_protocol::direct::PROGRESS_REPORT
        && let Some(now) = crate::report_progress::parse(e)
    {
        v.report = Some(now);
    }
    let Some(n) = notice_of(e) else { return };
    let payload = inline(e);
    let num = |k: &str| payload.and_then(|p| p.get(k)).and_then(Value::as_u64);
    match n.kind {
        NoticeKind::ProviderSwitch => {
            v.route.working_again();
            v.route.switches += 1;
            v.route.model = payload
                .and_then(|p| p.get("to"))
                .and_then(Value::as_str)
                .map(sanitize_line)
                .filter(|m| !m.is_empty());
        }
        NoticeKind::GoalLoop
            if payload.and_then(|p| p.get("phase")).and_then(Value::as_str) == Some("kick") =>
        {
            v.harness_prompts = v.harness_prompts.saturating_add(1);
        }
        NoticeKind::ProviderRetry => {
            v.harness_prompts = v.harness_prompts.saturating_add(1);
            v.route.retry = Some((
                u32::try_from(num("attempt").unwrap_or(1)).unwrap_or(u32::MAX),
                u32::try_from(num("of").unwrap_or(1)).unwrap_or(u32::MAX),
            ));
        }
        NoticeKind::ProviderWait => v.route.waiting = true,
        NoticeKind::ProviderOutage => {
            v.route.outage_minutes =
                num("outageMinutes").or_else(|| num("waitedS").map(|s| s / 60));
        }
        _ => {}
    }
    if n.kind.is_run_notice() {
        v.notices.push(n);
    }
}

fn fold(v: &mut RunView, e: &Envelope, tools: &mut ToolTracker) {
    // Defense in depth: validate() already enforced this at apply time.
    if e.authority != e.event_type.required_authority() {
        return;
    }
    let agent_id = e.agent_id.clone();
    match e.event_type {
        EventType::RunCreated => {
            if v.status.is_none() {
                v.status = Some(GoalStatus::Active);
                v.phase = Some(GoalPhase::Planning);
            }
        }
        EventType::SessionStarted => {
            let id = str_field(e, "agentId").or(agent_id);
            if let Some(id) = id {
                let line = agent(v, &id);
                line.engine = str_field(e, "engine").map(|s| sanitize_line(&s));
                line.model_id = str_field(e, "modelId").map(|s| sanitize_line(&s));
                // A window of 0 is no window.
                line.context_tokens = typed::<u32>(e, "contextTokens").filter(|w| *w > 0);
            }
        }
        EventType::TurnStarted => {
            v.route.working_again();
            if let Some(id) = agent_id {
                agent(v, &id).turns += 1;
            }
        }
        // A tool call: its title is the agent's newest words until Cline writes more.
        EventType::ToolSummary => {
            v.route.working_again();
            if let Some(t) = tools.apply(e) {
                let title = tools.rows()[t.index].title.clone();
                if let (Some(id), false) = (agent_id, title.is_empty()) {
                    agent(v, &id).last_summary = title;
                }
            }
        }
        EventType::TurnProgress | EventType::TurnEnded => {
            if e.event_type == EventType::TurnProgress {
                fold_progress(v, e, tools);
            }
            if let (Some(id), Some(text)) = (
                agent_id,
                str_field(e, "text").or_else(|| str_field(e, "summary")),
            ) {
                agent(v, &id).last_summary = sanitize_line(&text);
            }
        }
        EventType::PermissionRequested => {
            if let Some(n) = notice_of(e) {
                v.notices.push(n);
            }
        }
        // A task's own goal (task_id set) is not the run's: only the run-level goal completes or blocks the run.
        EventType::GoalStatus | EventType::GoalBlocked if e.task_id.is_some() => {}
        EventType::GoalStatus => {
            if let Some(s) = typed::<GoalStatus>(e, "status") {
                set_status(v, s, typed::<GoalPhase>(e, "phase"));
            }
        }
        EventType::GoalBlocked => {
            set_status(v, GoalStatus::Blocked, None);
            if v.status == Some(GoalStatus::Blocked) {
                v.blocked = Some(BlockedView {
                    reason: sanitize_line(&str_field(e, "reason").unwrap_or_default()),
                });
            }
        }
        EventType::DeliveryPr => {
            if let Some(url) = str_field(e, "url") {
                v.pr_url = Some(sanitize_line(&url));
            }
        }
        EventType::DeliveryReady => {
            if let (Some(artifact_id), Some(sha256), Some(size_bytes)) = (
                str_field(e, "artifactId"),
                str_field(e, "sha256"),
                typed::<u64>(e, "sizeBytes"),
            ) {
                v.zip = Some(ZipLink {
                    artifact_id: sanitize_line(&artifact_id),
                    sha256: sanitize_line(&sha256),
                    size_bytes,
                });
            }
            if v.pr_url.is_none() {
                v.pr_url = str_field(e, "prUrl").map(|u| sanitize_line(&u));
            }
        }
        EventType::RunCompleted => {
            v.completed = true;
            if let Some(s) = typed::<GoalStatus>(e, "status") {
                set_status(v, s, None);
            }
            // A later `run.completed` that carries no label does not erase the result an earlier one gave.
            if let Some(r) = run_result(e) {
                v.result = Some(r);
            }
            // Nor does one without observations erase the facts an earlier one listed.
            let seen = observations(e);
            if !seen.is_empty() {
                v.observations = seen;
            }
        }
        // The harness cleared the run with it; the view is cleared even before the goal.status
        // page is read.
        EventType::RunAborted => {
            if let Some(status) = str_field(e, "status") {
                set_status(v, GoalStatus::Cleared, None);
                v.aborted = Some(AbortedView {
                    status: sanitize_line(&status),
                    error: str_field(e, "error").map(|x| sanitize_line(&x)),
                    cause: str_field(e, "cause").map(|x| sanitize_line(&x)),
                    stopped_at: str_field(e, "stoppedAt").map(|x| sanitize_line(&x)),
                });
            }
        }
        // Every other type belongs to a line this client no longer folds: accepted, shown nowhere.
        _ => {}
    }
}
