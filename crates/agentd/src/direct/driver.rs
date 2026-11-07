//! The ACP driver of a direct run: `initialize`, `session/new` at the repository root,
//! the model pin check, ONE `session/prompt` carrying the user's text byte for byte, and the watchdogs around it. It reports
//! how the prompt ended ([`RawEnd`]) and decides nothing about the work.
//!
//! The optional nudge sends the nudge text
//! as a follow-up prompt after an `end_turn` that did not say DONE, while the tree keeps changing; it never runs by default.

use crate::BoxFuture;
use crate::acp::{AcpClient, AcpError, Activity, WatchLimits, Watched, read_usage, watch};
use crate::direct::classify::{
    Classified, PromptDone, RawEnd, TreeFacts as MeasuredTree, classify, refine,
    refine_engine_stop, scrub_detail, with_disk,
};
use crate::direct::collector::DirectCollector;
use crate::direct::context::ContextDir;
use crate::direct::disk::DiskFull;
use crate::direct::engine_stop::StopReport;
use crate::direct::failover::{FailoverPlan, FailoverRecord, Route};
use crate::direct::fresh::{self, FreshSpec};
use crate::direct::hooks::{
    Event, Hooks, LadderEvent, PromptReplied, PromptSent, SessionEnd, SessionEnded, SessionStarted,
    TurnEnd, TurnVote,
};
use crate::direct::input::{
    Ask, AssessFacts, Assessment, DigestSource, DigestSources, InputPolicy, RunInputState,
    ShapeCounters, StopWhy, TreeSource, Window, assess, digest, needs_input_payload_for,
    new_session_prompt, reason_word,
};
use crate::direct::recorder::StopContext;
use crate::direct::retry::{
    RESUME_PROMPT_INTERRUPTED, RetryPolicy, disk_boundary, prompt_with_retry_from, set_model,
};
use crate::direct::review::{
    FACTS_PATHS_ASKED, FactsInput, Gate, ReviewPlan, facts_text, render, skip_reason,
};
use crate::direct::snapshot::ScratchRepo;
use crate::direct::summary::InputNote;
use crate::direct::waiting::{
    LateAnswers, WaitCtx, WaitEnd, closed_event, drain_stale, wait_for_answer,
};
use crate::events::{EventRecorder, Instruct};
use crate::logging::LogSink;
use crate::time::{format_rfc3339, now_unix};
use crate::turn::prompt_usage;
use harness_protocol::common::Usage;
use harness_protocol::direct::{
    DIGEST_BUDGET_BYTES, DIRECT_TURN, DirectAttempt, DirectAttemptKind, DirectAttemptResult,
    DirectFailoverNote, DirectFreshSession, DirectIntervention, DirectOutcome, DirectReview,
    DirectSwitch, DirectSwitchKind, InstructAction, InstructOrigin, MAX_NEEDS_INPUT_FILES,
    NeedsInputEvidence, NeedsInputModel, NeedsInputReason, REVIEW_CANCELLED, REVIEW_COMPLETED,
    REVIEW_FAILED, REVIEW_RAN, REVIEW_SKIP_NOT_DONE,
};
use harness_protocol::envelope::EventType;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch as watch_chan};
use tokio::time::Instant;

/// `initialize` and `session/new` get this long each; an engine that hangs there has not started.
pub const SETUP_STEP_TIMEOUT: Duration = Duration::from_secs(120);
/// The cancel written after a deadline or a stall gets this long to be written.
pub const CANCEL_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// The nudge stops once this many prompts in a row changed nothing in the tree.
pub const NUDGE_UNCHANGED_LIMIT: u32 = 2;
/// Resumes a silence gets before the person is asked (the recovery ladder's class 7: the cancel the driver writes at the stall, then one
/// resume of the same session). A run-wide count: a second silence anywhere asks.
pub const SILENCE_RESUMES: u32 = 1;
/// After the cancel of a silence the engine's pipe is read until it has been quiet this long, so the answer to the cancelled prompt is
/// consumed before the resume is written.
pub const SETTLE_QUIET: Duration = Duration::from_secs(3);
/// The most the settling after a cancel may take, whatever the engine keeps writing.
pub const SETTLE_MAX: Duration = Duration::from_secs(15);

/// The follow-up the request may ask for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nudge {
    pub text: String,
    /// At least 1 (0 means no nudge and the caller passes `None`).
    pub max: u32,
}

/// What to send and how long to wait.
#[derive(Clone, Debug)]
pub struct PromptPlan {
    /// `git rev-parse --show-toplevel` of the worktree.
    pub cwd: String,
    /// The model id the engine must report at `session/new`.
    pub model: String,
    /// The user's prompt, sent as typed.
    pub prompt: String,
    pub nudge: Option<Nudge>,
    /// The user's cancel, as the events response delivered it: once true, no follow-up prompt is sent.
    pub cancel: Option<tokio::sync::watch::Receiver<bool>>,
    /// From the moment the prompt is sent: the deadline less the time setup took.
    pub timeout: Duration,
    pub silence: Duration,
    /// What to do when the prompt ends in a provider failure ([`RetryPolicy::off`] sends the one prompt and nothing more).
    pub retry: RetryPolicy,
    /// The routes to switch to when the resumes on the model are spent (`None` or one route: no failover).
    pub failover: Option<FailoverPlan>,
    /// How many times the prompt is resumed on the same route after the engine ended it `cancelled` itself (`AGENTD_DIRECT_ENGINE_STOP_RESUMES`,
    /// default 1; 0 ends the run at the first such stop). Such a stop is the engine's own brake, never a provider failure: it switches
    /// nothing.
    pub engine_stop_resumes: u32,
}

/// Tells the nudge whether the tree changed: a fingerprint of HEAD and the working tree.
pub trait TreeProbe: Send + Sync {
    fn fingerprint(&self) -> Pin<Box<dyn Future<Output = String> + Send + '_>>;
}

/// The probe of a real run: HEAD and the hash of the work tree's content, made in a private repository of agentd's
/// ([`ScratchRepo`]) so the engine's repository is never written. A probe that cannot read the tree answers with a string no
/// other call returns, so an error is never read as "nothing changed".
#[derive(Debug)]
pub struct GitProbe {
    repo: std::path::PathBuf,
    scratch: Option<ScratchRepo>,
    errors: std::sync::atomic::AtomicU64,
}

impl GitProbe {
    /// Makes the private repository under `agent_data`; when that fails every fingerprint is an error string.
    pub async fn open(agent_data: &std::path::Path, repo: &std::path::Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            scratch: ScratchRepo::new(agent_data, repo).await.ok(),
            errors: std::sync::atomic::AtomicU64::new(0),
        }
    }

    async fn tree(&self) -> Option<String> {
        let head = crate::workspace::git_with_env(&self.repo, &["rev-parse", "HEAD"], &[])
            .await
            .ok()?;
        let tree = self.scratch.as_ref()?.tree().await.ok()?;
        Some(format!("{head}\n{tree}"))
    }
}

impl TreeProbe for GitProbe {
    fn fingerprint(&self) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        Box::pin(async move {
            match self.tree().await {
                Some(f) => f,
                None => format!(
                    "error-{}",
                    self.errors
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                ),
            }
        })
    }
}

/// A probe for a run with no nudge: the tree always looks changed.
#[derive(Debug)]
pub struct NoProbe;

impl TreeProbe for NoProbe {
    fn fingerprint(&self) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        Box::pin(async { String::new() })
    }
}

/// How the driver ended, with what it learned on the way.
#[derive(Debug)]
pub struct DriverEnd {
    pub raw: RawEnd,
    pub session_id: Option<String>,
    /// The model id the engine reported back at `session/new`.
    pub pinned: Option<String>,
    pub usage: Option<Usage>,
    pub nudges_sent: u32,
    pub max_silent_gap_s: Option<u32>,
    /// When the first prompt was sent.
    pub prompt_started: Option<Instant>,
    /// Every try of the prompt when it was retried after a provider failure; empty for a run with one try.
    pub attempts: Vec<DirectAttempt>,
    /// Every confirmed switch of route or model; empty without failover or without a switch.
    pub switches: Vec<DirectSwitch>,
    /// The model the engine ended on, once it switched.
    pub final_model: Option<String>,
    /// What failover used and did; `Some` for a run that had failover routes.
    pub failover: Option<DirectFailoverNote>,
    /// Seconds every route had been down when the run ended, if it ended in such a stretch.
    pub outage_s: Option<u64>,
    /// Every question the run asked its user and how it ended; empty for a run that never asked.
    pub interventions: Vec<DirectIntervention>,
    /// Every fresh session the run started.
    pub fresh_sessions: Vec<DirectFreshSession>,
    /// How the last stop followed a wait for the user: the sentence that leads the detail (`You chose to finish.`).
    pub input_note: Option<InputNote>,
    /// The facts of the last attempt when the run ended on the engine's own stop: the end sequence words that stop with them.
    pub engine_stop: Option<StopReport>,
    /// The channel of the user's answers, handed back: answers that arrive during the end sequence are acknowledged as ignored.
    pub late_answers: Option<LateAnswers>,
    /// A module of the seam ended the run (`TurnVote::Stop`): its words. The end sequence reads the run as "stopped" with this detail.
    pub hook_stop: Option<String>,
    /// The last stage ended on error -32603 while the session disk was short: the cause with its numbers. The end sequence reads
    /// the run as the harness's own stop that names the disk ([`crate::direct::classify::with_disk`]).
    pub disk_full: Option<crate::direct::disk::DiskFull>,
    /// What the review round did, or why it did not run; `None` for a run without the stage loop.
    pub review: Option<DirectReview>,
}

