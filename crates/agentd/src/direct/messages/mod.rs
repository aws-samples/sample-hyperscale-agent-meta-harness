//! Module `messages`: the user's messages to a running engine.
//!
//! agentd holds the queue and sends exactly ONE `session/prompt` at a time. The pinned engine has an internal queue of its own and it
//! is not used: a second prompt while one is open is queued silently, answered at once with `end_turn` and usage 0, and a
//! `session/cancel` after that does nothing. So a message waits here until the prompt in flight has ended, and then goes out as the
//! next prompt, one message per turn, in the order the control plane gave (`now` first).
//!
//! What the module does, by hook (the table is in `AGENTS.md`):
//!
//! - `on_controls` / [`MessagesHook::offer`]: the hand-out names messages (never their text). They enter the [`Queue`]; a repeated
//!   hand-out is processed once. A `Cancel` in the batch wins: none of its messages is taken.
//! - `on_turn_end`: the next message is **claimed** with the control plane (one conditional step, fenced by the worker generation and
//!   the session) and only then returned as `TurnVote::Prompt(text)`. A claim that loses (withdrawn, held, cancelled) costs nothing.
//!   The module is registered last, so a pending message beats another module's continuation, and a `Stop` of an earlier module is
//!   final before anything is claimed. A message pending when the engine ends its turn becomes the next prompt instead of the run being
//!   declared done.
//! - `on_seal`: the one atomic "anything pending? if not, the run is done". Pending messages come back as the next prompt; none means
//!   admission is closed and nothing more is accepted (the control plane answers 409 run-sealed).
//! - `on_event`: `SessionStarted` is the epoch (a model change is not a new session); `PromptSent` with the claimed text is the
//!   acknowledgement (`delivered (turn k)`); `PromptReplied` feeds the tripwire and the allowance; `EngineStopped` reaps tool processes.
//! - `/now`: [`MessagesHook::offer`] of a `now` message while a prompt is open claims it, then `DriverCtl::interrupt()` (one
//!   `session/cancel`; the prompt's own answer is awaited by the client, 0.19 to 0.48 s measured), then at the stage's end the module
//!   reaps the engine's tool processes (a cancel leaves orphans) and votes the message as the new prompt. If the engine does not answer
//!   within the 10 s grace (or processes of the engine survive the kill), the message is demoted to `next` and keeps its claim; the user
//!   is told "could not interrupt; delivered after this turn" (`turn.progress`, kind `notify`), the engine and every process of its
//!   family are killed, the driver's own ladder starts a fresh session, and the message goes out first there, once, with no
//!   confirmation. The interrupt counter and the rule that an interrupt stop opens no question are unchanged.
//! - The user's cancel stays final and separate from the interrupt: a vote that comes back after it is dropped by the driver; this
//!   module claims nothing once it is up and closes the run for the cancel.
//! - `on_shutdown` / `on_finalize`: every ending. A claimed message that never went out ends failed (not sent), admission closes
//!   with a terminal seal (pending and held messages end rejected), and the engine's leftover processes are reaped before the end
//!   commit and the bundle.
//! - Tripwire: a reply of `end_turn` with usage 0, no update since the prompt and an answer within moments is the engine's queue, never
//!   a done report. The engine is replaced (processes reaped, a fresh session follows) and no `/now` relies on its cancel. When
//!   the reap cannot vouch that the engine's processes are gone the run stops instead of sending a prompt beside them.
//! - Continuation: after the first done-looking stop 300 s of the run's time must remain for another message turn; a message that finds
//!   less ends rejected (`run-sealed`) and the run is sealed. No count of turns and no limit on extra work apply by default. A stage may
//!   still set `AGENTD_MESSAGES_MAX_TURNS` and `AGENTD_MESSAGES_MAX_EXEC_S`: a message beyond them ends rejected (`continuation-limit`) and the
//!   run is sealed. The time is a boundary check: no new turn starts once it is spent, and the turn in flight is not cut short.
//!
//! What the module owes the control plane stays owned until it is settled: a claim is
//! checked against the session and the clock when the message is about to go out (not when the turn ended), a claim that comes back after
//! the session changed is given back, an interrupt is bound to the prompt it was aimed at, an outcome that could not be posted is kept
//! and tried again at the seal and at the close, and the seal waits for the acknowledgement of a send (the wire refuses a normal seal
//! while a message is dispatching).
//!
//! Settings come from `AGENTD_MESSAGES_*` ([`Settings::from_env`]); `AGENTD_MESSAGES=off` turns the module off. Nothing here says a
//! run is verified: the harness records how the engine stopped.

pub mod http;
pub mod machine;
pub mod reap;
pub mod store;
pub mod types;

pub use machine::{Budget, Offered, Queue, Verdict};
pub use reap::{Reaper, TAG_ENV};
pub use store::{ConfirmRefused, MemoryStore, MessageStore, Op, WithdrawRefused};
pub use types::{
    ClaimOutcome, FailReason, Held, MAX_TEXT_BYTES, Message, Mode, Notice, Refusal, RejectReason,
    SealMode, SealOutcome, State, StoreError,
};

use crate::BoxFuture;
use crate::acp::{Dir, Watched};
use crate::direct::hooks::{
    DirectHook, DriverCtl, EngineStop, Event, Finalize, HookEnv, Shutdown, StartupCtx, TurnEnd,
    TurnVote, Wire,
};
use crate::direct::retry::RESUME_PROMPT_INTERRUPTED;
use crate::events::EventRecorder;
use crate::logging::LogSink;
use harness_protocol::api::internal::PendingControl;
use harness_protocol::direct::DIRECT_TURN;
use harness_protocol::envelope::EventType;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc, oneshot, watch};

// ---- settings -----------------------------------------------------------------------------------------------------------------------

/// The numbers of the module.
#[derive(Clone, Debug)]
pub struct Settings {
    /// `AGENTD_MESSAGES=off` turns the module off for a run.
    pub enabled: bool,
    /// Message-driven turns after the first done-looking stop. `None`: no count; `AGENTD_MESSAGES_MAX_TURNS` sets one.
    pub max_turns: Option<u32>,
    /// Extra agent execution after the first done-looking stop. `None`: no limit; `AGENTD_MESSAGES_MAX_EXEC_S` sets one.
    pub max_exec: Option<Duration>,
    /// Run time that must remain for another turn. The driver's deadline already holds the 900 s reserve for delivery.
    pub min_left: Duration,
    /// Interrupts (`/now`) per run.
    pub interrupts: u32,
    /// How long the module waits for the engine's tool processes to end after a cancel.
    pub quiesce: Duration,
    /// Hand-outs kept; the rest stay eligible on the server and come again.
    pub queue_cap: usize,
    /// One call to the store.
    pub store_timeout: Duration,
    /// Attempts per call; only a retryable failure is repeated.
    pub retries: u32,
    pub retry_pause: Duration,
    /// A reply this soon after its prompt, with no update, no usage and `end_turn`, is the engine's queue (the tripwire).
    pub trip_window: Duration,
    /// One decision of the module (a turn end or the seal) stops looking for another message after this long; the driver cuts a module
    /// off at 120 s ([`crate::direct::hooks::Limits`]).
    pub decision_cap: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_turns: None,
            max_exec: None,
            min_left: Duration::from_secs(300),
            interrupts: 5,
            quiesce: Duration::from_secs(10),
            queue_cap: harness_protocol::messages::MAX_OUTSTANDING_MESSAGES as usize,
            store_timeout: Duration::from_secs(8),
            retries: 3,
            retry_pause: Duration::from_millis(200),
            trip_window: Duration::from_secs(2),
            decision_cap: Duration::from_secs(90),
        }
    }
}

