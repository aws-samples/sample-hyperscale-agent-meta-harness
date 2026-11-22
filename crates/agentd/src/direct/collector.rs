//! The collector of the direct driver. It implements [`AcpHandler`]: it records what the engine
//! does and decides nothing. Message chunks become the final message; a tool call becomes a `tool.summary` event and a tool
//! call update becomes `turn.progress`; the latest message goes out as a progress line at most once per 5 s (the TUI shows it
//! as the agent's last words); a permission request, which `--auto-approve true` makes rare, is allowed and recorded.

use crate::acp::{AcpHandler, read_usage};
use crate::direct::classify::clean_message;
use crate::direct::engine_stop::{AttemptFacts, LOOP_GUARD_CALLS};
use crate::direct::hooks::{Event, Hooks, LadderEvent};
use crate::direct::recorder::{
    Anomaly, MESSAGES_KEPT, Recorder, RecorderCounters, StopContext, StopDigest, ToolNote,
};
use crate::events::EventRecorder;
use crate::secrets::SecretSet;
use harness_protocol::common::Usage;
use harness_protocol::direct::{
    DIRECT_TURN, DirectSwitch, NEEDS_INPUT_PAYLOAD_BUDGET, NeedsInputEvidence, NeedsInputPayload,
    NeedsInputReason, PROGRESS_NEEDS_INPUT, tail_chars,
};
use harness_protocol::envelope::EventType;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// The progress line of the latest message goes out at most this often.
pub const MESSAGE_PROGRESS_EVERY: Duration = Duration::from_secs(5);
/// The least time a call must have blocked for the engine's guard on its identical repeats to count as a wait that was sent again.
pub const MIN_WAIT_BLOCK: Duration = Duration::from_secs(5);
/// The most a call must have blocked to count as a wait, however long the session's bound is. The waits that sessions send are shorter
/// than half of a long bound: a rules file of an earlier lifecycle pack names 100 s, models were seen to write 110 s and 115 s, and the default
/// bound is 240 s. Fork patch 0016 holds the same number in the engine's own guard (`MAX_WAIT_THRESHOLD_MS`).
pub const MAX_WAIT_THRESHOLD: Duration = Duration::from_secs(50);
/// How many guard stops on a wait one session of a run is forgiven (the collector lives for one session).
pub const MAX_WAIT_STOPS_FORGIVEN: u32 = 3;
/// The hint word of an engine stop that was forgiven as a wait.
pub const WAIT_STOP_HINT: &str = "wait-repeated";
/// The progress line carries the last this-many characters of the message.
pub const MESSAGE_PROGRESS_CHARS: usize = 2_000;
/// The tool call ids the attempt facts remember as resolved, to count a repeated terminal update once.
const RESOLVED_REMEMBERED: usize = 64;