/// A failure in the inner future: before the prompt (a setup failure) or during it.
pub(crate) struct Fail {
    pub(crate) setup: bool,
    pub(crate) stage: &'static str,
    pub(crate) err: AcpError,
    /// The failure was the start of a fresh session (not a setup failure of the run, and not the new session's first prompt).
    pub(crate) fresh_start: bool,
}

/// What the stages of one run learn and hand to the next.
pub(crate) struct Inner {
    pub(crate) session_id: Option<String>,
    pub(crate) pinned: Option<String>,
    pub(crate) usage: UsageTally,
    pub(crate) nudges: u32,
    pub(crate) prompt_started: Option<Instant>,
    pub(crate) attempts: Vec<DirectAttempt>,
    pub(crate) failover: FailoverRecord,
    /// Sessions of the engine so far (1 for the first).
    pub(crate) sessions: u32,
    /// Every fresh session the run started.
    pub(crate) fresh: Vec<DirectFreshSession>,
    /// The kind of the first send of the stage in progress: what a stalled try is recorded as.
    pub(crate) stage_kind: DirectAttemptKind,
}

impl Inner {
    fn new() -> Self {
        Self {
            session_id: None,
            pinned: None,
            usage: UsageTally::default(),
            nudges: 0,
            prompt_started: None,
            attempts: Vec::new(),
            failover: FailoverRecord::default(),
            sessions: 1,
            fresh: Vec::new(),
            stage_kind: DirectAttemptKind::Prompt,
        }
    }
}

pub(crate) fn model_option(result: &Value) -> Option<Option<String>> {
    result
        .get("configOptions")
        .and_then(Value::as_array)
        .and_then(|opts| {
            opts.iter()
                .find(|o| o.get("id").and_then(Value::as_str) == Some("model"))
        })
        .map(|o| {
            o.get("currentValue")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

/// True when `message` says DONE as a word.
pub fn says_done(message: &str) -> bool {
    message
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == "DONE")
}

/// Why a fresh session was started: agentd's own remedy for a saved conversation the provider rejects, a full context window or a dead
/// engine (`harness`), or the answer of the person (`user`), or of the later analyzer (`analyzer`). The word the `new-session` event carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTrigger {
    Harness,
    User,
    Analyzer,
}

impl SessionTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::User => "user",
            Self::Analyzer => "analyzer",
        }
    }
}

/// One send of the run and what it needs around it. The first stage of every run is `Prompt(plan.prompt)`: setup (`initialize`,
/// `session/new`, the model pin) and the user's text byte for byte. Later stages exist only for a run that may ask its user what to
/// do: they are started by an answer, or by agentd's one automatic fresh session.
#[derive(Clone, Debug)]
pub enum Stage {
    /// A prompt in the live session: the user's first text, or what the person typed to continue.
    Prompt(String),
    /// The one resume of a silence: the fixed sentence in the same session after the cancel (a try of kind `resume`).
    Resume(String),
    /// The person chose another model: the engine is switched to `route` (`session/set_config_option`) and then gets `text`, or the
    /// fixed resume sentence when there is none.
    TryModel { route: Route, text: Option<String> },
    /// A new session in the same work tree. `restart` stops the engine process and starts it again first (a dead or hung engine, or a
    /// process that keeps its counters); `prompt` is the harness-written text (task, why it stopped, the user's words, the notes).
    FreshSession(FreshSpec),
}

/// Starts the engine process again with the launch it first had (same environment, rules, settings and credential socket). Used by a
/// fresh session when the engine is dead, hung, or keeps its counters between sessions; otherwise a fresh session is a `session/new`
/// in the live process.
pub trait Respawn: Send {
    type R: AsyncRead + Unpin + Send;
    type W: AsyncWrite + Unpin + Send;
    /// Stops the engine process (the group is killed after the grace), starts the same launch again and returns its pipes.
    fn respawn(&mut self) -> RespawnFuture<'_, Self::R, Self::W>;

    /// Where the digest of every stop is uploaded for the people who read the run later (`stop-digest-<n>.json`); `None` writes none.
    fn audit(&self) -> Option<Arc<dyn AuditSink>> {
        None
    }
}

/// What a restart answers: the output and the input of the new engine process, or why it did not start.
pub type RespawnFuture<'a, R, W> = BoxFuture<'a, Result<(R, W), String>>;

/// The host of a run that cannot restart its engine (the tests of the old line, and every run without an answer window).
#[derive(Debug)]
pub struct NoRespawn<R, W>(std::marker::PhantomData<fn() -> (R, W)>);

impl<R, W> NoRespawn<R, W> {
    pub fn new() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<R, W> Default for NoRespawn<R, W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: AsyncRead + Unpin + Send, W: AsyncWrite + Unpin + Send> Respawn for NoRespawn<R, W> {
    type R = R;
    type W = W;
    fn respawn(&mut self) -> RespawnFuture<'_, R, W> {
        Box::pin(async { Err("this run cannot restart its engine".to_string()) })
    }
}

/// The usage of a whole run: the sessions it ended plus the live one. A new session reports its counters from zero, so what the
/// earlier sessions used is kept apart and added.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageTally {
    /// What the sessions that ended used.
    finished: Option<Usage>,
    /// The live session so far: the sum of what its prompts reported, else the latest streamed figure.
    session: Option<Usage>,
    /// The streamed figure that was already there when the live session began: it belongs to the session before.
    stale_streamed: Option<Usage>,
}

fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens + b.input_tokens,
        output_tokens: a.output_tokens + b.output_tokens,
        cache_read_input_tokens: a.cache_read_input_tokens + b.cache_read_input_tokens,
        cache_write_input_tokens: a.cache_write_input_tokens + b.cache_write_input_tokens,
    }
}

impl UsageTally {
    /// A prompt of the live session was answered: `reported` is the usage its response carried, `streamed` the collector's latest figure.
    pub fn after_prompt(&mut self, reported: Option<Usage>, streamed: Option<Usage>) {
        let streamed = streamed.filter(|u| Some(*u) != self.stale_streamed);
        self.session = prompt_usage(self.session, reported, streamed);
    }

    /// A fresh session begins: what the live one used is final. `streamed_now` is the collector's figure at this moment, which the new
    /// session must not be credited with.
    pub fn new_session(&mut self, streamed_now: Option<Usage>) {
        self.finished = self.total();
        self.session = None;
        self.stale_streamed = streamed_now;
    }

    /// The whole run so far; `None` when nothing was reported.
    pub fn total(&self) -> Option<Usage> {
        match (self.finished, self.session) {
            (None, s) => s,
            (f, None) => f,
            (Some(f), Some(s)) => Some(add_usage(f, s)),
        }
    }
}

/// Runs the whole ACP exchange of a run with no answer window: [`run_direct_stages`] with one stage and no way to restart the engine.
pub async fn run_direct_prompt<R, W>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    activity: &Arc<Activity>,
    plan: &PromptPlan,
    tree: &dyn TreeProbe,
) -> DriverEnd
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    run_direct_stages(
        client,
        collector,
        activity,
        plan,
        tree,
        None,
        &mut NoRespawn::new(),
    )
    .await
}