impl Settings {
    /// Short pauses and bounds, for tests that run a whole agent in a few seconds.
    pub fn for_tests() -> Self {
        Self {
            quiesce: Duration::from_secs(5),
            store_timeout: Duration::from_secs(5),
            retry_pause: Duration::from_millis(2),
            ..Self::default()
        }
    }

    /// The settings of a run from `AGENTD_MESSAGES*` variables; a value that does not parse is ignored.
    pub fn from_env(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let mut s = Self::default();
        let num = |k: &str| get(k).and_then(|v| v.trim().parse::<u64>().ok());
        if get("AGENTD_MESSAGES").is_some_and(|v| matches!(v.trim(), "off" | "0" | "false")) {
            s.enabled = false;
        }
        if let Some(n) = num("AGENTD_MESSAGES_MAX_TURNS") {
            s.max_turns = u32::try_from(n).ok().or(s.max_turns);
        }
        if let Some(n) = num("AGENTD_MESSAGES_MAX_EXEC_S") {
            s.max_exec = Some(Duration::from_secs(n));
        }
        if let Some(n) = num("AGENTD_MESSAGES_MIN_LEFT_S") {
            s.min_left = Duration::from_secs(n);
        }
        if let Some(n) = num("AGENTD_MESSAGES_INTERRUPTS") {
            s.interrupts = u32::try_from(n).unwrap_or(s.interrupts);
        }
        if let Some(n) = num("AGENTD_MESSAGES_QUIESCE_S") {
            s.quiesce = Duration::from_secs(n);
        }
        if let Some(n) = num("AGENTD_MESSAGES_QUEUE") {
            s.queue_cap = usize::try_from(n).unwrap_or(s.queue_cap).max(1);
        }
        s
    }
}

// ---- what the module is wired to -------------------------------------------------------------------------------------------------

/// The parts of the run the module uses: the driver's interrupt, the user's cancel, the log, and the reaper of the engine's family.
/// `on_startup` makes it from the run's [`HookEnv`]; a test makes its own.
pub struct Wiring {
    pub ctl: DriverCtl,
    pub cancel: watch::Receiver<bool>,
    pub log: Arc<dyn LogSink>,
    pub reaper: Option<Reaper>,
    /// The run's event recorder: the notice that an interrupt could not cancel the turn is written to it (`turn.progress`, kind `notify`).
    pub events: Option<Arc<EventRecorder>>,
}

impl std::fmt::Debug for Wiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wiring")
            .field("reaper", &self.reaper)
            .finish_non_exhaustive()
    }
}

impl Wiring {
    pub fn new(ctl: DriverCtl, cancel: watch::Receiver<bool>, log: Arc<dyn LogSink>) -> Self {
        Self {
            ctl,
            cancel,
            log,
            reaper: None,
            events: None,
        }
    }

    pub fn with_reaper(mut self, r: Reaper) -> Self {
        self.reaper = Some(r);
        self
    }

    pub fn with_events(mut self, rec: Arc<EventRecorder>) -> Self {
        self.events = Some(rec);
        self
    }
}

/// What a test can count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub claims: u32,
    pub delivered: u32,
    pub interrupts: u32,
    pub released: u32,
    pub rejected: u32,
    pub tripwires: u32,
}

// ---- state ------------------------------------------------------------------------------------------------------------------------

/// The message whose prompt went out and has not been answered.
struct Sent {
    id: String,
    text: String,
    session: String,
    at: Instant,
}

/// An outcome the control plane has not been told yet. It stays owned by the module until a call succeeds or the server refuses it for
/// good; the seal and the close try again.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Owed {
    Submitted(String, u32),
    Failed(String, FailReason),
    Release(String),
    Reject(String, RejectReason),
}

impl Owed {
    fn id(&self) -> &str {
        match self {
            Self::Submitted(id, _)
            | Self::Failed(id, _)
            | Self::Release(id)
            | Self::Reject(id, _) => id,
        }
    }
}

struct Run {
    epoch: Option<String>,
    epoch_no: u32,
    /// The epoch the store has been told.
    reported: Option<String>,
    queue: Queue,
    /// Claimed and not yet handed to a vote (the interrupt's message waits here for the stage to end).
    ready: VecDeque<Message>,
    /// Returned in a `Prompt` vote; its `PromptSent` has not been seen.
    voted: Option<Message>,
    sent: Option<Sent>,
    prompt_at: Option<Instant>,
    /// The number of the latest prompt written (`PromptSent.no`): an interrupt is aimed at the prompt that was open when it was asked.
    prompt_no: u32,
    interrupts: u32,
    interrupting: Option<String>,
    quarantined: bool,
    cancel_unsafe: bool,
    tripped: bool,
    budget: Budget,
    sealed: bool,
    terminal: bool,
    user_cancel: bool,
    /// The close has started: no interrupt starts, and a claim that comes back is left for the close to settle.
    closing: bool,
    owed: Vec<Owed>,
    /// A claim call ended without an answer: the control plane may hold a message this module does not know. The seal and the close ask
    /// what the module holds (a claim without an id) before they go on.
    claim_unsure: bool,
    /// Messages that were voted as a prompt (or taken over), so a repeated claim of one is never sent a second time.
    handled: HashSet<String>,
    /// Interrupt messages whose cancel the engine did not acknowledge: demoted to `next`, the claim kept across the restart of the engine,
    /// and sent first in the new session.
    demoted: HashSet<String>,
    stats: Stats,
}

enum Job {
    Epoch(String, u32),
    Submitted(String, u32),
    Release(String),
    Barrier(oneshot::Sender<()>),
}

struct Inner {
    store: Arc<dyn MessageStore>,
    settings: Settings,
    tag: String,
    st: Mutex<Run>,
    wiring: OnceLock<Wiring>,
    jobs: mpsc::UnboundedSender<Job>,
    jobs_rx: Mutex<Option<mpsc::UnboundedReceiver<Job>>>,
    /// The note that the store cannot serve this run was written.
    noted: AtomicBool,
    /// What the first call that learned it said.
    unavailable_because: Mutex<Option<String>>,
    /// Interrupts (`now`) the run may make: the stage's count, or the request's `limits.interrupts` once the run's request is known.
    interrupt_cap: AtomicU32,
    /// `session/update` messages from the engine since the latest prompt was sent.
    updates: AtomicU32,
    /// Claims an interrupt is making; a turn end waits for them so a message claimed a moment ago is not lost.
    claiming: AtomicU32,
    claim_done: Notify,
    /// Interrupts are served one at a time, in the order the control plane gave.
    gate: tokio::sync::Mutex<()>,
    /// Tests only: the next question whether a prompt is open is answered yes ([`MessagesHook::pretend_prompt_open`]).
    pretend_open: AtomicBool,
}

/// The module.
pub struct MessagesHook {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for MessagesHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessagesHook")
            .field("tag", &self.inner.tag)
            .finish_non_exhaustive()
    }
}

/// The module for a run, or `None` when `AGENTD_MESSAGES=off`. `Hooks::standard` calls it once per run. The store is the private internal
/// API ([`http::HttpMessageStore`]); a plane that serves no message routes answers 404 and the module stays out of that run.
pub fn hook(env: &HookEnv) -> Option<Arc<dyn DirectHook>> {
    let settings = Settings::from_env(&|k| std::env::var(k).ok());
    if !settings.enabled {
        return None;
    }
    let transport = http::SignedTransport::new(
        env.creds.clone(),
        env.internal_api_url.clone(),
        env.region.clone(),
    );
    let store = http::HttpMessageStore::new(
        Arc::new(transport),
        &env.run_id,
        &env.agent_id,
        &http::new_generation(),
    );
    let hook: Arc<dyn DirectHook> = MessagesHook::new(Arc::new(store), settings);
    Some(hook)
}