#[derive(Debug)]
pub struct DirectCollector {
    rec: Arc<EventRecorder>,
    /// The message being written (the text since the last tool call).
    current: String,
    /// The last message before it that had text.
    previous: String,
    tool_calls: u64,
    /// What the attempt in progress did (reset by [`Self::begin_prompt`]).
    facts: AttemptFacts,
    /// Kind and title (as the engine sent them) of the latest tool call of the attempt.
    last_call: Option<(String, String)>,
    /// The latest tool calls whose end was already counted.
    resolved: VecDeque<String>,
    usage: Option<Usage>,
    last_progress: Option<Instant>,
    /// Exact secret values that must not appear in the message or an event: the run's set, read at every redaction, so a secret the
    /// credential store adds after a refresh is replaced like the invocation's own.
    secrets: SecretSet,
    /// The bounded record of the tool calls and messages, redacted at write time.
    recorder: Recorder,
    /// `session/prompt`s begun (the attempts of the run so far).
    prompts_sent: u32,
    /// The engine session of the run (1; 2 after a fresh session).
    session: u32,
    /// When the collector was made, which is when the run started its engine: the age of the run on a notice.
    started: Instant,
    /// The modules of the seam (`crate::direct::hooks`); empty for a run with none.
    hooks: Hooks,
    /// Notified when a tool call ended ([`Self::with_tree_wake`]).
    tree_wake: Option<Arc<tokio::sync::Notify>>,
    /// The engine's profile name, for the words of a notice that name it.
    engine: String,
    /// The least time each call of a run of identical calls must have blocked for the engine's guard on it to count as a wait
    /// that was sent again ([`Self::with_wait_bound`]); `None` forgives nothing.
    wait_threshold: Option<Duration>,
    /// When the calls that are still open started, each with the number of the run of identical calls it belongs to (the newest few).
    open_calls: VecDeque<(String, u64, Instant)>,
    /// The number of the run of identical calls in progress: it moves when a call differs from the one before it and at every prompt.
    tail_no: u64,
    /// The id of the newest call of the run of identical calls in progress.
    tail_newest: Option<String>,
    /// How long the newest call of that run blocked, once it ended. It joins [`Self::tail_shortest`] when a later identical call starts.
    tail_newest_blocked: Option<Duration>,
    /// The shortest time a finished call of that run blocked, its newest call left out.
    tail_shortest: Option<Duration>,
    /// Guard stops on a wait that did not spend the automatic resume, in this session of the run.
    wait_stops_forgiven: u32,
    /// This engine session was already told that a command was cut at another number than the run's limit.
    limit_miss_told: bool,
    /// The watch of the session's storage ([`Self::with_storage_watch`]); `None` for a run that has none.
    storage: Option<crate::direct::storage_watch::StorageWatch>,
}

impl DirectCollector {
    pub fn new(rec: Arc<EventRecorder>) -> Self {
        Self {
            rec,
            current: String::new(),
            previous: String::new(),
            tool_calls: 0,
            facts: AttemptFacts::default(),
            last_call: None,
            resolved: VecDeque::new(),
            usage: None,
            last_progress: None,
            secrets: SecretSet::default(),
            recorder: Recorder::default(),
            prompts_sent: 0,
            session: 1,
            started: Instant::now(),
            hooks: Hooks::none(),
            tree_wake: None,
            engine: String::new(),
            wait_threshold: None,
            open_calls: VecDeque::new(),
            tail_no: 0,
            tail_newest: None,
            tail_newest_blocked: None,
            tail_shortest: None,
            wait_stops_forgiven: 0,
            limit_miss_told: false,
            storage: None,
        }
    }

    /// The watch of the session's storage. A final tool result that names no space left on device wakes it, and the watch makes
    /// a test write of its own; the collector only sets that flag and never waits for the storage. The words of a result bring no
    /// notice by themselves.
    pub fn with_storage_watch(mut self, watch: crate::direct::storage_watch::StorageWatch) -> Self {
        self.storage = Some(watch);
        self
    }

    /// Hands a tool result that named no space left on to the watch of the storage, once.
    fn storage_named(&mut self) {
        if self.recorder.take_space_named()
            && let Some(watch) = &self.storage
        {
            watch.wake(crate::direct::storage_watch::Seen::ToolResult);
        }
    }

    /// The bound of one wait on a background job that the launch carries, in seconds (`engine::direct::guard_wait_bound`). A session
    /// that waits for a long job sends the same bounded wait again while the job runs, and the first engine's guard ends a prompt at the
    /// fifth identical call. Such a stop is no loop: when every finished call of the identical run but its newest blocked for at least the
    /// threshold, the stop does not spend the run's automatic resume ([`Self::forgive_wait_stop`]). The threshold is the larger of
    /// [`MIN_WAIT_BLOCK`] and the smaller of half of this bound and [`MAX_WAIT_THRESHOLD`]. `None` forgives nothing.
    pub fn with_wait_bound(mut self, seconds: Option<u32>) -> Self {
        self.wait_threshold = seconds.map(|s| {
            Duration::from_millis(u64::from(s) * 500)
                .min(MAX_WAIT_THRESHOLD)
                .max(MIN_WAIT_BLOCK)
        });
        self
    }