/// Runs the whole ACP exchange: the stage loop. The first stage is setup and the user's prompt. With `input` the run may go on: when a
/// stage ends in a way agentd cannot mend, the run's user is asked what to do, and the
/// answer starts the next stage. Without it the run ends after the first stage, as it always did. On a deadline or a stall it writes
/// `session/cancel` before it returns, so the engine can stop its tools; the caller closes the engine's stdin and reaps it.
pub async fn run_direct_stages<R, W, S>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    activity: &Arc<Activity>,
    plan: &PromptPlan,
    tree: &dyn TreeProbe,
    input: Option<InputWiring>,
    host: &mut S,
) -> DriverEnd
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
    S: Respawn<R = R, W = W>,
{
    // A run that can ask its user what to do asks at the ladder's cap of fruitless prompts instead of holding until the deadline.
    let capped;
    let plan = if input.as_ref().is_some_and(|w| !w.policy.wait.is_zero())
        && plan.failover.as_ref().is_some_and(|f| !f.stop_at_cap)
    {
        let mut p = plan.clone();
        if let Some(f) = p.failover.as_mut() {
            f.stop_at_cap = true;
        }
        capped = p;
        &capped
    } else {
        plan
    };
    let mut inner = Inner::new();
    let started = Instant::now();
    // One absolute deadline for the whole run: every stage gets what is left of it, and a person's wait is inside it.
    let deadline = started + plan.timeout;
    let mut machine = input.map(|w| Machine::new(w, started));
    let mut stage = Stage::Prompt(plan.prompt.clone());
    let mut first = true;
    let mut max_silent_gap_s: Option<u32> = None;
    let mut stops = 0u32;
    // The modules of the seam; empty for a run with none, and then nothing below is called.
    let hooks = collector.hooks().clone();
    let mut hook_stop: Option<String> = None;
    // The review round: one extra prompt after a delivery the engine called done. `round` is the round whose turn is in flight.
    let review = machine.as_ref().and_then(|m| m.w.review.clone());
    let mut round: Option<Round> = None;
    let mut round_ran = false;
    let mut review_note: Option<DirectReview> = None;
    // Why the round did not run, as the last end of the run found it (read when the run was about to end, not recomputed after the loop).
    let mut review_skipped: Option<&'static str> = None;
    // The files the harness counted at the last stage end: every way out of the loop passes the assignment below.
    let mut last_files: Option<u32>;
    let raw = loop {
        let limits = WatchLimits::new(
            deadline.saturating_duration_since(Instant::now()),
            plan.silence,
        );
        let rec = machine.as_ref().map(|m| m.w.rec.clone());
        let watched = if first {
            watch(
                exchange(client, collector, activity, plan, tree, &mut inner),
                limits,
                activity,
            )
            .await
        } else {
            watch(
                run_stage(
                    client,
                    collector,
                    activity,
                    plan,
                    tree,
                    &mut inner,
                    host,
                    rec.as_deref(),
                    &stage,
                ),
                limits,
                activity,
            )
            .await
        };
        first = false;
        // Read before anything is written to the engine: a write restarts the silence clock. The longest silence of any stage is kept;
        // a person's wait is never part of it.
        max_silent_gap_s = max_silent_gap_s.max(activity.max_silent_gap_s());
        let cancel_sent = activity.cancel_sent();
        let message = collector.final_message();
        let (watched, setup_error, fresh_failed) = match watched {
            Watched::Done(Ok(done)) => (Watched::Done(Ok(done)), None, false),
            Watched::Done(Err(f)) => {
                let setup_error = f.setup.then(|| format!("{}: {}", f.stage, f.err));
                (Watched::Done(Err(f.err)), setup_error, f.fresh_start)
            }
            Watched::TimedOut => (Watched::TimedOut, None, false),
            Watched::Stalled { silent } => (Watched::Stalled { silent }, None, false),
        };
        if matches!(watched, Watched::TimedOut | Watched::Stalled { .. })
            && let Some(sid) = &inner.session_id
        {
            // Bounded: an engine that reads nothing must not hold the end of the run.
            let _ = tokio::time::timeout(
                CANCEL_WRITE_TIMEOUT,
                client.notify("session/cancel", json!({ "sessionId": sid })),
            )
            .await;
        }
        let mut raw = RawEnd {
            watched,
            cancel_sent,
            message,
            setup_error,
        };
        // The work tree as it is now, measured once through agentd's private repository: the digest of this stop and the assessment read it.
        let mut files = match &machine {
            Some(m) => {
                m.w.tree
                    .changed_files(MAX_NEEDS_INPUT_FILES)
                    .await
                    .map(|c| c.total)
            }
            None => None,
        };
        stops += 1;
        write_stop_digest(
            host.audit(),
            collector,
            &raw,
            files,
            stops,
            &inner,
            plan,
            started,
        )
        .await;
        last_files = files;
        // The stage that just ended was the review round's: it is judged here, once. The answer window is not asked about it (a failed round
        // must not turn into a question for the user), the modules are asked about a round that ended with a message as about any turn, and about
        // a failed one not at all: its session may be dead or still working, and the turn before the round stands. The round never repeats.
        let (round_ended, round_failed) = match (round.take(), review.as_ref()) {
            (Some(r), Some(rp)) => {
                let rec = machine.as_ref().map(|m| m.w.rec.clone());
                let note = end_round(r, rp, &mut raw, plan, &inner, tree, rec.as_deref()).await;
                let failed = note.result.as_deref() == Some(REVIEW_FAILED);
                review_note = Some(note);
                (true, failed)
            }
            _ => (false, false),
        };
        // The modules vote first: a prompt they propose or a stop they ask for comes before anything the driver would do next. A module
        // that votes `Wait` keeps the run open while the engine works by itself (the goal loop): the pipe is read, the deadline, the
        // silence limit and the user's cancel apply, and the modules are asked again.
        if !hooks.is_empty() && !round_failed {
            let mut held = false;
            let vote = loop {
                let classified = classify_stage(
                    &raw,
                    files,
                    collector,
                    inner.failover.stage.disk_full.as_ref(),
                );
                let voted = consult(
                    &hooks,
                    false,
                    &raw,
                    &classified,
                    files,
                    stops,
                    &inner,
                    collector,
                    activity,
                    plan,
                    deadline,
                )
                .await;
                let Some(TurnVote::Wait { until }) = voted else {
                    break voted;
                };
                held = true;
                match hold(
                    client,
                    collector,
                    activity,
                    plan,
                    inner.session_id.as_deref(),
                    deadline,
                    until,
                )
                .await
                {
                    // What the engine said while the run was held is the run's last message.
                    Hold::Again => raw.message = collector.final_message(),
                    Hold::Ended(watched) => {
                        raw.watched = watched;
                        raw.cancel_sent = activity.cancel_sent();
                        raw.message = collector.final_message();
                        break None;
                    }
                }
            };
            if held {
                // The tree changed while the engine worked.
                files = match &machine {
                    Some(m) => {
                        m.w.tree
                            .changed_files(MAX_NEEDS_INPUT_FILES)
                            .await
                            .map(|c| c.total)
                    }
                    None => None,
                };
                last_files = files;
            }
            match vote {
                Some(TurnVote::Prompt(text)) => {
                    stage = Stage::Prompt(text);
                    continue;
                }
                Some(TurnVote::Stop { detail }) => {
                    hook_stop = Some(detail);
                    break raw;
                }
                _ => {}
            }
        }
        let finished = match machine.as_mut() {
            None => true,
            Some(_) if round_ended => true,
            Some(m) => match m
                .after_stage(
                    client,
                    collector,
                    activity,
                    plan,
                    &mut inner,
                    &mut raw,
                    files,
                    deadline,
                    fresh_failed,
                )
                .await
            {
                After::Finish => true,
                After::Run(next) => {
                    stage = next;
                    false
                }
            },
        };
        if !finished {
            continue;
        }
        // Everything else says the run is over: the one atomic last question of the modules, before the run is sealed. After a failed review round
        // nothing is sent into the session: a late message stays undelivered, as it does when any turn fails.
        if !hooks.is_empty() && !round_failed {
            let classified = classify_stage(
                &raw,
                files,
                collector,
                inner.failover.stage.disk_full.as_ref(),
            );
            match consult(
                &hooks,
                true,
                &raw,
                &classified,
                files,
                stops,
                &inner,
                collector,
                activity,
                plan,
                deadline,
            )
            .await
            {
                Some(TurnVote::Prompt(text)) => {
                    stage = Stage::Prompt(text);
                    continue;
                }
                Some(TurnVote::Stop { detail }) => hook_stop = Some(detail),
                _ => {}
            }
        }
        // Nothing else is pending: the review round. One extra prompt in the same session when the engine ended its turn with a message
        // and the harness counted a changed file; the rule is `review::skip_reason`, and a round that does not run is recorded after the loop.
        if let (Some(rp), Some(m)) = (review.as_ref(), machine.as_ref())
            && !round_ran
        {
            let gate = Gate {
                mode: rp.mode,
                hook_stop: hook_stop.is_some(),
                cancelled: plan.cancel.as_ref().is_some_and(|c| *c.borrow()),
                raw: &raw,
                files,
                left: deadline.saturating_duration_since(Instant::now()),
                guard: rp.guard,
            };
            let why = skip_reason(&gate);
            review_skipped = why;
            // The rule lets a round through only after a clean end of a turn, which is an answered prompt.
            if why.is_none()
                && let Watched::Done(Ok(done)) = &raw.watched
            {
                let (prompt, started_round) =
                    begin_round(rp, &*m.w.tree, &raw, done.clone(), files, &inner, tree).await;
                m.w.rec.record(
                    EventType::TurnProgress,
                    json!({
                        "turn": DIRECT_TURN,
                        "kind": "review-round",
                        "phase": "start",
                        "text": format!(
                            "The harness asks {} to review its own delivery once, as the maintainer who must merge it.",
                            m.w.engine_label
                        ),
                    }),
                );
                round = Some(started_round);
                round_ran = true;
                stage = Stage::Prompt(prompt);
                continue;
            }
        }
        break raw;
    };
    if let Some(sid) = &inner.session_id {
        hooks.event(&Event::SessionEnded(SessionEnded {
            session_id: sid,
            number: inner.sessions,
            why: SessionEnd::RunEnded,
        }));
    }
    let engine_stop = engine_stop_of(&raw, collector);
    // The last stage's cause: a stage that began later (the person's message) starts its facts over.
    let disk_full = inner.failover.stage.disk_full.take();
    // A round that was not sent is recorded with the first rule that stopped it, read from how the run ended.
    let review_note = review_note.or_else(|| {
        review.as_ref().map(|rp| {
            let reason = review_skipped.unwrap_or_else(|| {
                // The run ended before the loop came to its last question (a module's stop at a turn end): read the rule from how it ended.
                let gate = Gate {
                    mode: rp.mode,
                    hook_stop: hook_stop.is_some(),
                    cancelled: plan.cancel.as_ref().is_some_and(|c| *c.borrow()),
                    raw: &raw,
                    files: last_files,
                    left: deadline.saturating_duration_since(Instant::now()),
                    guard: rp.guard,
                };
                skip_reason(&gate).unwrap_or(REVIEW_SKIP_NOT_DONE)
            });
            DirectReview::skipped(reason)
        })
    });
    let (interventions, input_note, late_answers) = match machine {
        Some(m) => (
            m.interventions,
            m.note,
            Some(LateAnswers::new(
                m.w.instructions,
                m.answered,
                m.w.engine_label,
                m.w.rec,
            )),
        ),
        None => (Vec::new(), None, None),
    };
    DriverEnd {
        raw,
        session_id: inner.session_id,
        pinned: inner.pinned,
        usage: inner.usage.total().or(collector.usage()),
        nudges_sent: inner.nudges,
        max_silent_gap_s,
        prompt_started: inner.prompt_started,
        // A run with one try lists none.
        attempts: if inner.attempts.len() > 1 {
            inner.attempts
        } else {
            Vec::new()
        },
        failover: plan
            .failover
            .as_ref()
            .filter(|f| f.routes.len() > 1)
            .map(|f| inner.failover.note(f)),
        outage_s: inner.failover.outage_s(),
        final_model: inner.failover.final_model,
        switches: inner.failover.switches,
        interventions,
        fresh_sessions: inner.fresh,
        input_note,
        engine_stop,
        late_answers,
        hook_stop,
        disk_full,
        review: review_note,
    }
}