impl MessagesHook {
    pub fn new(store: Arc<dyn MessageStore>, settings: Settings) -> Arc<Self> {
        let (jobs, rx) = mpsc::unbounded_channel();
        let budget = Budget::new(settings.max_turns, settings.max_exec, settings.min_left);
        let queue = Queue::new(settings.queue_cap);
        let interrupt_cap = AtomicU32::new(settings.interrupts);
        Arc::new(Self {
            inner: Arc::new(Inner {
                store,
                settings,
                tag: Reaper::new_tag(),
                st: Mutex::new(Run {
                    epoch: None,
                    epoch_no: 0,
                    reported: None,
                    queue,
                    ready: VecDeque::new(),
                    voted: None,
                    sent: None,
                    prompt_at: None,
                    prompt_no: 0,
                    interrupts: 0,
                    interrupting: None,
                    quarantined: false,
                    cancel_unsafe: false,
                    tripped: false,
                    budget,
                    sealed: false,
                    terminal: false,
                    user_cancel: false,
                    closing: false,
                    owed: Vec::new(),
                    claim_unsure: false,
                    handled: HashSet::new(),
                    demoted: HashSet::new(),
                    stats: Stats::default(),
                }),
                wiring: OnceLock::new(),
                jobs,
                jobs_rx: Mutex::new(Some(rx)),
                noted: AtomicBool::new(false),
                unavailable_because: Mutex::new(None),
                interrupt_cap,
                updates: AtomicU32::new(0),
                claiming: AtomicU32::new(0),
                claim_done: Notify::new(),
                gate: tokio::sync::Mutex::new(()),
                pretend_open: AtomicBool::new(false),
            }),
        })
    }

    /// Wires the module to a run and starts its worker (the background calls to the store). Once; a second call changes nothing.
    /// `on_startup` does it; a test calls it itself. Needs a running tokio runtime.
    pub fn bind(&self, w: Wiring) {
        if self.inner.wiring.set(w).is_ok() {
            let rx = self
                .inner
                .jobs_rx
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            if let Some(rx) = rx {
                tokio::spawn(worker(self.inner.clone(), rx));
            }
        }
    }

    /// The tag that marks the engine's processes (`AGENTD_ENGINE_TAG`).
    pub fn tag(&self) -> &str {
        &self.inner.tag
    }

    pub fn stats(&self) -> Stats {
        self.inner.lock().stats
    }

    /// The request's `limits` member for this run: `interrupts` replaces the stage's count of interrupts; a member left out keeps it.
    /// `0` allows none, it never means without limit.
    pub fn apply_limits(&self, limits: &harness_protocol::direct::RunLimits) {
        if let Some(n) = limits.interrupts {
            self.inner.interrupt_cap.store(n, Ordering::SeqCst);
        }
    }

    /// The interrupts this run may make.
    pub fn interrupt_cap(&self) -> u32 {
        self.inner.interrupt_cap.load(Ordering::SeqCst)
    }

    /// The allowance used so far: message-driven turns started and the time they ran.
    pub fn allowance(&self) -> (u32, Duration) {
        let st = self.inner.lock();
        (st.budget.turns(), st.budget.exec())
    }

    /// For tests, which have no driver: the next time the module asks whether a prompt is open it is told yes (once). The driver's
    /// [`DriverCtl`] of a test is not connected to a client, so it never reports an open prompt.
    pub fn pretend_prompt_open(&self, open: bool) {
        self.inner.pretend_open.store(open, Ordering::SeqCst);
    }

    /// Takes a hand-out (the messages the control plane lists as eligible). Returns what the queue did with it.
    pub fn offer(&self, notices: Vec<Notice>) -> Offered {
        self.inner.offer(false, notices)
    }

    /// As [`offer`](Self::offer) for a batch of controls: a `Cancel` in the batch wins and none of its messages is taken.
    /// Returns how many were queued.
    pub fn offer_batch(&self, cancel: bool, notices: Vec<Notice>) -> usize {
        self.inner.offer(cancel, notices).added
    }

    /// Waits until the background calls made so far have reached the store.
    pub async fn flush(&self) {
        self.inner.flush().await;
    }

    /// The end of the run for the messages: waits for a claim an interrupt is making, settles what was claimed and never sent and every
    /// outcome still owed, seals the run for good (pending and held messages end rejected, nothing more is accepted) and reaps what the
    /// engine left. Idempotent; bounded.
    pub async fn close(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(12), self.inner.close()).await;
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Run> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn log(&self, line: &str) {
        if let Some(w) = self.wiring.get() {
            w.log.line(&format!("direct messages: {line}"));
        }
    }

    /// A call failed. When the store has learned it cannot serve this run the reason is kept for the one note the close writes (there, so
    /// that the note has the same place in the run's log every time, whichever call learned it first); otherwise the failure is logged.
    fn failed(&self, what: &str, e: &StoreError) {
        if self.store.unavailable() {
            let mut why = self
                .unavailable_because
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if why.is_none() {
                *why = Some(e.to_string());
            }
        } else {
            self.log(&format!("{what} failed: {e}"));
        }
    }

    /// The one note that the store cannot serve this run, if it learned so and the note was not written yet.
    fn note_unavailable(&self) {
        if !self.store.unavailable() || self.noted.swap(true, Ordering::SeqCst) {
            return;
        }
        let why = self
            .unavailable_because
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .unwrap_or_else(|| "the store said so".to_string());
        self.log(&format!(
            "the control plane cannot serve message control for this run ({why}); the module stayed out of it"
        ));
    }

    /// The module is wired to a run and the store can serve it.
    fn usable(&self) -> bool {
        self.wiring.get().is_some() && !self.store.unavailable()
    }

    fn cancelled(&self) -> bool {
        self.lock().user_cancel || self.wiring.get().is_some_and(|w| *w.cancel.borrow())
    }

    fn job(&self, j: Job) {
        let _ = self.jobs.send(j);
    }

    /// Whether a `session/prompt` is open now (the driver's answer; a test's pretence is used up by the first question).
    fn prompt_open_now(&self) -> bool {
        self.pretend_open.swap(false, Ordering::SeqCst)
            || self.wiring.get().is_some_and(|w| w.ctl.prompt_open())
    }

    fn offer(self: &Arc<Self>, cancel: bool, notices: Vec<Notice>) -> Offered {
        if cancel {
            self.lock().user_cancel = true;
            return Offered::default();
        }
        let (r, flows, aimed) = {
            let mut st = self.lock();
            if st.sealed || st.terminal || st.user_cancel || st.closing {
                return Offered::default();
            }
            // Only a `now` message that is new to the queue, and only while a prompt is open, starts an interrupt.
            let flows: Vec<String> = notices
                .iter()
                .filter(|n| n.mode == Mode::Now && !st.queue.knows(&n.id))
                .map(|n| n.id.clone())
                .collect();
            let r = st.queue.offer(notices);
            (
                r,
                flows
                    .into_iter()
                    .filter(|id| st.queue.contains(id))
                    .collect::<Vec<_>>(),
                st.prompt_no,
            )
        };
        if r.added > 0 && !flows.is_empty() && self.prompt_open_now() {
            for id in flows {
                let me = self.clone();
                tokio::spawn(async move { me.interrupt_flow(id, aimed).await });
            }
        }
        r
    }