    /// Whether the attempt ended in the engine's guard on a wait that was sent again: [`LOOP_GUARD_CALLS`] or more identical calls at
    /// its end, of which every one that finished, the newest call left out, blocked for at least the threshold of
    /// [`Self::with_wait_bound`]. The newest call is the one the guard refused: the first engine answers it at once as a completed call
    /// whose command was aborted (live proof of release 5.22), or leaves it open, so it says nothing about a wait. It looks at how long
    /// the calls blocked, never at their text.
    pub fn guard_stop_on_wait(&self) -> bool {
        self.facts.identical_tail >= LOOP_GUARD_CALLS
            && matches!((self.wait_threshold, self.tail_shortest), (Some(least), Some(shortest)) if shortest >= least)
    }

    /// Says once per stop whether the engine's stop is forgiven as a wait that was sent again ([`Self::guard_stop_on_wait`]): at most
    /// [`MAX_WAIT_STOPS_FORGIVEN`] times in this session of the run, so a model that only waits still ends. The count is not stored: a
    /// run that goes on in a later session starts it again.
    pub fn forgive_wait_stop(&mut self) -> bool {
        if self.guard_stop_on_wait() && self.wait_stops_forgiven < MAX_WAIT_STOPS_FORGIVEN {
            self.wait_stops_forgiven += 1;
            true
        } else {
            false
        }
    }

    /// The engine of the run (the profile name): the recorder reads the engine's tool titles and checks its tools by that engine's table.
    pub fn with_engine(mut self, engine: &str) -> Self {
        self.recorder.set_engine(engine);
        self.engine = engine.to_string();
        self
    }

    /// The run's command time limit in milliseconds, for an engine whose cut names the limit that applied: a shell call that named no
    /// limit and was cut at another number is told to the run once per engine session. `None` (the first engine, the fake engine) says nothing.
    pub fn with_command_limit(mut self, limit_ms: Option<u64>) -> Self {
        self.recorder.set_command_limit(limit_ms);
        self
    }

    /// The modules that watch this run. The collector raises their ladder events from the functions that already record them.
    pub fn with_hooks(mut self, hooks: Hooks) -> Self {
        self.hooks = hooks;
        self
    }

    /// `wake` is notified when a tool call ended (completed or failed), which is when the work tree may have changed: the copy task
    /// of the run then looks at the tree without waiting for its timer.
    pub fn with_tree_wake(mut self, wake: Arc<tokio::sync::Notify>) -> Self {
        self.tree_wake = Some(wake);
        self
    }

    /// The modules of the run (cheap to clone; an empty set when none is registered).
    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    /// `session/prompt`s begun so far: the number of the prompt in flight, counted across sessions.
    pub fn prompts_begun(&self) -> u32 {
        self.prompts_sent
    }

    /// Secret values (the session credentials, the nonce, the package token) that are replaced by `<redacted>` in the final
    /// message and in every event text: a set of the collector's own holding these values and no other.
    pub fn with_secrets(self, secrets: Vec<String>) -> Self {
        self.with_secret_set(SecretSet::new(secrets))
    }

    /// The run's shared secret set: what it holds at the moment of each redaction is replaced, so a secret key, session token or nonce
    /// that a credential refresh mints during the run (`CredStore::watch_secrets` adds it) is redacted like the invocation's own.
    pub fn with_secret_set(mut self, secrets: SecretSet) -> Self {
        self.recorder.set_secret_set(secrets.clone());
        self.secrets = secrets;
        self
    }

    fn clean(&self, text: &str) -> String {
        self.secrets.redact(&clean_message(text))
    }