/// How a hold ([`TurnVote::Wait`]) ended.
enum Hold {
    /// The time asked for passed; the modules are asked again.
    Again,
    /// The run ended another way: the deadline, the silence limit, the user's cancel or the engine's pipe. It reads as that end of a prompt.
    Ended(Watched<Result<PromptDone, AcpError>>),
}

/// How often a hold looks at the clock, the silence and the user's cancel when the engine says nothing.
const HOLD_TICK: Duration = Duration::from_secs(1);

/// Writes `session/cancel` for the session, bounded: an engine that reads nothing must not hold the end of the run.
async fn write_cancel<R, W>(client: &mut AcpClient<R, W>, session: Option<&str>)
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    if let Some(sid) = session {
        let _ = tokio::time::timeout(
            CANCEL_WRITE_TIMEOUT,
            client.notify("session/cancel", json!({ "sessionId": sid })),
        )
        .await;
    }
}

/// Keeps the run open for `until` while no prompt is open (a module voted [`TurnVote::Wait`]): reads the engine's pipe, so that its
/// updates reach the collector and the wire tap and it never blocks on a full pipe, and watches what a prompt is watched for. The deadline
/// and the silence limit end the hold as they end a prompt (the cancel is written first); the user's cancel is written once and ends it
/// as the user's stop at once (no prompt is open, so no answer is waited for); a closed pipe ends it as a dead engine.
async fn hold<R, W>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    activity: &Activity,
    plan: &PromptPlan,
    session: Option<&str>,
    deadline: Instant,
    until: Duration,
) -> Hold
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let slice_end = Instant::now() + until;
    let mut cancel = plan.cancel.clone();
    loop {
        let now = Instant::now();
        if cancel.as_ref().is_some_and(|c| *c.borrow()) {
            write_cancel(client, session).await;
            activity.cancel_written();
            return Hold::Ended(Watched::Done(Err(AcpError::CancelIgnored)));
        }
        if now >= deadline {
            write_cancel(client, session).await;
            return Hold::Ended(Watched::TimedOut);
        }
        let silent = activity.silent_for();
        if silent >= plan.silence {
            write_cancel(client, session).await;
            return Hold::Ended(Watched::Stalled { silent });
        }
        if now >= slice_end {
            return Hold::Again;
        }
        let wake = slice_end.min(deadline).min(now + HOLD_TICK);
        let changed = async {
            match cancel.as_mut() {
                Some(c) => {
                    let _ = c.changed().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            r = client.pump_idle(collector) => {
                if let Err(e) = r {
                    return Hold::Ended(Watched::Done(Err(e)));
                }
            }
            () = tokio::time::sleep_until(wake) => {}
            () = changed => {}
        }
    }
}

/// Puts the end of a stage to the modules: the vote of the turn end (`seal` false) or the seal. `None` when the stage is not one a module
/// may continue: nothing was asked of the model, the prompt was not answered, the user's cancel is up or no time is left. A vote that comes back
/// after the user's cancel or the deadline is dropped.
#[allow(clippy::too_many_arguments)]
async fn consult(
    hooks: &Hooks,
    seal: bool,
    raw: &RawEnd,
    classified: &Classified,
    files: Option<u32>,
    n: u32,
    inner: &Inner,
    collector: &DirectCollector,
    activity: &Activity,
    plan: &PromptPlan,
    deadline: Instant,
) -> Option<TurnVote> {
    let cancelled = || plan.cancel.as_ref().is_some_and(|c| *c.borrow());
    let left = deadline.saturating_duration_since(Instant::now());
    // A module's interrupt was answered by the engine, or the engine did not answer it within the grace: both are the module's to decide.
    let interrupted = activity.interrupted();
    let answered = matches!(raw.watched, Watched::Done(Ok(_)))
        || (interrupted && matches!(raw.watched, Watched::Done(Err(AcpError::CancelIgnored))));
    if raw.setup_error.is_some() || !answered || cancelled() || left.is_zero() {
        return None;
    }
    let end = TurnEnd {
        n,
        session_id: inner.session_id.as_deref(),
        session: inner.sessions,
        raw,
        classified,
        files_changed: files,
        interrupted,
        time_left: left,
        tool_calls: collector.tool_calls(),
    };
    let vote = if seal {
        hooks.seal(&end).await
    } else {
        hooks.turn_end(&end).await
    };
    if cancelled() || deadline <= Instant::now() {
        return None;
    }
    Some(vote)
}

pub(crate) async fn step<R, W>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    stage: &'static str,
    method: &str,
    params: Value,
) -> Result<Value, Box<Fail>>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    match tokio::time::timeout(
        SETUP_STEP_TIMEOUT,
        client.request(method, params, collector),
    )
    .await
    {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(err)) => Err(Box::new(Fail {
            setup: true,
            stage,
            err,
            fresh_start: false,
        })),
        Err(_) => Err(Box::new(Fail {
            setup: true,
            stage,
            err: AcpError::Protocol(format!(
                "no answer within {} s",
                SETUP_STEP_TIMEOUT.as_secs()
            )),
            fresh_start: false,
        })),
    }
}

pub(crate) async fn send_prompt<R, W>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    session_id: &str,
    text: &str,
) -> Result<(PromptDone, Option<Usage>), AcpError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    collector.begin_prompt();
    let hooks = collector.hooks().clone();
    let no = collector.prompts_begun();
    if !hooks.is_empty() {
        hooks.event(&Event::PromptSent(PromptSent {
            no,
            session_id,
            text,
        }));
    }
    let reply = client
        .request(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": text }] }),
            collector,
        )
        .await;
    if !hooks.is_empty() {
        hooks.event(&Event::PromptReplied(PromptReplied {
            no,
            session_id,
            stop_reason: reply
                .as_ref()
                .ok()
                .and_then(|p| p.get("stopReason").and_then(Value::as_str)),
            usage: reply.as_ref().ok().and_then(read_usage),
            error: reply.as_ref().err(),
        }));
    }
    let p = reply?;
    let reported = read_usage(&p);
    Ok((
        PromptDone {
            stop_reason: p
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            usage: reported,
        },
        reported,
    ))
}

// ---- the answer window --------------------------------------------------------------------------------------------------------

/// Where agentd puts the audit copies of what it wrote for the people who read the run later (`fresh-session-<n>.md`, `stop-digest-<n>.json`
/// under the run's `agents/direct/` prefix). A failure is logged and never stops the run.
pub trait AuditSink: Send + Sync {
    fn put<'a>(&'a self, file: &'a str, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), String>>;
}

/// The default of a fresh session the person asked for: a new session in the live process (`false`) or a restart of the engine process
/// (`true`).
pub const FRESH_SESSION_RESTARTS_PROCESS: bool = false;