    // ---- calls to the store, with their retries -------------------------------------------------------------------------------

    /// Makes `f` for each attempt; a retryable failure is repeated after a short pause, a timeout counts as retryable.
    async fn call<'a, T>(
        &self,
        what: &str,
        f: impl Fn() -> BoxFuture<'a, Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        let attempts = self.settings.retries.max(1);
        let mut last = StoreError::fatal("no attempt");
        for attempt in 1..=attempts {
            match tokio::time::timeout(self.settings.store_timeout, f()).await {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(e)) if e.retryable => last = e,
                Ok(Err(e)) => return Err(e),
                Err(_) => last = StoreError::retryable(format!("{what} timed out")),
            }
            if attempt < attempts {
                tokio::time::sleep(self.settings.retry_pause * attempt).await;
            }
        }
        Err(last)
    }

    /// One attempt, bounded by a few seconds: the calls of the close, which has a budget for all of them.
    async fn quick<'a, T>(
        &self,
        what: &str,
        f: impl Fn() -> BoxFuture<'a, Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        let bound = self.settings.store_timeout.min(Duration::from_secs(3));
        match tokio::time::timeout(bound, f()).await {
            Ok(r) => r,
            Err(_) => Err(StoreError::retryable(format!("{what} timed out"))),
        }
    }

    async fn attempt<'a, T>(
        &self,
        quick: bool,
        what: &str,
        f: impl Fn() -> BoxFuture<'a, Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        if quick {
            self.quick(what, f).await
        } else {
            self.call(what, f).await
        }
    }

    async fn flush(&self) {
        self.flush_for(self.settings.store_timeout * 2).await;
    }

    async fn flush_for(&self, bound: Duration) {
        let (tx, rx) = oneshot::channel();
        self.job(Job::Barrier(tx));
        let _ = tokio::time::timeout(bound, rx).await;
    }

    /// Tells the store the session before a claim: the server commits the epoch before any later claim.
    async fn sync_epoch(&self) -> Option<String> {
        let (epoch, number, reported) = {
            let st = self.lock();
            (st.epoch.clone()?, st.epoch_no, st.reported.clone())
        };
        if reported.as_deref() != Some(epoch.as_str()) {
            match self
                .call("epoch", || self.store.report_epoch(&epoch, number))
                .await
            {
                Ok(()) => self.lock().reported = Some(epoch.clone()),
                Err(e) => self.failed("reporting the session", &e),
            }
        }
        Some(epoch)
    }

    // ---- outcomes the module owes ---------------------------------------------------------------------------------------------

    /// Keeps an outcome that could not be posted, to be tried again at the seal and at the close; a refusal for good, or a store that
    /// cannot serve the run, is only logged.
    fn owe_or_log(&self, o: Owed, what: &str, e: &StoreError) {
        if e.retryable && !self.store.unavailable() {
            let mut st = self.lock();
            if !st.owed.contains(&o) {
                st.owed.push(o);
            }
            drop(st);
            self.log(&format!("{what} not posted yet; it is tried again: {e}"));
        } else {
            self.failed(what, e);
        }
    }

    /// The claim of a message that will not be sent now goes back (the message stays pending, or is held in another session).
    async fn give_back(&self, id: &str) {
        // The message is not going out: a later claim of it may be sent.
        self.lock().handled.remove(id);
        match self.call("release", || self.store.release(id)).await {
            Ok(()) => self.lock().stats.released += 1,
            Err(e) => self.owe_or_log(Owed::Release(id.to_string()), "giving a claim back", &e),
        }
    }

    async fn settle_failed(&self, id: &str, why: FailReason, quick: bool) {
        if let Err(e) = self
            .attempt(quick, "failed", || self.store.failed(id, why))
            .await
        {
            self.owe_or_log(Owed::Failed(id.to_string(), why), "failing a message", &e);
        }
    }

    async fn reject_for_limit(&self, id: &str) {
        match self
            .call("reject", || {
                self.store.reject(id, RejectReason::ContinuationLimit)
            })
            .await
        {
            Ok(()) => self.lock().stats.rejected += 1,
            Err(e) => self.owe_or_log(
                Owed::Reject(id.to_string(), RejectReason::ContinuationLimit),
                "rejecting a message",
                &e,
            ),
        }
    }

    /// Tries every outcome still owed, in the order they arose. An outcome that fails again stays owed, and so does every later one
    /// about the same message (a rejection waits for the release before it). A refusal for good drops the outcome.
    async fn settle_owed(&self, quick: bool) {
        let owed: Vec<Owed> = std::mem::take(&mut self.lock().owed);
        if owed.is_empty() {
            return;
        }
        let mut keep: Vec<Owed> = Vec::new();
        let mut blocked: HashSet<String> = HashSet::new();
        for o in owed {
            if blocked.contains(o.id()) {
                keep.push(o);
                continue;
            }
            let r = match &o {
                Owed::Submitted(id, turn) => {
                    self.attempt(quick, "submitted", || self.store.submitted(id, *turn))
                        .await
                }
                Owed::Failed(id, why) => {
                    self.attempt(quick, "failed", || self.store.failed(id, *why))
                        .await
                }
                Owed::Release(id) => {
                    self.attempt(quick, "release", || self.store.release(id))
                        .await
                }
                Owed::Reject(id, why) => {
                    self.attempt(quick, "reject", || self.store.reject(id, *why))
                        .await
                }
            };
            match r {
                Ok(()) => {
                    let mut st = self.lock();
                    match &o {
                        Owed::Submitted(..) => st.stats.delivered += 1,
                        Owed::Release(_) => st.stats.released += 1,
                        Owed::Reject(..) => st.stats.rejected += 1,
                        Owed::Failed(..) => {}
                    }
                }
                Err(e) if e.retryable && !self.store.unavailable() => {
                    blocked.insert(o.id().to_string());
                    keep.push(o);
                }
                Err(e) => self.failed(&format!("settling message {}", o.id()), &e),
            }
        }
        if !keep.is_empty() {
            let mut st = self.lock();
            // what arose meanwhile follows what is still owed from before
            let later = std::mem::take(&mut st.owed);
            st.owed = keep;
            for o in later {
                if !st.owed.contains(&o) {
                    st.owed.push(o);
                }
            }
        }
    }

    // ---- claiming and voting ------------------------------------------------------------------------------------------------------

    /// Claims a message; `None` when another call has it, it was withdrawn, held or cancelled, or the session changed while the claim was
    /// out (the claim is then given back). An error means the store could not say (then the message stays eligible).
    async fn claim(&self, n: &Notice, epoch: &str) -> Result<Option<Message>, StoreError> {
        let outcome = match self.call("claim", || self.store.claim(&n.id, epoch)).await {
            Ok(o) => o,
            Err(e) => {
                // The answer may have been lost after the control plane applied the claim.
                self.lock().claim_unsure = true;
                return Err(e);
            }
        };
        match outcome {
            ClaimOutcome::Claimed(m) => {
                self.lock().stats.claims += 1;
                if self.lock().epoch.as_deref() != Some(epoch) {
                    // The text was written for a session that is over: it does not go into the new one.
                    self.log(&format!(
                        "message {} was claimed in session {epoch}, which ended meanwhile; the claim is given back",
                        m.id
                    ));
                    self.give_back(&m.id).await;
                    return Ok(None);
                }
                Ok(Some(m))
            }
            ClaimOutcome::Refused(why) => {
                self.log(&format!("claim of {} refused: {why:?}", n.id));
                Ok(None)
            }
        }
    }

    /// Turns a claimed message into the vote, or ends it: too long (failed), over the allowance or too late (given back, then
    /// rejected). The claim is never abandoned silently. The clock is read now, when the message is about to go out.
    async fn dispatch(&self, m: Message, until: Instant) -> Option<TurnVote> {
        if m.text.len() > MAX_TEXT_BYTES {
            self.log(&format!("message {} is too long to send", m.id));
            self.settle_failed(&m.id, FailReason::TooLarge, false).await;
            return None;
        }
        if self.lock().handled.contains(&m.id) {
            // A repeated claim of a message this module already voted or sent: it is the same delivery, never a second prompt.
            self.log(&format!(
                "message {} was claimed again; it is not sent twice",
                m.id
            ));
            return None;
        }
        let left = until.saturating_duration_since(Instant::now());
        let verdict = self.lock().budget.verdict(left);
        if verdict != Verdict::Allowed {
            // A claimed message cannot be rejected; the claim goes back first. What is too late is sealed (and rejected as sealed)
            // by the seal, which knows the reason.
            self.give_back(&m.id).await;
            if verdict == Verdict::ContinuationLimit {
                self.reject_for_limit(&m.id).await;
            }
            self.log(&format!("message {} not sent: {verdict:?}", m.id));
            return None;
        }
        let text = m.text.clone();
        self.lock().handled.insert(m.id.clone());
        // A vote the driver never sent is given back before this one replaces it.
        let older = self.lock().voted.replace(m);
        if let Some(old) = older {
            self.give_back(&old.id).await;
        }
        Some(TurnVote::Prompt(text))
    }

    /// The next message to send, claimed: the interrupt's message first, then the queue. `until` is the run's deadline.
    async fn next_message(&self, until: Instant) -> Option<TurnVote> {
        let cap = Instant::now() + self.settings.decision_cap;
        let mut claims_until: Option<Instant> = None;
        loop {
            if Instant::now() >= cap {
                self.log("the decision took too long; the rest waits for the next turn end");
                return None;
            }
            let ready = self.lock().ready.pop_front();
            if let Some(m) = ready {
                if let Some(v) = self.dispatch(m, until).await {
                    return Some(v);
                }
                continue;
            }
            let left = until.saturating_duration_since(Instant::now());
            // Out of time: what waits is dealt with by the seal, which says why.
            if self.lock().budget.verdict(left) == Verdict::TooLate {
                return None;
            }
            let Some(n) = self.lock().queue.take_next() else {
                // Nothing is queued. An interrupt that is claiming a message right now has already taken it out of the queue: wait for
                // that claim (bounded) so the message it is claiming is seen by this turn end and not lost to the seal.
                let bound = *claims_until
                    .get_or_insert_with(|| Instant::now() + self.settings.store_timeout * 2);
                if self.claiming.load(Ordering::SeqCst) > 0 && Instant::now() < bound {
                    let _ =
                        tokio::time::timeout(Duration::from_millis(50), self.claim_done.notified())
                            .await;
                    continue;
                }
                return None;
            };
            if self.cancelled() {
                return None;
            }
            if self.lock().budget.verdict(left) == Verdict::ContinuationLimit {
                // The allowance is spent: the message is refused for it without a claim.
                self.reject_for_limit(&n.id).await;
                self.log(&format!(
                    "message {} not sent: the continuation allowance is spent",
                    n.id
                ));
                continue;
            }
            let epoch = self.sync_epoch().await?;
            match self.claim(&n, &epoch).await {
                Ok(Some(m)) => {
                    if let Some(v) = self.dispatch(m, until).await {
                        return Some(v);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    // The store cannot answer: the message stays eligible there and here, and the run is not held for it.
                    self.failed(&format!("claim of {}", n.id), &e);
                    self.lock().queue.requeue(n);
                    return None;
                }
            }
        }
    }

    /// After a claim whose answer may have been lost: asks the control plane what this agentd holds in `dispatching` (a claim without an id)
    /// and takes over what it does not know. A message it never sent and that was claimed in the current session is queued to go out
    /// (`ready`); one claimed in an older session is given back, and the control plane holds it for the user to confirm. Nothing the module
    /// already handles is touched. A failed question leaves the doubt for the next try.
    async fn reconcile(&self, quick: bool) {
        let epoch = {
            let st = self.lock();
            if !st.claim_unsure {
                return;
            }
            let Some(epoch) = st.epoch.clone() else {
                return;
            };
            epoch
        };
        let held = match self
            .attempt(quick, "held", || self.store.held(&epoch))
            .await
        {
            Ok(h) => h,
            Err(e) => {
                self.failed("asking what the agent holds", &e);
                return;
            }
        };
        let known: HashSet<String> = {
            let mut st = self.lock();
            st.claim_unsure = false;
            let mut k: HashSet<String> = st.handled.clone();
            k.extend(st.ready.iter().map(|m| m.id.clone()));
            k.extend(st.voted.iter().map(|m| m.id.clone()));
            k.extend(st.sent.iter().map(|s| s.id.clone()));
            k.extend(st.owed.iter().map(|o| o.id().to_string()));
            k
        };
        for h in held {
            if known.contains(&h.message.id) {
                continue;
            }
            let id = h.message.id.clone();
            // The decision is taken under the lock; the call to the control plane is made after it.
            let adopt = {
                let mut st = self.lock();
                st.queue.remove(&id);
                if h.current_session && st.epoch.as_deref() == Some(epoch.as_str()) {
                    st.stats.claims += 1;
                    st.ready.push_back(h.message);
                    true
                } else {
                    false
                }
            };
            if adopt {
                self.log(&format!(
                    "message {id} was claimed and its answer lost; it is taken over"
                ));
            } else {
                self.log(&format!(
                    "message {id} was claimed in an earlier session; the claim is given back"
                ));
                self.give_back(&id).await;
            }
        }
    }

    /// Waits (bounded) for the claims an interrupt is making.
    async fn settle_claims(&self) {
        self.settle_claims_for(self.settings.store_timeout * 2)
            .await;
    }

    async fn settle_claims_for(&self, bound: Duration) {
        let end = Instant::now() + bound;
        while self.claiming.load(Ordering::SeqCst) > 0 && Instant::now() < end {
            let _ =
                tokio::time::timeout(Duration::from_millis(50), self.claim_done.notified()).await;
        }
    }

    // ---- /now --------------------------------------------------------------------------------------------------------------------

    /// One `now` message, asked while the prompt numbered `aimed` was open. The cancel it makes must hit that prompt and no other.
    async fn interrupt_flow(self: Arc<Self>, id: String, aimed: u32) {
        let _turn = self.gate.lock().await;
        let Some(w) = self.wiring.get() else {
            return;
        };
        // The allowance decides first. When it is spent the message is refused whatever the interrupt does, so cutting the turn that is
        // running would lose its work for nothing: the message is refused at once, without a claim, and the turn goes on. The user is not
        // told it will be delivered. (The time left is not known here; the turn end still refuses a message that comes too late.)
        let refused = {
            let mut st = self.lock();
            if st.sealed || st.terminal || st.user_cancel || st.closing || *w.cancel.borrow() {
                return;
            }
            if st.budget.verdict(Duration::MAX) == Verdict::ContinuationLimit {
                let n = st.queue.take(&id);
                if n.is_some() {
                    // Counted while the queue is locked, as a claim is: a turn end or a seal that comes now waits for the refusal to reach
                    // the store.
                    self.claiming.fetch_add(1, Ordering::SeqCst);
                }
                Some(n)
            } else {
                None
            }
        };
        if let Some(n) = refused {
            if let Some(n) = n {
                self.log(&format!(
                    "message {} cannot interrupt (the continuation allowance is spent); it is refused and the turn goes on",
                    n.id
                ));
                self.reject_for_limit(&n.id).await;
                self.claiming.fetch_sub(1, Ordering::SeqCst);
                self.claim_done.notify_waiters();
            }
            return;
        }
        // Preconditions, read once.
        let notice = {
            let mut st = self.lock();
            if st.sealed || st.terminal || st.user_cancel || st.closing || *w.cancel.borrow() {
                return;
            }
            let cap = self.interrupt_cap.load(Ordering::SeqCst);
            let why = if st.interrupts >= cap {
                Some(format!("the {cap} interrupts of the run are spent"))
            } else if st.quarantined {
                Some("the engine was replaced after a cancel it did not answer".to_string())
            } else if st.cancel_unsafe {
                Some("the engine's cancel is not safe in this session".to_string())
            } else if st.interrupting.is_some() {
                Some("another interrupt is under way".to_string())
            } else {
                None
            };
            if let Some(why) = why {
                // It cannot interrupt: it goes out at the next turn end, as a next message at the head of the queue, and the user is told
                // in the words of an interrupt the engine did not answer.
                st.queue.demote(&id);
                drop(st);
                self.log(&format!(
                    "message {id} cannot interrupt ({why}); it is delivered after this turn"
                ));
                self.notice("could not interrupt; delivered after this turn", &id);
                return;
            }
            let Some(n) = st.queue.take(&id) else {
                return;
            };
            // Counted while the queue is still locked: a turn end that finds the queue empty then sees this claim.
            self.claiming.fetch_add(1, Ordering::SeqCst);
            n
        };
        let claimed = match self.sync_epoch().await {
            Some(epoch) => self.claim(&notice, &epoch).await,
            None => Ok(None),
        };
        match claimed {
            Ok(Some(m)) => {
                let id = m.id.clone();
                let mut st = self.lock();
                if st.terminal || st.sealed || st.user_cancel || st.closing {
                    // The run ended while the claim was out: the close fails it as not sent.
                    st.ready.push_back(m);
                } else if st.prompt_no != aimed {
                    // The prompt the user saw has ended and another has started: a cancel now would hit the other one.
                    st.ready.push_front(m);
                    drop(st);
                    self.log(&format!(
                        "message {id} claimed; a later prompt is open than the one it was aimed at, so it goes out at the turn end"
                    ));
                } else {
                    st.ready.push_front(m);
                    // The check and the call are one step under the lock, so a prompt that opens in between is not the one cancelled
                    // and the message cannot be taken by a turn end that is running at the same moment.
                    let interrupted = st.interrupting.is_none() && w.ctl.interrupt();
                    if interrupted {
                        st.interrupting = Some(id.clone());
                    }
                    drop(st);
                    if interrupted {
                        self.log(&format!("interrupt for message {id}"));
                    } else {
                        self.log(&format!(
                            "message {id} claimed; no prompt was open, it goes out at the turn end"
                        ));
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                self.failed(&format!("claim of {}", notice.id), &e);
                self.lock().queue.requeue(notice);
            }
        }
        self.claiming.fetch_sub(1, Ordering::SeqCst);
        self.claim_done.notify_waiters();
    }

    /// The stage ended because of this module's interrupt.
    async fn after_interrupt(&self, end: &TurnEnd<'_>, until: Instant) -> TurnVote {
        let acknowledged = matches!(end.raw.watched, Watched::Done(Ok(_)));
        if acknowledged {
            // The engine answered the cancel. Its tools are not told: a cancel kills the shell, not what the shell started.
            let report = self.reap(true, self.settings.quiesce).await;
            if report.remaining.is_empty() && !report.unverified {
                let m = {
                    let mut st = self.lock();
                    st.interrupts += 1;
                    st.stats.interrupts += 1;
                    st.interrupting = None;
                    st.ready.pop_front()
                };
                if let Some(m) = m
                    && let Some(v) = self.dispatch(m, until).await
                {
                    return v;
                }
                return TurnVote::Prompt(RESUME_PROMPT_INTERRUPTED.to_string());
            }
            self.log(&format!(
                "processes of the engine survived the kill: {:?}{}",
                report.remaining,
                if report.unverified {
                    " (the process list could not be read)"
                } else {
                    ""
                }
            ));
            self.demote_ready();
        } else {
            self.log(
                "could not interrupt; the engine did not answer the cancel: delivered after this turn",
            );
            self.demote_ready();
        }
        {
            let mut st = self.lock();
            st.interrupting = None;
            st.quarantined = true;
        }
        self.hard_reset().await
    }

    /// An interrupt that could not cancel the turn: its message is demoted to `next`. The claim is kept (the message is not given back and
    /// so is not held by the restart that follows), the user is told, and it is sent first in the new session.
    fn demote_ready(&self) {
        let ids: Vec<String> = {
            let mut st = self.lock();
            let ids: Vec<String> = st.ready.iter().map(|m| m.id.clone()).collect();
            st.demoted.extend(ids.iter().cloned());
            ids
        };
        for id in ids {
            self.notice("could not interrupt; delivered after this turn", &id);
        }
    }

    /// One line the user sees (`turn.progress`, kind `notify`, which the terminal app shows as a notice of the run).
    fn notice(&self, detail: &str, message_id: &str) {
        self.log(&format!("message {message_id}: {detail}"));
        if let Some(rec) = self.wiring.get().and_then(|w| w.events.as_ref()) {
            rec.record(
                EventType::TurnProgress,
                serde_json::json!({
                    "turn": DIRECT_TURN,
                    "kind": "notify",
                    "reason": "message-not-interrupted",
                    "detail": detail,
                    "messageId": message_id,
                }),
            );
        }
    }

    /// The engine is in a state nothing can be asked of: its claims go back, it and everything it started are killed, and the driver's
    /// own recovery starts a fresh session (the prompt goes to a pipe that is closed). When the kill cannot vouch for what it did, the
    /// run stops: a prompt must not go to an engine whose tools may still be writing.
    async fn hard_reset(&self) -> TurnVote {
        self.release_ready().await;
        let report = self.reap(false, self.settings.quiesce).await;
        self.log(&format!(
            "engine and tools killed: {} processes, {} left{}",
            report.killed.len(),
            report.remaining.len(),
            if report.unverified {
                ", the process list could not be read"
            } else {
                ""
            }
        ));
        if !report.remaining.is_empty() || report.unverified {
            return TurnVote::Stop {
                detail: format!(
                    "the engine's processes could not be stopped ({}); the run stops instead of sending a prompt beside them",
                    if report.unverified {
                        "the process list could not be read".to_string()
                    } else {
                        format!("{} still running", report.remaining.len())
                    }
                ),
            };
        }
        TurnVote::Prompt(RESUME_PROMPT_INTERRUPTED.to_string())
    }

    /// Gives back every claim that is not on its way to the engine (the message goes on as a next-turn message; a new session holds it).
    async fn release_ready(&self) {
        let ready: Vec<Message> = {
            let mut st = self.lock();
            // a demoted interrupt message keeps its claim: it is sent first in the new session
            let demoted = st.demoted.clone();
            let (keep, give): (VecDeque<Message>, VecDeque<Message>) =
                st.ready.drain(..).partition(|m| demoted.contains(&m.id));
            st.ready = keep;
            let mut v: Vec<Message> = give.into_iter().collect();
            v.extend(st.voted.take());
            v
        };
        for m in ready {
            self.give_back(&m.id).await;
        }
    }

    async fn reap(&self, keep_engine: bool, bound: Duration) -> reap::Report {
        let Some(r) = self.wiring.get().and_then(|w| w.reaper.clone()) else {
            return reap::Report::default();
        };
        tokio::task::spawn_blocking(move || r.reap_blocking(keep_engine, bound))
            .await
            .unwrap_or_default()
    }

    // ---- the end ---------------------------------------------------------------------------------------------------------------

    /// The close, in this order: the engine's processes are stopped; a claim an interrupt is making is waited for (briefly); the
    /// acknowledgements in flight reach the store; what was claimed and never sent ends failed (not sent); every outcome still owed is
    /// tried again; and admission closes with the terminal seal. Each call has one bounded attempt: the close has a budget of 12 s.
    async fn close(&self) {
        self.lock().closing = true;
        self.note_unavailable();
        // Nothing the engine started may outlive it: the end commit and the bundle come next.
        let _ = self.reap(false, Duration::from_secs(2)).await;
        if !self.usable() {
            // Nothing to tell a store that cannot serve this run.
            return;
        }
        self.settle_claims_for(Duration::from_secs(3)).await;
        self.flush_for(Duration::from_secs(2)).await;
        // A claim the module never saw is found now, so that it ends failed (not sent), not delivery-unknown.
        self.reconcile(true).await;
        let unsent: Vec<Message> = {
            let mut st = self.lock();
            let mut v: Vec<Message> = st.ready.drain(..).collect();
            v.extend(st.voted.take());
            v
        };
        for m in unsent {
            self.settle_failed(&m.id, FailReason::NotSent, true).await;
        }
        self.settle_owed(true).await;
        let done = self.lock().terminal;
        if !done {
            let epoch = self
                .lock()
                .epoch
                .clone()
                .unwrap_or_else(|| "none".to_string());
            for _ in 0..2 {
                match self
                    .quick("seal", || self.store.seal(&epoch, SealMode::Terminal))
                    .await
                {
                    Ok(_) => {
                        self.lock().terminal = true;
                        break;
                    }
                    Err(e) if e.retryable => self.failed("the terminal seal", &e),
                    Err(e) => {
                        self.failed("the terminal seal", &e);
                        break;
                    }
                }
            }
        }
    }
}

/// The background calls to the store, in order.
async fn worker(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<Job>) {
    while let Some(job) = rx.recv().await {
        match job {
            Job::Epoch(epoch, n) => {
                match inner
                    .call("epoch", || inner.store.report_epoch(&epoch, n))
                    .await
                {
                    Ok(()) => {
                        let mut st = inner.lock();
                        // A later session may have been reported meanwhile; only the newest counts as told.
                        if st.epoch.as_deref() == Some(epoch.as_str()) {
                            st.reported = Some(epoch);
                        }
                    }
                    Err(e) => inner.failed("reporting the session", &e),
                }
            }
            Job::Submitted(id, turn) => match inner
                .call("submitted", || inner.store.submitted(&id, turn))
                .await
            {
                Ok(()) => inner.lock().stats.delivered += 1,
                Err(e) => inner.owe_or_log(
                    Owed::Submitted(id.clone(), turn),
                    &format!("acknowledging message {id}"),
                    &e,
                ),
            },
            Job::Release(id) => match inner.call("release", || inner.store.release(&id)).await {
                Ok(()) => inner.lock().stats.released += 1,
                Err(e) => inner.owe_or_log(Owed::Release(id.clone()), "giving a claim back", &e),
            },
            Job::Barrier(tx) => {
                let _ = tx.send(());
            }
        }
    }
}

// ---- the hooks --------------------------------------------------------------------------------------------------------------------

/// Whether the stop looks like done: the engine ended its turn.
fn done_looking(end: &TurnEnd<'_>) -> bool {
    matches!(&end.raw.watched, Watched::Done(Ok(d)) if d.stop_reason == "end_turn")
}

impl DirectHook for MessagesHook {
    fn name(&self) -> &'static str {
        "messages"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // Every process the engine starts carries the tag, whatever session or parent it ends up with.
            ctx.engine_env
                .insert(TAG_ENV.to_string(), self.inner.tag.clone());
            let uid = ctx.engine_uid.map(|(u, _)| u);
            // The `landlock` mode has no engine user to sweep, so the launcher's filter and the parent chain find what scrubbed its environment.
            let reaper = Reaper::for_run(&self.inner.tag, uid, ctx.sandbox.is_some());
            // The container runtime runs as the engine's user, so the sweep by user would stop it with the engine and the test report that follows
            // would find a socket nobody listens on. Its processes were named before the engine existed and are out of every reap.
            if !ctx.runtime_pids.is_empty() {
                let kept = ctx
                    .runtime_pids
                    .iter()
                    .filter(|pid| reaper.protect_pid(**pid))
                    .count();
                ctx.env.log.line(&format!(
                    "direct turn messages: kept {kept} of {} runtime process(es) out of the reap",
                    ctx.runtime_pids.len()
                ));
            }
            if let Some(l) = &ctx.request.limits {
                self.apply_limits(l);
            }
            let wiring = Wiring::new(
                ctx.env.ctl.clone(),
                ctx.env.controls.subscribe(),
                ctx.env.log.clone(),
            )
            .with_reaper(reaper)
            .with_events(ctx.env.rec.clone());
            self.bind(wiring);
            Ok(())
        })
    }

    fn on_wire(&self, wire: &Wire<'_>) {
        if wire.dir == Dir::FromEngine
            && !wire.stale
            && wire.msg.get("method").and_then(|m| m.as_str()) == Some("session/update")
        {
            self.inner.updates.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn on_event(&self, event: &Event<'_>) {
        let i = &self.inner;
        match event {
            Event::SessionStarted(s) => {
                // The engine that is running now is the one a cancel must not kill.
                if let Some(r) = i.wiring.get().and_then(|w| w.reaper.as_ref()) {
                    r.mark_engine();
                }
                let now = Instant::now();
                let (dropped, stale) = {
                    let mut st = i.lock();
                    if st.epoch.as_deref() == Some(s.session_id) {
                        return;
                    }
                    st.epoch = Some(s.session_id.to_string());
                    st.epoch_no = s.number;
                    st.quarantined = false;
                    st.cancel_unsafe = false;
                    st.tripped = false;
                    st.interrupting = None;
                    // A message turn that never got its reply ends with its session; its time is charged.
                    if let Some(sent) = st.sent.take() {
                        st.budget.turn_ended(now.duration_since(sent.at));
                    }
                    // What was written against another session is held by the control plane; it leaves the queue here.
                    let dropped = st.queue.retain_epoch(s.session_id);
                    // A demoted interrupt message keeps its claim into the new session and goes out first there.
                    let demoted = st.demoted.clone();
                    for m in st.ready.iter_mut().filter(|m| demoted.contains(&m.id)) {
                        m.epoch = s.session_id.to_string();
                    }
                    // A claim made in the old session is given back, whether it waits for a vote or was voted and never sent; the
                    // message waits to be confirmed.
                    let (keep, mut stale): (VecDeque<Message>, VecDeque<Message>) =
                        st.ready.drain(..).partition(|m| m.epoch == s.session_id);
                    st.ready = keep;
                    stale.extend(st.voted.take());
                    (dropped, stale)
                };
                if !dropped.is_empty() {
                    i.log(&format!(
                        "{} message(s) written for the earlier session are held",
                        dropped.len()
                    ));
                }
                for m in stale {
                    i.job(Job::Release(m.id));
                }
                i.job(Job::Epoch(s.session_id.to_string(), s.number));
            }
            Event::PromptSent(p) => {
                let now = Instant::now();
                i.updates.store(0, Ordering::SeqCst);
                let mut st = i.lock();
                st.prompt_at = Some(now);
                st.prompt_no = p.no;
                // The claimed message is delivered by a prompt with its text in the session it was claimed in.
                if st
                    .voted
                    .as_ref()
                    .is_some_and(|m| m.text == p.text && m.epoch == p.session_id)
                    && let Some(m) = st.voted.take()
                {
                    // The prompt carrying a claimed message went out: that is the delivery, in this turn.
                    st.sent = Some(Sent {
                        id: m.id.clone(),
                        text: m.text.clone(),
                        session: p.session_id.to_string(),
                        at: now,
                    });
                    st.budget.turn_started();
                    drop(st);
                    i.job(Job::Submitted(m.id, p.no));
                } else if st
                    .sent
                    .as_ref()
                    .is_some_and(|s| s.text == p.text && s.session == p.session_id)
                {
                    // The ladder sent the same text again after a provider error: the same delivery, in the turn of the first send.
                } else if let Some(s) = st.sent.take() {
                    // Another prompt replaced a message turn that was never answered: the time it ran is charged.
                    st.budget.turn_ended(now.duration_since(s.at));
                }
            }
            Event::PromptReplied(p) => {
                if p.error.is_some() {
                    return;
                }
                let now = Instant::now();
                let zero = p.usage.is_none_or(|u| {
                    u.input_tokens == 0
                        && u.output_tokens == 0
                        && u.cache_read_input_tokens == 0
                        && u.cache_write_input_tokens == 0
                });
                let tripped = {
                    let mut st = i.lock();
                    let quick = st
                        .prompt_at
                        .is_some_and(|t| now.duration_since(t) <= i.settings.trip_window);
                    let tripped = p.stop_reason == Some("end_turn")
                        && zero
                        && quick
                        && i.updates.load(Ordering::SeqCst) == 0;
                    if tripped {
                        st.tripped = true;
                        st.cancel_unsafe = true;
                        st.stats.tripwires += 1;
                    }
                    if let Some(s) = st.sent.take() {
                        st.budget.turn_ended(now.duration_since(s.at));
                    }
                    tripped
                };
                if tripped {
                    i.log("tripwire: end_turn with no usage and no update since the prompt; the engine's own queue took it");
                }
            }
            Event::EngineStopped(EngineStop::Reaped | EngineStop::Restarted) => {
                // The old engine is gone; what its tools left is stopped before anything else runs.
                if let Some(r) = i.wiring.get().and_then(|w| w.reaper.as_ref())
                    && !r.scan().is_empty()
                {
                    let report = r.reap_blocking(false, Duration::from_secs(2));
                    i.log(&reap::reaped_line(&report));
                }
            }
            _ => {}
        }
    }

    fn on_controls(&self, controls: &[PendingControl]) {
        // A Cancel wins over everything in its batch and after it. The messages arrive as `PendingControl::Message` notices (protocol
        // 1.38.0): they name a message and never carry its text.
        let cancel = controls.iter().any(|c| matches!(c, PendingControl::Cancel));
        let notices: Vec<Notice> = controls.iter().filter_map(http::notice_of).collect();
        if cancel || !notices.is_empty() {
            self.inner.offer(cancel, notices);
        }
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            let i = &self.inner;
            if !i.usable() || i.cancelled() {
                return TurnVote::Done;
            }
            // The run's deadline, fixed now: the clock is read again when a message is about to go out.
            let until = Instant::now() + end.time_left;
            i.lock().budget.note_stop(done_looking(end));
            if end.interrupted {
                if i.lock().interrupting.is_none() {
                    // another module's interrupt: not ours to answer
                    return TurnVote::Done;
                }
                return i.after_interrupt(end, until).await;
            }
            {
                // A stage that ended on its own settles any interrupt that was asked for it: the cancel (if it was written) is over.
                let mut st = i.lock();
                if st.interrupting.take().is_some() {
                    drop(st);
                    i.log("an interrupt was asked but the stage ended on its own; the message goes out now");
                }
            }
            if i.prompt_open_now() {
                // The driver sent a prompt of its own (a nudge) before it asked: nothing is claimed or sent beside it.
                i.log("a prompt is open; nothing is claimed or sent now");
                return TurnVote::Done;
            }
            let tripped = std::mem::take(&mut i.lock().tripped);
            if tripped {
                return i.hard_reset().await;
            }
            i.next_message(until).await.unwrap_or(TurnVote::Done)
        })
    }

    fn on_seal<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            let i = &self.inner;
            if !i.usable() || i.cancelled() {
                return TurnVote::Done;
            }
            let until = Instant::now() + end.time_left;
            if i.prompt_open_now() {
                i.log("a prompt is open; the seal waits");
                return TurnVote::Done;
            }
            i.settle_claims().await;
            // The wire refuses a normal seal while a message is dispatching: the acknowledgements in flight reach the store first, and
            // so does every outcome still owed.
            i.flush().await;
            i.settle_owed(false).await;
            // A claim whose answer was lost: the control plane would refuse a normal seal for it. Learn what is held first.
            i.reconcile(false).await;
            // A message claimed after the last turn end (an interrupt that lost the race) is sent before anything is sealed.
            if !i.lock().ready.is_empty()
                && let Some(v) = i.next_message(until).await
            {
                return v;
            }
            let mut unsendable = 0;
            for _ in 0..8 {
                let Some(epoch) = i.sync_epoch().await else {
                    return TurnVote::Done;
                };
                let left = until.saturating_duration_since(Instant::now());
                let mut mode = match i.lock().budget.verdict(left) {
                    Verdict::Allowed => SealMode::Normal,
                    Verdict::ContinuationLimit => SealMode::LimitReached,
                    Verdict::TooLate => SealMode::Terminal,
                };
                if unsendable >= 2 {
                    // Messages come back that the module cannot send (its claims keep failing): the run ends, and they end rejected as
                    // sealed, instead of the seal being asked until it gives up.
                    mode = SealMode::Terminal;
                }
                match i.call("seal", || i.store.seal(&epoch, mode)).await {
                    Ok(SealOutcome::Sealed { .. }) => {
                        // Admission is closed and what was held or pending has been rejected: the terminal seal at the shutdown has
                        // nothing left to do.
                        let mut st = i.lock();
                        st.sealed = true;
                        st.terminal = true;
                        return TurnVote::Done;
                    }
                    Ok(SealOutcome::Pending(notices)) => {
                        i.lock().queue.offer(notices);
                        if let Some(v) = i.next_message(until).await {
                            return v;
                        }
                        // None of them could be sent (withdrawn meanwhile, or the claims failed): ask again, a limited number of times.
                        unsendable += 1;
                    }
                    Err(e) => {
                        // A refusal (a message is dispatching that this module does not know) or an unreachable store: the vote is
                        // `Done` and the close settles what is left.
                        i.failed("the seal", &e);
                        return TurnVote::Done;
                    }
                }
            }
            TurnVote::Done
        })
    }

    fn on_shutdown<'a>(&'a self, _end: &'a Shutdown<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.close().await;
        })
    }

    fn on_finalize<'a>(&'a self, _fin: Finalize<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if !self.inner.lock().terminal {
                self.close().await;
            }
        })
    }
}