    /// The engine's final message: the text after its last tool call, else the last words before it; control characters removed.
    pub fn final_message(&self) -> String {
        if self.current.trim().is_empty() {
            self.clean(&self.previous)
        } else {
            self.clean(&self.current)
        }
    }

    /// A provider failure ended a try and another follows after `backoff_s` seconds (`turn.progress`, kind `provider-retry`): the
    /// TUI shows it as a line, the run's event log keeps it.
    pub fn record_provider_retry(
        &self,
        attempt: u32,
        of: u32,
        class: &str,
        backoff_s: u64,
        detail: &str,
    ) {
        self.rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "provider-retry",
                "attempt": attempt,
                "of": of,
                "class": class,
                "backoffS": backoff_s,
                "detail": self.clean(detail),
            }),
        );
        if !self.hooks.is_empty() {
            self.hooks.event(&Event::Ladder(LadderEvent::ProviderRetry {
                attempt,
                of,
                class,
                backoff_s,
                detail: &self.clean(detail),
            }));
        }
    }

    /// The engine was switched to another route or model (`turn.progress`, kind `provider-switch`): the TUI shows it as a line, the
    /// run's event log keeps it.
    pub fn record_provider_switch(&self, switch: &DirectSwitch) {
        self.rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "provider-switch",
                "n": switch.n,
                "from": switch.from,
                "to": switch.to,
                "switchKind": switch.kind,
                "class": switch.class,
                "detail": switch.detail.as_deref().map(|d| self.clean(d)),
            }),
        );
        if !self.hooks.is_empty() {
            self.hooks
                .event(&Event::Ladder(LadderEvent::Switch(switch)));
        }
    }

    /// Every model route has been down for `waited_s` seconds and the run goes on waiting and probing until its deadline: raised once per
    /// stretch, after [`crate::direct::failover::OUTAGE_NOTICE_AFTER_S`]. The collector does not read the work tree, so the notice
    /// carries no file counts.
    pub fn record_outage_notice(&self, waited_s: u64) -> String {
        let minutes = waited_s / 60;
        let id = self
            .rec
            .record_urgent_minted(EventType::TurnProgress, |id| {
                bounded_value(self.outage_notice(id, minutes))
            });
        if !self.hooks.is_empty() {
            self.hooks
                .event(&Event::Ladder(LadderEvent::OutageNotice { waited_s }));
        }
        id
    }

    /// The payload of the outage notice whose event id is `id`.
    fn outage_notice(&self, id: &str, minutes: u64) -> NeedsInputPayload {
        NeedsInputPayload {
            turn: DIRECT_TURN,
            kind: PROGRESS_NEEDS_INPUT.into(),
            text: format!(
                "Every model route has been down for {minutes} minutes. The run keeps trying until its time limit (c c stops it)."
            ),
            request_id: id.to_string(),
            n: 0,
            reason: NeedsInputReason::ProviderOutage.as_str().into(),
            title: "Every model route is down".into(),
            stop_outcome: None,
            evidence: NeedsInputEvidence {
                attempts: self.prompts_sent,
                tool_calls: self.tool_calls,
                files_changed: 0,
                identical_calls: None,
                failed_turns: None,
                last_error: None,
                run_s: self.started.elapsed().as_secs(),
                sessions: self.session,
            },
            last_message: None,
            files: Vec::new(),
            files_total: 0,
            blocking: false,
            options: Vec::new(),
            models: Vec::new(),
            question: None,
            recommended: None,
            can_continue: false,
            wait_s: 0,
            expires_at: None,
        }
    }

    /// A prompt succeeded again after the outage notice `request_id`: closes it `input-closed how=recovered`. The
    /// notice takes no answer, so nothing else closes it but this or the end of the run.
    pub fn record_outage_recovered(&self, request_id: &str, waited_s: u64) {
        self.rec.record(
            EventType::TurnProgress,
            crate::direct::waiting::closed_event(request_id, "recovered", waited_s),
        );
        if !self.hooks.is_empty() {
            self.hooks
                .event(&Event::Ladder(LadderEvent::OutageRecovered { waited_s }));
        }
    }

    /// A fact the run says while it goes on (`turn.progress`, kind `notify`), in the worker's own sentence: the event of
    /// [`Self::record_notify`] without its ladder event, as the notice about a late copy is sent. No module hears of it, so none can
    /// turn it into a prompt.
    pub fn record_note(&self, reason: &str, detail: &str) {
        self.rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "notify",
                "reason": reason,
                "outageMinutes": null,
                "detail": self.clean(detail),
            }),
        );
    }

    /// Something the user or the harness must know about while the run goes on or ends (`turn.progress`, kind `notify`): `reason` is
    /// `outage` (every route has been down for `outage_minutes`; the run keeps waiting) or `history_unusable` (the provider refuses the
    /// replayed conversation). The hook of the recovery ladder's "needs your input".
    pub fn record_notify(&self, reason: &str, outage_minutes: Option<u64>, detail: &str) {
        self.rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "notify",
                "reason": reason,
                "outageMinutes": outage_minutes,
                "detail": self.clean(detail),
            }),
        );
        if !self.hooks.is_empty() {
            self.hooks.event(&Event::Ladder(LadderEvent::Notify {
                reason,
                detail: &self.clean(detail),
            }));
        }
    }

    /// Every route has failed and agentd waits `wait_s` seconds before it probes them (`turn.progress`, kind `provider-wait`).
    pub fn record_provider_wait(&self, pass: u32, wait_s: u64, detail: &str) {
        self.rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "provider-wait",
                "pass": pass,
                "waitS": wait_s,
                "detail": self.clean(detail),
            }),
        );
        if !self.hooks.is_empty() {
            self.hooks.event(&Event::Ladder(LadderEvent::Wait {
                pass,
                wait_s,
                detail: &self.clean(detail),
            }));
        }
    }

    /// The run has waited with every route down for the signed ladder's alarm time (`turn.progress`, kind `provider-outage`); sent once
    /// per stretch.
    pub fn record_provider_outage(&self, waited_s: u64) {
        self.rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "provider-outage",
                "waitedS": waited_s,
                "outageMinutes": waited_s / 60,
            }),
        );
        if !self.hooks.is_empty() {
            self.hooks
                .event(&Event::Ladder(LadderEvent::Outage { waited_s }));
        }
    }

    /// A new prompt starts (a resume, a nudge): the final message belongs to the last prompt, so what the earlier prompts said is
    /// dropped, and the facts of the attempt start over.
    pub fn begin_prompt(&mut self) {
        self.recorder.begin_prompt();
        self.prompts_sent = self.prompts_sent.saturating_add(1);
        self.current.clear();
        self.previous.clear();
        self.facts = AttemptFacts::default();
        self.last_call = None;
        self.resolved.clear();
        self.restart_tail();
    }

    /// A new run of identical calls starts: nothing is known about how long its calls blocked.
    fn restart_tail(&mut self) {
        self.tail_no += 1;
        self.tail_newest = None;
        self.tail_newest_blocked = None;
        self.tail_shortest = None;
    }

    /// A call of the run of identical calls in progress, not its newest, blocked for `blocked`.
    fn earlier_call_blocked(&mut self, blocked: Duration) {
        self.tail_shortest = Some(self.tail_shortest.map_or(blocked, |d| d.min(blocked)));
    }

    /// What the attempt in progress (or the one just ended) did: tool calls started, completed, and the failed and identical tails.
    /// The engine's own stops are told apart by these (`crate::direct::engine_stop`); nothing here decides.
    pub fn attempt_facts(&self) -> AttemptFacts {
        self.facts
    }

    /// A tool call ended: count it once, whatever the engine repeats.
    fn resolve(&mut self, id: &str, status: &str) {
        if status != "completed" && status != "failed" {
            return;
        }
        if self.resolved.iter().any(|r| r == id) {
            return;
        }
        if self.resolved.len() >= RESOLVED_REMEMBERED {
            self.resolved.pop_front();
        }
        self.resolved.push_back(id.to_string());
        // How long the call blocked, kept for the run of identical calls it belongs to. The newest call of the run is kept apart:
        // when the engine's guard ends the prompt, that call is the one it refused.
        if let Some(at) = self.open_calls.iter().position(|(open, _, _)| open == id)
            && let Some((_, tail, started)) = self.open_calls.remove(at)
            && tail == self.tail_no
        {
            let blocked = started.elapsed();
            if self.tail_newest.as_deref() == Some(id) {
                self.tail_newest_blocked = Some(blocked);
            } else {
                self.earlier_call_blocked(blocked);
            }
        }
        // The tool call may have changed the work tree; the copy task looks.
        if let Some(wake) = &self.tree_wake {
            wake.notify_one();
        }
        if status == "completed" {
            self.facts.completed += 1;
            self.facts.failed_tail = 0;
        } else {
            self.facts.failed_tail = self.facts.failed_tail.saturating_add(1);
        }
    }

    /// A tool call started: count it, and extend or restart the run of identical calls (kind and title as sent, before any secret is
    /// hidden: two commands that print the same secret are two commands).
    fn start_call(&mut self, update: &Value) {
        self.facts.tool_calls += 1;
        let key = (
            update
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
        if self.last_call.as_ref() == Some(&key) {
            self.facts.identical_tail = self.facts.identical_tail.saturating_add(1);
            // The call before this one is no longer the newest of the run: its time is judged with the others'.
            if let Some(blocked) = self.tail_newest_blocked.take() {
                self.earlier_call_blocked(blocked);
            }
        } else {
            self.facts.identical_tail = 1;
            self.last_call = Some(key);
            self.restart_tail();
        }
        let id = update.get("toolCallId").and_then(Value::as_str);
        self.tail_newest = id.map(str::to_string);
        if let Some(id) = id {
            if self.open_calls.len() >= RESOLVED_REMEMBERED {
                self.open_calls.pop_front();
            }
            self.open_calls
                .push_back((id.to_string(), self.tail_no, Instant::now()));
        }
    }

    /// The engine stopped the run itself and one more try follows (`turn.progress`, kind `engine-stop`): what the attempt looked like,
    /// the model that stays, and how long agentd waits.
    pub fn record_engine_stop(
        &self,
        hint: &str,
        attempt: u32,
        of: u32,
        backoff_s: u64,
        detail: &str,
    ) {
        self.rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "engine-stop",
                "hint": hint,
                "attempt": attempt,
                "of": of,
                "backoffS": backoff_s,
                "detail": self.clean(detail),
            }),
        );
        if !self.hooks.is_empty() {
            self.hooks.event(&Event::Ladder(LadderEvent::EngineStop {
                hint,
                attempt,
                of,
                backoff_s,
                detail: &self.clean(detail),
            }));
        }
    }

    /// The engine's guard ended a prompt on a wait that was sent again and the stop is not counted:
    /// the prompt is resumed after `backoff_s`.
    pub fn record_wait_stop(&self, backoff_s: u64, detail: &str) {
        self.rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "engine-stop",
                "hint": WAIT_STOP_HINT,
                "backoffS": backoff_s,
                "detail": self.clean(detail),
            }),
        );
        if !self.hooks.is_empty() {
            self.hooks.event(&Event::Ladder(LadderEvent::EngineStop {
                hint: WAIT_STOP_HINT,
                attempt: 0,
                of: 0,
                backoff_s,
                detail: &self.clean(detail),
            }));
        }
    }

    /// The retry loop drops the answer of the try it just made and sends another: the modules are told, so that one which matches
    /// engine runs to answered prompts takes that prompt's window back. Nothing is recorded in the run's events.
    pub fn record_answer_dropped(&self) {
        if !self.hooks.is_empty() {
            self.hooks.event(&Event::Ladder(LadderEvent::AnswerDropped));
        }
    }

    /// The last 30 tool calls as title and status, oldest first. The status is the recorder's reading of the result, so a command that
    /// exited non-zero or hit the 30 s shell cut is `failed` or `timed_out`, not `completed`.
    pub fn tool_notes(&self) -> Vec<ToolNote> {
        self.recorder.tool_notes()
    }

    /// The last 3 messages of the engine, oldest first, redacted; the message being written counts as the newest.
    pub fn last_messages(&self) -> Vec<String> {
        let mut messages = self.recorder.last_messages();
        let current = self.recorder.message_text(&self.current);
        if !current.is_empty() && messages.last() != Some(&current) {
            messages.push(current);
        }
        let skip = messages.len().saturating_sub(MESSAGES_KEPT);
        messages.split_off(skip)
    }

    /// The tail of the last failed test-like call, when its output was kept.
    pub fn failed_test_tail(&self) -> Option<String> {
        self.recorder.failed_test_tail()
    }

    /// The error text of the newest failed call, on one line, at most 200 characters.
    pub fn last_error(&self) -> Option<String> {
        self.recorder.last_error()
    }

    /// The anomaly lines the harness computed about the tool inputs still in the ring.
    pub fn anomalies(&self) -> Vec<Anomaly> {
        self.recorder.anomalies()
    }

    /// The run-wide counters the recorder adds to [`Self::attempt_facts`].
    pub fn recorder_counters(&self) -> RecorderCounters {
        self.recorder.counters()
    }

    /// The digest of stop number `n` (at most 64 KiB); the driver writes it to S3 at every stop. `ctx.final_message` defaults to the
    /// message being written.
    pub fn stop_digest(&self, n: u32, mut ctx: StopContext) -> StopDigest {
        if ctx.final_message.is_none() {
            ctx.final_message = Some(self.final_message());
        }
        self.recorder.stop_digest(n, &self.facts, &ctx)
    }

    /// A question for the user (`turn.progress`, kind `needs-input`) whose event id IS the request id, flushed at once.
    pub fn record_needs_input(&self, id: &str, payload: Value) {
        self.rec
            .record_urgent_with_id(EventType::TurnProgress, id, payload);
    }

    /// [`record_needs_input`](Self::record_needs_input) for a question whose id is made at the append: the id goes to `build`, which
    /// makes the payload, and no event of this run can sort between the id and the question. Returns the id (the request id).
    pub fn record_needs_input_minted(&self, build: impl FnOnce(&str) -> Value) -> String {
        self.rec
            .record_urgent_minted(EventType::TurnProgress, build)
    }

    /// The engine session the run is in (1 for the first, 2 after a fresh session); the outage notice and the card count it.
    pub fn set_session(&mut self, session: u32) {
        self.session = session.max(1);
        // A new engine session is told again when its limit is not applied.
        self.limit_miss_told = false;
    }

    pub fn tool_calls(&self) -> u64 {
        self.tool_calls
    }

    /// The latest usage the engine streamed; `None` when it reported none.
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    fn progress_if_due(&mut self) {
        let due = self
            .last_progress
            .is_none_or(|t| t.elapsed() >= MESSAGE_PROGRESS_EVERY);
        if !due {
            return;
        }
        self.last_progress = Some(Instant::now());
        let text = self.clean(&self.current);
        self.rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "message",
                "text": tail_chars(&text, MESSAGE_PROGRESS_CHARS),
            }),
        );
    }
}