/// Everything a run needs to ask its user what to do when it would otherwise end. A run without it is the run of today:
/// one stage, no question.
pub struct InputWiring {
    pub policy: InputPolicy,
    /// The answers the internal API delivers (`TurnControls::take_instructions`).
    pub instructions: mpsc::Receiver<Instruct>,
    /// The user's cancel.
    pub cancel: watch_chan::Receiver<bool>,
    /// What the work tree holds, read through agentd's private repository, never `git` in the engine's.
    pub tree: Arc<dyn TreeSource>,
    /// The harness facts the digest is made of.
    pub sources: Arc<dyn DigestSource>,
    /// `<agent_data>/context/`, outside the work tree.
    pub context: ContextDir,
    /// The user's first prompt, byte for byte.
    pub task: String,
    /// How the engine is named in the texts of the events (`engine::direct::engine_label`): the driver itself names no engine.
    pub engine_label: String,
    /// The routes `try_model` may name: the invocation's own, which the session credentials cover.
    pub routes: Vec<NeedsInputModel>,
    /// Where the question, the acknowledgements and the new-session events go.
    pub rec: Arc<EventRecorder>,
    pub audit: Option<Arc<dyn AuditSink>>,
    pub log: Option<Arc<dyn LogSink>>,
    /// The review round: one extra prompt in the same session after a delivery the engine called done. `None` sends none and records
    /// nothing.
    pub review: Option<ReviewPlan>,
}

impl std::fmt::Debug for InputWiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputWiring")
            .field("policy", &self.policy)
            .field("routes", &self.routes)
            .finish_non_exhaustive()
    }
}

/// What follows a stage.
enum After {
    Finish,
    Run(Stage),
}

/// What follows a question.
enum Asked {
    Run(Stage),
    Finish,
    /// The engine died while the question was open: assess the stop again.
    Reassess,
}

/// The one task that owns every transition of the answer window: the state of the questions, the sessions and the waits.
struct Machine {
    w: InputWiring,
    st: RunInputState,
    interventions: Vec<DirectIntervention>,
    note: Option<InputNote>,
    started: Instant,
    /// The ids of the questions an accepted answer closed: a second delivery of the same answer is `already-answered`.
    answered: Vec<String>,
    /// Silences resumed so far ([`SILENCE_RESUMES`]).
    silence_resumes: u32,
}

/// The longest the upload of a stop's digest may hold the run.
const STOP_DIGEST_UPLOAD_LIMIT: Duration = Duration::from_secs(30);

/// Writes the digest of stop `n` (the recorder's bounded record of the last tool calls, messages and counters, redacted at write time) to
/// the run's audit prefix, at EVERY stop, with or without an answer window. A failed or slow upload is the sink's to log and never stops the run.
#[allow(clippy::too_many_arguments)]
async fn write_stop_digest(
    audit: Option<Arc<dyn AuditSink>>,
    collector: &DirectCollector,
    raw: &RawEnd,
    files: Option<u32>,
    n: u32,
    inner: &Inner,
    plan: &PromptPlan,
    started: Instant,
) {
    let Some(audit) = audit else {
        return;
    };
    let classified = classify_stage(
        raw,
        files,
        collector,
        inner.failover.stage.disk_full.as_ref(),
    );
    let ctx = StopContext {
        outcome: Some(classified.outcome.as_str().to_string()),
        label: Some(classified.outcome.label().to_string()),
        detail: classified.detail.clone(),
        stop_reason: crate::direct::classify::stop_reason(raw),
        session: inner.sessions,
        run_s: started.elapsed().as_secs(),
        files_changed: files,
        model: Some(
            inner
                .failover
                .final_model
                .clone()
                .unwrap_or_else(|| plan.model.clone()),
        ),
        final_message: None,
    };
    let bytes = collector.stop_digest(n, ctx).to_json().into_bytes();
    let file = format!("stop-digest-{n}.json");
    // The sink logs a failure; the result is of no further use here.
    let _ = tokio::time::timeout(STOP_DIGEST_UPLOAD_LIMIT, audit.put(&file, bytes)).await;
}

// ---- the review round ---------------------------------------------------------------------------------------------------

/// The review round in flight: what the stage loop keeps while its turn is open.
struct Round {
    started: Instant,
    /// What the run had used when the round began.
    usage_before: Option<Usage>,
    /// The work tree's fingerprint and `HEAD` when the round began.
    fp_before: String,
    head_before: Option<String>,
    /// How the turn before the round ended: the run keeps it when the round fails.
    done: PromptDone,
    message: String,
}

/// The facts of the work tree, the prompt made from them, and the round's starting point.
async fn begin_round(
    rp: &ReviewPlan,
    w_tree: &dyn TreeSource,
    raw: &RawEnd,
    done: PromptDone,
    files: Option<u32>,
    inner: &Inner,
    tree: &dyn TreeProbe,
) -> (String, Round) {
    let (paths, total) = match w_tree.changed_files(FACTS_PATHS_ASKED).await {
        Some(c) => (c.paths, c.total),
        None => (Vec::new(), files.unwrap_or(0)),
    };
    let top = rp.repo.base_top_level().await;
    let facts = facts_text(&FactsInput {
        paths: &paths,
        total,
        base_top_level: top.as_deref(),
        untracked: rp.repo.untracked().await,
    });
    let round = Round {
        started: Instant::now(),
        usage_before: inner.usage.total(),
        fp_before: tree.fingerprint().await,
        head_before: rp.repo.head().await,
        done,
        message: raw.message.clone(),
    };
    (render(&rp.text, &rp.base, &facts), round)
}

/// A work-tree fingerprint that says something: not empty (the probe of a run without one) and not an error string.
fn fingerprint_known(f: &str) -> bool {
    !f.is_empty() && !f.starts_with("error-")
}

/// The part of a fingerprint that is the work tree's content: the probe writes `HEAD`, a line feed and the tree's hash, and a commit that
/// records what was already in the tree moves `HEAD` and no file. A fingerprint without a line feed is all content.
fn tree_part(f: &str) -> &str {
    f.split_once('\n').map_or(f, |(_, tree)| tree)
}

/// The round's turn ended. A turn that ended with a message is the run's last turn, and its message is the run's last message. Any other end
/// of it (the provider failed, the engine died or stalled, the deadline came, the turn had no message or ended for length) puts back the turn
/// before the round, so the run ends with the outcome it would have had without the round. The user's cancel is the one exception: the user
/// stopped the run, and it ends as that.
async fn end_round(
    r: Round,
    rp: &ReviewPlan,
    raw: &mut RawEnd,
    plan: &PromptPlan,
    inner: &Inner,
    tree: &dyn TreeProbe,
    rec: Option<&EventRecorder>,
) -> DirectReview {
    let clean = classify(raw).outcome == DirectOutcome::ReportedDone;
    // The user's stop only when the engine was told to cancel: a cancel that came after the round had failed for its own reason does not make
    // the failure the user's.
    let cancelled = plan.cancel.as_ref().is_some_and(|c| *c.borrow()) && raw.cancel_sent;
    let result = if clean {
        REVIEW_COMPLETED
    } else if cancelled {
        REVIEW_CANCELLED
    } else {
        REVIEW_FAILED
    };
    if result == REVIEW_FAILED {
        *raw = RawEnd {
            watched: Watched::Done(Ok(r.done)),
            cancel_sent: false,
            message: r.message,
            setup_error: None,
        };
    }
    let fp_after = tree.fingerprint().await;
    let after = inner.usage.total();
    // Tokens only when the engine reported something for the round; a total that did not move says nothing, it is not zero.
    let (input_tokens, output_tokens) = match after {
        Some(a) if after != r.usage_before => {
            let b = r.usage_before.unwrap_or_default();
            (
                Some(a.input_tokens.saturating_sub(b.input_tokens)),
                Some(a.output_tokens.saturating_sub(b.output_tokens)),
            )
        }
        _ => (None, None),
    };
    let note = DirectReview {
        status: REVIEW_RAN.to_string(),
        reason: None,
        commit_before: r.head_before,
        commit_after: rp.repo.head().await,
        changed_files: (fingerprint_known(&r.fp_before) && fingerprint_known(&fp_after))
            .then(|| tree_part(&r.fp_before) != tree_part(&fp_after)),
        seconds: Some(r.started.elapsed().as_secs()),
        input_tokens,
        output_tokens,
        result: Some(result.to_string()),
    };
    if let Some(rec) = rec {
        rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "review-round",
                "phase": "end",
                "result": result,
                "text": note.sentence(),
            }),
        );
    }
    note
}

/// The facts of the attempt that ended in the engine's own stop (a prompt it ended `cancelled` with nobody having asked), for the label
/// and the hint of that stop; `None` for every other end.
pub(crate) fn engine_stop_of(raw: &RawEnd, collector: &DirectCollector) -> Option<StopReport> {
    let ended_cancelled = matches!(
        &raw.watched,
        Watched::Done(Ok(done)) if done.stop_reason == "cancelled"
    );
    (ended_cancelled && !raw.cancel_sent && raw.setup_error.is_none())
        .then(|| StopReport::new(collector.attempt_facts(), Duration::ZERO))
}

/// The stop table's reading of a stage's end, with the delivered tree, the engine-stop facts and the disk cause applied.
fn classify_stage(
    raw: &RawEnd,
    files: Option<u32>,
    collector: &DirectCollector,
    disk: Option<&DiskFull>,
) -> Classified {
    let c = refine(
        classify(raw),
        MeasuredTree {
            files_changed: files,
        },
    );
    let c = match engine_stop_of(raw, collector) {
        Some(report) => refine_engine_stop(c, &report),
        None => c,
    };
    with_disk(c, disk)
}

/// True while the engine's output can still be read.
fn pipe_open(raw: &RawEnd) -> bool {
    !matches!(
        &raw.watched,
        Watched::Done(Err(AcpError::Closed
            | AcpError::Io(_)
            | AcpError::Protocol(_)))
    )
}

impl Machine {
    fn new(w: InputWiring, started: Instant) -> Self {
        Self {
            w,
            st: RunInputState::default(),
            interventions: Vec::new(),
            note: None,
            started,
            answered: Vec::new(),
            silence_resumes: 0,
        }
    }

    fn log(&self, line: &str) {
        if let Some(l) = &self.w.log {
            l.line(line);
        }
    }

    /// Answers that reached the channel while no question was open (a second delivery of an answer, an answer for a question that
    /// closed): each is acknowledged as ignored, with the reason that tells the two apart.
    fn drain_stale(&mut self) {
        drain_stale(
            &mut self.w.instructions,
            &self.answered,
            &self.w.rec,
            &self.w.engine_label,
        );
    }

    /// The prompt in flight when the silence limit came was dropped before it could be written down: the attempt list gets it here, so
    /// the resume that follows is a numbered try of its own.
    fn note_stalled_attempt(&self, inner: &mut Inner, classified: &Classified) {
        let now = now_unix();
        let silent = classified
            .fault_hint
            .as_ref()
            .and_then(|h| h.silent_s)
            .map_or(0, i64::from);
        let n = u32::try_from(inner.attempts.len() + 1).unwrap_or(u32::MAX);
        inner.attempts.push(DirectAttempt {
            n,
            kind: inner.stage_kind,
            started_at: format_rfc3339(now - silent),
            ended_at: format_rfc3339(now),
            result: DirectAttemptResult::ProviderError,
            retry_class: Some("silence".into()),
            detail: classified.detail.clone(),
            backoff_s: None,
            model_id: inner.failover.final_model.clone(),
        });
    }

    fn stage_facts(
        &self,
        collector: &DirectCollector,
        plan: &PromptPlan,
        inner: &Inner,
        raw: &RawEnd,
        files: Option<u32>,
    ) -> AssessFacts {
        let (client_alive, prompt_answered) = match &raw.watched {
            Watched::Done(Ok(_)) => (true, true),
            // An error answer is the engine's answer to the prompt: the session is alive and can take another prompt.
            Watched::Done(Err(AcpError::Rpc { .. })) => (true, true),
            _ => (false, false),
        };
        let current = inner
            .failover
            .final_model
            .as_deref()
            .unwrap_or(plan.model.as_str());
        AssessFacts {
            client_alive,
            prompt_answered,
            attempts: u32::try_from(inner.attempts.len()).unwrap_or(u32::MAX),
            tool_calls: collector.tool_calls(),
            identical_calls: inner.failover.stage.identical_calls,
            failed_turns: inner.failover.stage.failed_turns,
            files_changed: files,
            stage_engine_stops: inner.failover.stage.stage_engine_stops,
            cap_hit: inner.failover.stage.cap_hit,
            routes: self
                .w
                .routes
                .iter()
                .filter(|m| m.id != current)
                .cloned()
                .collect(),
            run_s: self.started.elapsed().as_secs(),
            disk_full: inner.failover.stage.disk_full.clone(),
        }
    }