/// The payload cut to the inline event limit (`NEEDS_INPUT_PAYLOAD_BUDGET`) as a JSON value.
fn bounded_value(payload: NeedsInputPayload) -> Value {
    payload.bounded(NEEDS_INPUT_PAYLOAD_BUDGET).to_value()
}

/// The JSON-RPC result that allows a permission request: the first `allow_once` or `allow_always` option, else cancelled.
pub fn allow_once(params: &Value) -> Value {
    let pick = ["allow_once", "allow_always"].iter().find_map(|kind| {
        params
            .get("options")
            .and_then(Value::as_array)?
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
            .and_then(|o| o.get("optionId").cloned())
    });
    match pick {
        Some(id) => json!({ "outcome": { "outcome": "selected", "optionId": id } }),
        None => json!({ "outcome": { "outcome": "cancelled" } }),
    }
}

impl AcpHandler for DirectCollector {
    fn on_notification(&mut self, method: &str, params: &Value) {
        if method != "session/update" {
            return;
        }
        let update = params.get("update").unwrap_or(&Value::Null);
        match update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "agent_message_chunk" => {
                if let Some(t) = update.pointer("/content/text").and_then(Value::as_str) {
                    self.current.push_str(t);
                    self.progress_if_due();
                }
            }
            "tool_call" => {
                self.tool_calls += 1;
                self.start_call(update);
                if !self.current.trim().is_empty() {
                    self.recorder.on_message(&self.current);
                }
                self.recorder.on_tool_call(update);
                self.storage_named();
                if let (Some(id), Some(status)) = (
                    update.get("toolCallId").and_then(Value::as_str),
                    update.get("status").and_then(Value::as_str),
                ) {
                    let (id, status) = (id.to_string(), status.to_string());
                    self.resolve(&id, &status);
                }
                if self.current.trim().is_empty() {
                    self.current.clear();
                } else {
                    self.previous = std::mem::take(&mut self.current);
                }
                let title = update
                    .get("title")
                    .and_then(Value::as_str)
                    .map(|t| self.clean(t));
                self.rec.record(
                    EventType::ToolSummary,
                    json!({
                        "toolCallId": update.get("toolCallId"),
                        "title": title,
                        "kind": update.get("kind"),
                        "status": update.get("status"),
                    }),
                );
            }
            "tool_call_update" => {
                self.recorder.on_tool_call_update(update);
                self.storage_named();
                if let Some(miss) = self.recorder.take_limit_miss()
                    && !self.limit_miss_told
                {
                    self.limit_miss_told = true;
                    self.record_notify(
                        crate::engine::direct::LIMIT_REASON,
                        None,
                        &crate::engine::direct_words::limit_not_applied(
                            &self.engine,
                            miss.cut_ms / 1000,
                            miss.limit_ms / 1000,
                        ),
                    );
                }
                if let (Some(id), Some(status)) = (
                    update.get("toolCallId").and_then(Value::as_str),
                    update.get("status").and_then(Value::as_str),
                ) {
                    let (id, status) = (id.to_string(), status.to_string());
                    self.resolve(&id, &status);
                }
                self.rec.record(
                    EventType::TurnProgress,
                    json!({
                        "toolCallId": update.get("toolCallId"),
                        "status": update.get("status"),
                    }),
                );
            }
            _ => {}
        }
        if let Some(u) = read_usage(update) {
            self.usage = Some(u);
        }
    }

    fn on_request(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        if method != "session/request_permission" {
            return Err((-32601, format!("method {method} is not offered by agentd")));
        }
        // The VM is the boundary: with `--auto-approve true` none is expected, and one that arrives is allowed.
        self.rec.record(
            EventType::PermissionRequested,
            json!({
                "toolCallId": params.pointer("/toolCall/toolCallId").and_then(Value::as_str).unwrap_or_default(),
                "allowed": true,
                "auto": true,
                "title": params
                    .pointer("/toolCall/title")
                    .and_then(Value::as_str)
                    .map(|t| self.clean(t)),
            }),
        );
        Ok(allow_once(params))
    }
}