    /// The stage has ended. Decides whether the run is over, an automatic fresh session follows, or the person is asked (rows 4 to 6 of
    /// the state machine, and everything the question leads to).
    #[allow(clippy::too_many_arguments)]
    async fn after_stage<R, W>(
        &mut self,
        client: &mut AcpClient<R, W>,
        collector: &mut DirectCollector,
        activity: &Arc<Activity>,
        plan: &PromptPlan,
        inner: &mut Inner,
        raw: &mut RawEnd,
        files: Option<u32>,
        deadline: Instant,
        fresh_failed: bool,
    ) -> After
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        self.drain_stale();
        let mut classified = classify_stage(
            raw,
            files,
            collector,
            inner.failover.stage.disk_full.as_ref(),
        );
        let mut facts = self.stage_facts(collector, plan, inner, raw, files);
        // Silence (the recovery ladder's class 7): the driver wrote the cancel at the stall; the run resumes the same session once, and
        // the second silence is a question.
        if classified.outcome == DirectOutcome::StoppedSilence
            && self.silence_resumes < SILENCE_RESUMES
            && deadline > Instant::now()
        {
            self.silence_resumes += 1;
            self.note_stalled_attempt(inner, &classified);
            self.log(&format!(
                "direct turn silence: {}; the run resumes the session once",
                classified.detail.as_deref().unwrap_or("no engine output")
            ));
            collector
                .hooks()
                .event(&Event::Ladder(LadderEvent::SilenceResume {
                    detail: classified.detail.as_deref().unwrap_or("no engine output"),
                }));
            settle_after_cancel(client, collector).await;
            return After::Run(Stage::Resume(RESUME_PROMPT_INTERRUPTED.to_string()));
        }
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match assess(&classified, &facts, &self.st, &self.w.policy, left) {
                Assessment::Finish => {
                    if fresh_failed {
                        self.note = Some(InputNote::FreshSessionFailed);
                    }
                    return After::Finish;
                }
                Assessment::Auto(auto) => {
                    self.st.auto_sessions += 1;
                    let restart = auto.restart_process || !facts.client_alive;
                    let spec = self
                        .fresh_spec(
                            collector,
                            inner,
                            &classified,
                            &facts,
                            auto.reason,
                            restart,
                            SessionTrigger::Harness,
                            None,
                            (None, None),
                        )
                        .await;
                    return After::Run(Stage::FreshSession(spec));
                }
                Assessment::Ask(ask) => {
                    match self
                        .ask(
                            client,
                            collector,
                            activity,
                            plan,
                            inner,
                            raw,
                            &classified,
                            &facts,
                            ask,
                            deadline,
                        )
                        .await
                    {
                        Asked::Run(stage) => return After::Run(stage),
                        Asked::Finish => return After::Finish,
                        Asked::Reassess => {
                            classified = classify_stage(
                                raw,
                                files,
                                collector,
                                inner.failover.stage.disk_full.as_ref(),
                            );
                            facts.client_alive = false;
                            facts.prompt_answered = false;
                        }
                    }
                }
            }
        }
    }

    /// The facts a digest reads: what the sources gather (the private repository, the progress file, the engine's error output) and
    /// what the driver itself holds (the attempts, the collector's tool notes and messages).
    async fn gather(
        &self,
        collector: &DirectCollector,
        inner: &Inner,
        classified: &Classified,
    ) -> DigestSources {
        let mut src = self.w.sources.gather().await;
        src.task = self.w.task.clone();
        src.attempts = inner.attempts.clone();
        src.shape = vec![ShapeCounters {
            attempt: u32::try_from(inner.attempts.len()).unwrap_or(u32::MAX),
            facts: collector.attempt_facts(),
        }];
        src.tools = collector.tool_notes();
        let last = collector.final_message();
        src.last_message = if last.trim().is_empty() {
            collector.last_messages().last().cloned()
        } else {
            Some(last)
        };
        src.test_tail = collector.failed_test_tail();
        src.stop_outcome = Some(classified.outcome.as_str().to_string());
        src
    }

    /// The numbers the card shows and the new-session prompt quotes.
    fn evidence(
        &self,
        facts: &AssessFacts,
        classified: &Classified,
        inner: &Inner,
    ) -> NeedsInputEvidence {
        NeedsInputEvidence {
            attempts: facts.attempts,
            tool_calls: facts.tool_calls,
            files_changed: facts.files_changed.unwrap_or(0),
            identical_calls: facts.identical_calls,
            failed_turns: facts.failed_turns,
            // A full disk shows its own line: the stop detail speaks of a run that ended, and the run is waiting for a person.
            last_error: facts
                .disk_full
                .as_ref()
                .map(DiskFull::line)
                .or_else(|| classified.detail.clone()),
            run_s: facts.run_s,
            sessions: inner.sessions,
        }
    }

    /// Builds the first prompt of a fresh session and writes what goes with it: the digest of the harness's own facts, the context files
    /// outside the work tree and the audit copy.
    #[allow(clippy::too_many_arguments)]
    async fn fresh_spec(
        &mut self,
        collector: &DirectCollector,
        inner: &Inner,
        classified: &Classified,
        facts: &AssessFacts,
        reason: NeedsInputReason,
        restart: bool,
        trigger: SessionTrigger,
        request_id: Option<String>,
        (user_text, analyzer_notes): (Option<String>, Option<String>),
    ) -> FreshSpec {
        let sources = self.gather(collector, inner, classified).await;
        let d = digest(&sources, DIGEST_BUDGET_BYTES);
        let n = inner.sessions + 1;
        let text = d.text();
        let (bytes, sha) = match self
            .w
            .context
            .write_task(&self.w.task)
            .and_then(|_| self.w.context.write_digest(n, &text))
        {
            Ok(f) => (f.bytes, f.sha256),
            Err(e) => {
                // The prompt carries an inline summary, so the new session works without the files.
                self.log(&format!(
                    "direct turn context files not written: {}",
                    scrub_detail(&e)
                ));
                (d.len() as u64, d.sha256())
            }
        };
        if let Some(audit) = &self.w.audit
            && let Err(e) = audit
                .put(&format!("fresh-session-{n}.md"), text.clone().into_bytes())
                .await
        {
            self.log(&format!(
                "direct turn digest audit copy not uploaded: {}",
                scrub_detail(&e)
            ));
        }
        let why = StopWhy::of(
            reason,
            &self.evidence(facts, classified, inner),
            Some(classified.outcome.as_str()),
            d.files(),
        );
        let prompt = new_session_prompt(
            &self.w.task,
            &why,
            &d,
            user_text.as_deref(),
            analyzer_notes.as_deref(),
            self.w.context.path(),
        );
        FreshSpec {
            restart,
            prompt,
            trigger,
            reason: reason.as_str().to_string(),
            request_id,
            digest_bytes: bytes,
            digest_sha256: sha,
        }
    }

    /// Raises the question, waits for its end and turns the end into the next step (rows 6 to 14 of the state machine).
    #[allow(clippy::too_many_arguments)]
    async fn ask<R, W>(
        &mut self,
        client: &mut AcpClient<R, W>,
        collector: &mut DirectCollector,
        activity: &Arc<Activity>,
        plan: &PromptPlan,
        inner: &Inner,
        raw: &mut RawEnd,
        classified: &Classified,
        facts: &AssessFacts,
        ask: Ask,
        deadline: Instant,
    ) -> Asked
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        self.st.requests += 1;
        let n = self.st.requests;
        let sources = self.gather(collector, inner, classified).await;
        let d = digest(&sources, DIGEST_BUDGET_BYTES);
        let opened = Instant::now();
        let opened_at = format_rfc3339(now_unix());
        let expires = format_rfc3339(
            now_unix() + i64::try_from(ask.window.as_secs()).unwrap_or(i64::MAX / 4),
        );
        let evidence = self.evidence(facts, classified, inner);
        // The question's event id IS the request id; it is made at the append so that no event of the run sorts between the two.
        let request_id = collector.record_needs_input_minted(|id| {
            needs_input_payload_for(
                id,
                n,
                &ask,
                &d,
                &evidence,
                &expires,
                facts.disk_full.as_ref(),
            )
        });
        self.log(&format!(
            "direct turn needs input: request {request_id} n={n} reason={} window={} s",
            reason_word(ask.reason, facts.disk_full.as_ref()),
            ask.window.as_secs()
        ));
        // A person is deciding: the engine's silence is not the provider's.
        activity.hold();
        let mut window = Window::open(&request_id, &ask, &self.st, &self.w.policy, opened);
        let end = wait_for_answer(
            client,
            collector,
            WaitCtx {
                rec: &self.w.rec,
                instructions: &mut self.w.instructions,
                cancel: &mut self.w.cancel,
                request_id: &request_id,
                who: &self.w.engine_label,
                answered: &self.answered,
                models: &ask.models,
                opened,
                pipe_open: pipe_open(raw),
                deadline,
            },
            &mut window,
        )
        .await;
        activity.release();
        let waited = opened.elapsed();
        self.st.waited += waited;
        let waited_s = waited.as_secs();
        let next_attempt = u32::try_from(inner.attempts.len() + 1).unwrap_or(u32::MAX);
        let last_attempt = u32::try_from(inner.attempts.len()).unwrap_or(u32::MAX);
        let mut record = DirectIntervention {
            n,
            request_id: request_id.clone(),
            reason: reason_word(ask.reason, facts.disk_full.as_ref()).to_string(),
            opened_at,
            closed_at: format_rfc3339(now_unix()),
            waited_s,
            outcome: String::new(),
            action: None,
            origin: None,
            model: None,
            text_bytes: None,
            at_attempt: last_attempt,
        };
        let close = |how: &str| {
            self.w.rec.record(
                EventType::TurnProgress,
                closed_event(&request_id, how, waited_s),
            );
        };
        match end {
            WaitEnd::Answered(a) => {
                self.answered.push(request_id.clone());
                record.action = Some(a.action);
                record.origin = Some(a.origin);
                record.model = a.model.clone();
                record.text_bytes = Some(a.text.as_ref().map_or(0, |t| t.len() as u64));
                record.at_attempt = next_attempt;
                match a.action {
                    InstructAction::Finish => {
                        close("finish");
                        record.outcome = "finish".into();
                        record.at_attempt = last_attempt;
                        self.interventions.push(record);
                        self.note = Some(InputNote::ChoseFinish);
                        Asked::Finish
                    }
                    InstructAction::ContinueWithPrompt => {
                        record.outcome = "answered".into();
                        self.interventions.push(record);
                        Asked::Run(Stage::Prompt(a.text.unwrap_or_default()))
                    }
                    InstructAction::TryModel => {
                        record.outcome = "answered".into();
                        self.interventions.push(record);
                        let id = a.model.unwrap_or_default();
                        let route = plan
                            .failover
                            .as_ref()
                            .and_then(|f| f.routes.iter().find(|r| r.id == id))
                            .cloned()
                            .unwrap_or_else(|| {
                                let known = ask.models.iter().find(|m| m.id == id);
                                Route {
                                    id: id.clone(),
                                    region: String::new(),
                                    context_tokens: known.map_or(0, |m| m.context_tokens),
                                    max_output_tokens: 0,
                                }
                            });
                        Asked::Run(Stage::TryModel {
                            route,
                            text: a.text,
                        })
                    }
                    InstructAction::NewSessionWithPrompt => {
                        record.outcome = "answered".into();
                        self.interventions.push(record);
                        self.st.user_sessions += 1;
                        let trigger = match a.origin {
                            InstructOrigin::User => SessionTrigger::User,
                            InstructOrigin::Analyzer => SessionTrigger::Analyzer,
                        };
                        let texts = match a.origin {
                            InstructOrigin::User => (a.text, None),
                            InstructOrigin::Analyzer => (None, a.text),
                        };
                        let restart = !facts.client_alive || FRESH_SESSION_RESTARTS_PROCESS;
                        let spec = self
                            .fresh_spec(
                                collector,
                                inner,
                                classified,
                                facts,
                                ask.reason,
                                restart,
                                trigger,
                                Some(request_id.clone()),
                                texts,
                            )
                            .await;
                        Asked::Run(Stage::FreshSession(spec))
                    }
                }
            }
            WaitEnd::Expired => {
                close("expired");
                record.outcome = "expired".into();
                self.interventions.push(record);
                self.note = Some(InputNote::NobodyAnswered { waited_s });
                Asked::Finish
            }
            WaitEnd::Cancelled => {
                close("cancelled");
                record.outcome = "cancelled".into();
                self.interventions.push(record);
                // The user's stop: the run ends as the user's cancel, whatever stop was held.
                activity.cancel_written();
                *raw = RawEnd {
                    watched: Watched::Done(Ok(PromptDone {
                        stop_reason: "cancelled".into(),
                        usage: None,
                    })),
                    cancel_sent: true,
                    message: raw.message.clone(),
                    setup_error: None,
                };
                Asked::Finish
            }
            WaitEnd::Deadline => {
                close("deadline");
                record.outcome = "deadline".into();
                self.interventions.push(record);
                raw.watched = Watched::TimedOut;
                Asked::Finish
            }
            WaitEnd::EngineDied => {
                close("superseded");
                record.outcome = "superseded".into();
                self.interventions.push(record);
                raw.watched = Watched::Done(Err(AcpError::Closed));
                raw.cancel_sent = false;
                Asked::Reassess
            }
        }
    }
}

/// After the cancel of a silence: reads the engine's pipe until it has been quiet for [`SETTLE_QUIET`] (at most [`SETTLE_MAX`]), so that the
/// answer to the cancelled prompt is consumed before the resume is written to a session that may still be busy. A closed pipe ends it at
/// once; the resume then fails and the stop reads as a dead engine.
async fn settle_after_cancel<R, W>(client: &mut AcpClient<R, W>, collector: &mut DirectCollector)
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let end = Instant::now() + SETTLE_MAX;
    loop {
        let until = (Instant::now() + SETTLE_QUIET).min(end);
        match tokio::time::timeout_at(until, client.pump_idle(collector)).await {
            Ok(Ok(())) if Instant::now() < end => {}
            _ => return,
        }
    }
}

// ---- the stages ---------------------------------------------------------------------------------------------------------------

/// The plan one stage runs under: the same settings, and a ladder that begins on the route the engine is really on (after earlier
/// switches, a user's prompt must not begin on route 0). A fresh session begins on the route the user chose.
fn stage_plan(plan: &PromptPlan, inner: &Inner, on_real_route: bool) -> PromptPlan {
    let mut p = plan.clone();
    if on_real_route && let Some(f) = p.failover.take() {
        p.failover = Some(f.starting_on(inner.failover.final_model.as_deref()));
    }
    p
}

fn no_session() -> Box<Fail> {
    Box::new(Fail {
        setup: false,
        stage: "session/prompt",
        err: AcpError::Protocol("the run has no engine session".into()),
        fresh_start: false,
    })
}

/// One later stage of the run (the first is [`exchange`]).
#[allow(clippy::too_many_arguments)]
async fn run_stage<R, W, S>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    activity: &Arc<Activity>,
    plan: &PromptPlan,
    tree: &dyn TreeProbe,
    inner: &mut Inner,
    host: &mut S,
    rec: Option<&EventRecorder>,
    stage: &Stage,
) -> Result<PromptDone, Box<Fail>>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
    S: Respawn<R = R, W = W>,
{
    // What one stage must not hand to the next: a tool a dropped prompt left running, the cancel flag, the stage's own facts.
    activity.reset_for_stage();
    inner.failover.begin_stage();
    match stage {
        Stage::Prompt(text) | Stage::Resume(text) => {
            let sid = inner.session_id.clone().ok_or_else(no_session)?;
            let sp = stage_plan(plan, inner, true);
            let kind = if matches!(stage, Stage::Resume(_)) {
                KIND_RESUME
            } else {
                KIND_INSTRUCTION
            };
            send_and_nudge(
                client, collector, activity, &sp, tree, inner, &sid, text, kind,
            )
            .await
        }
        Stage::TryModel { route, text } => {
            let sid = inner.session_id.clone().ok_or_else(no_session)?;
            let timeout = plan
                .failover
                .as_ref()
                .map_or(Duration::from_secs(30), |f| f.switch_timeout);
            set_model(client, collector, &sid, route, timeout)
                .await
                .map_err(|why| {
                    Box::new(Fail {
                        setup: false,
                        stage: "session/set_config_option",
                        err: AcpError::Protocol(format!(
                            "the engine did not take model {}: {why}",
                            route.id
                        )),
                        fresh_start: false,
                    })
                })?;
            let from = inner
                .failover
                .final_model
                .clone()
                .unwrap_or_else(|| plan.model.clone());
            let switch = DirectSwitch {
                n: u32::try_from(inner.failover.switches.len() + 1).unwrap_or(u32::MAX),
                at: format_rfc3339(now_unix()),
                from,
                to: route.id.clone(),
                kind: DirectSwitchKind::Model,
                // The person chose it: the ladder's fruitless-switch bound never sees it.
                class: "user".to_string(),
                detail: None,
            };
            collector.record_provider_switch(&switch);
            inner.failover.final_model = Some(route.id.clone());
            inner.failover.switches.push(switch);
            let sp = stage_plan(plan, inner, true);
            let sending = text
                .clone()
                .unwrap_or_else(|| RESUME_PROMPT_INTERRUPTED.to_string());
            send_and_nudge(
                client,
                collector,
                activity,
                &sp,
                tree,
                inner,
                &sid,
                &sending,
                KIND_INSTRUCTION,
            )
            .await
        }
        Stage::FreshSession(spec) => {
            let sid = match fresh::start(client, collector, activity, plan, inner, host, spec).await
            {
                Ok(sid) => sid,
                Err(mut f) => {
                    f.fresh_start = true;
                    return Err(f);
                }
            };
            if let Some(rec) = rec {
                rec.record(
                    EventType::TurnProgress,
                    fresh::new_session_event(inner.sessions, spec, spec.prompt.len()),
                );
            }
            let sp = stage_plan(plan, inner, false);
            send_and_nudge(
                client,
                collector,
                activity,
                &sp,
                tree,
                inner,
                &sid,
                &spec.prompt,
                KIND_FRESH,
            )
            .await
        }
    }
}

const KIND_INSTRUCTION: DirectAttemptKind = DirectAttemptKind::Instruction;
const KIND_RESUME: DirectAttemptKind = DirectAttemptKind::Resume;
const KIND_FRESH: DirectAttemptKind = DirectAttemptKind::FreshSession;

pub(crate) fn init_params() -> Value {
    json!({ "protocolVersion": 1, "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false } })
}

/// The first stage: setup (`initialize`, `session/new`, the model pin), then the user's prompt byte for byte.
async fn exchange<R, W>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    activity: &Arc<Activity>,
    plan: &PromptPlan,
    tree: &dyn TreeProbe,
    inner: &mut Inner,
) -> Result<PromptDone, Box<Fail>>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    step(client, collector, "initialize", "initialize", init_params()).await?;
    // No `_meta`: the engine's own system prompt and its rules stand as they are.
    let new = step(
        client,
        collector,
        "session/new",
        "session/new",
        json!({ "cwd": plan.cwd, "mcpServers": [] }),
    )
    .await?;
    let session_id = new
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Box::new(Fail {
                setup: true,
                stage: "session/new",
                err: AcpError::Protocol("session/new returned no sessionId".into()),
                fresh_start: false,
            })
        })?
        .to_string();
    inner.session_id = Some(session_id.clone());
    // Events of any other session (an abandoned one) are dropped from here on.
    client.set_session(&session_id);
    let got = model_option(&new).flatten();
    inner.pinned = got.clone();
    if got.as_deref() != Some(plan.model.as_str()) {
        return Err(Box::new(Fail {
            setup: true,
            stage: "model pin",
            err: AcpError::ModelMismatch {
                wanted: plan.model.clone(),
                got,
            },
            fresh_start: false,
        }));
    }
    collector
        .hooks()
        .event(&Event::SessionStarted(SessionStarted {
            session_id: &session_id,
            number: 1,
            response: &new,
            cwd: &plan.cwd,
            trigger: None,
        }));
    send_and_nudge(
        client,
        collector,
        activity,
        plan,
        tree,
        inner,
        &session_id,
        &plan.prompt,
        DirectAttemptKind::Prompt,
    )
    .await
}

/// Sends `text` as the prompt of the live session (with the ladder of retries and switches), then the optional nudge.
#[allow(clippy::too_many_arguments)]
async fn send_and_nudge<R, W>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    activity: &Arc<Activity>,
    plan: &PromptPlan,
    tree: &dyn TreeProbe,
    inner: &mut Inner,
    session_id: &str,
    text: &str,
    first_kind: DirectAttemptKind,
) -> Result<PromptDone, Box<Fail>>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let fail = |err| {
        Box::new(Fail {
            setup: false,
            stage: "session/prompt",
            err,
            fresh_start: false,
        })
    };
    // The nudge compares the tree before and after each prompt; without one the probe is never called.
    let mut before = match &plan.nudge {
        Some(_) => tree.fingerprint().await,
        None => String::new(),
    };
    if inner.prompt_started.is_none() {
        inner.prompt_started = Some(Instant::now());
    }
    inner.stage_kind = first_kind;
    let (mut done, reported) = prompt_with_retry_from(
        client,
        collector,
        activity,
        session_id,
        text,
        plan,
        &mut inner.attempts,
        &mut inner.failover,
        first_kind,
    )
    .await
    .map_err(fail)?;
    inner.usage.after_prompt(reported, collector.usage());
    let Some(nudge) = &plan.nudge else {
        done.usage = inner.usage.total();
        return Ok(done);
    };
    // The nudge: only after an end_turn that did not say DONE, never after a cancel, and not while nothing changes.
    let mut unchanged = 0;
    loop {
        let after = tree.fingerprint().await;
        unchanged = if after == before { unchanged + 1 } else { 0 };
        before = after;
        if done.stop_reason != "end_turn"
            || activity.cancel_sent()
            || plan.cancel.as_ref().is_some_and(|c| *c.borrow())
            || says_done(&collector.final_message())
            || inner.nudges >= nudge.max
            || unchanged >= NUDGE_UNCHANGED_LIMIT
        {
            break;
        }
        inner.nudges += 1;
        disk_boundary(&plan.retry, collector, &mut inner.failover).await;
        let (next, reported) = send_prompt(client, collector, session_id, &nudge.text)
            .await
            .map_err(fail)?;
        inner.usage.after_prompt(reported, collector.usage());
        done = next;
    }
    done.usage = inner.usage.total();
    Ok(done)
}
