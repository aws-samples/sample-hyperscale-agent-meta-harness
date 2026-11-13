//! ACP client: newline-delimited JSON-RPC 2.0 over the engine's stdio, the activity clock behind the stall watchdog
//! and the usage reader. The sequence a direct run sends (`initialize`, `session/new`, the model check, `session/prompt`) is
//! `crate::direct::driver`'s.

use harness_protocol::api::internal::{FaultHint, FaultTransport};
use harness_protocol::common::Usage;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AcpError {
    #[error("engine I/O: {0}")]
    Io(String),
    #[error("the engine closed its output")]
    Closed,
    /// A JSON-RPC error answer. `data` is the error's `data` member as the engine sent it;
    /// the shapes each engine sends are in `crate::engine::faults`. The display text
    /// leaves it out, so the messages the old classifier reads are unchanged.
    #[error("engine error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("engine did not take model {wanted} (currentValue {got:?})")]
    ModelMismatch { wanted: String, got: Option<String> },
    #[error("the engine did not answer session/cancel within the grace")]
    CancelIgnored,
    /// A tool call was running and the engine sent no line at all for the tool-idle limit ([`Activity::set_tool_idle`]); agentd wrote
    /// `session/cancel` and gave the prompt up. The engine may be alive but it does not answer, so this reads as an engine that must be started again.
    #[error(
        "a tool call made no progress for {silent_s} s; agentd cancelled the prompt and stopped the engine"
    )]
    ToolStuck { silent_s: u32 },
}

impl AcpError {
    /// What this failure tells the run's reader about a provider fault.
    /// Only a failure on the engine transport carries one: the
    /// engine's JSON-RPC internal error (-32603), an I/O error on its pipes, or its output closing.
    /// Every other error (auth, invalid params, a model that was not taken, a protocol mismatch)
    /// keeps the ordinary failed-turn path. Of the error data only short facts are copied: an
    /// HTTP status (100 to 599), an error name of at most 64 safe characters, and a
    /// retry-after; the hint carries none of the engine's message text. (The error string is
    /// separate: an RPC error's text stays in it as `engine error <code>: <message>`, and for a
    /// failure on the pipes agentd adds the exit and one scrubbed stderr line, `engine_diagnostic`.)
    pub fn fault_hint(&self) -> Option<FaultHint> {
        let (transport, data, message) = match self {
            AcpError::Rpc {
                code: -32603,
                data,
                message,
            } => (
                FaultTransport::Rpc32603,
                data.as_ref(),
                Some(message.as_str()),
            ),
            AcpError::Io(_) => (FaultTransport::Io, None, None),
            AcpError::Closed => (FaultTransport::Closed, None, None),
            _ => return None,
        };
        let mut hint = FaultHint {
            transport: Some(transport),
            ..FaultHint::default()
        };
        let engine_timeout = message.is_some_and(crate::engine::faults::is_idle_timeout);
        let Some(data) = data.and_then(Value::as_object) else {
            if engine_timeout {
                hint.code = Some("stall".into());
            }
            return Some(hint);
        };
        let field = |names: &[&str]| names.iter().find_map(|k| data.get(*k));
        hint.http_status = field(&["statusCode", "status", "httpStatus", "http_status"])
            .and_then(number)
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| (100..=599).contains(n));
        hint.retry_after_s = field(&[
            "retryAfterS",
            "retryAfter",
            "retryAfterSeconds",
            "retry_after",
        ])
        .and_then(retry_after);
        if let Some(explicit) = field(&["awsErrorType"])
            .and_then(Value::as_str)
            .and_then(error_name)
        {
            hint.aws_error_type = Some(explicit.clone());
            hint.code = Some(explicit);
        } else if let Some(name) = field(&["errorName", "errorType", "__type", "name", "code"])
            .and_then(Value::as_str)
            .and_then(error_name)
        {
            // An AWS exception name ends in `Exception`; `APIError` and `UnknownError` are the
            // engines' own classes and are not offered as an AWS type.
            if name.ends_with("Exception") {
                hint.aws_error_type = Some(name.clone());
            }
            hint.code = Some(name);
        }
        // The engine's own idle timeout, in its words, is a stall the 900 s watchdog never sees;
        // a structured status or AWS type in the data stays final.
        if engine_timeout && hint.http_status.is_none() && hint.aws_error_type.is_none() {
            hint.code = Some("stall".into());
        }
        Some(hint)
    }
}

/// A JSON number or a string of digits, as a `u64`.
fn number(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Seconds to wait: a whole number, a decimal rounded up, or a string of either; saturating.
fn retry_after(v: &Value) -> Option<u32> {
    let secs = match v {
        Value::Number(n) => n
            .as_u64()
            .map(|u| u as f64)
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0)),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| *f >= 0.0),
        _ => None,
    }?;
    Some(secs.ceil().min(f64::from(u32::MAX)) as u32)
}

/// An error name safe to log and send: the part after the last `#`, before the first `:`, of at
/// most 64 letters, digits, `_`, `-` or `.`.
fn error_name(raw: &str) -> Option<String> {
    let name = raw.rsplit('#').next().unwrap_or(raw);
    let name = name.split(':').next().unwrap_or(name).trim();
    (!name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
    .then(|| name.to_string())
}

/// The stall budget when a turn names none: 900 s exceeds a 32K-token generation at
/// 40 tokens per second.
pub const DEFAULT_PROVIDER_SILENCE_S: u32 = 900;

/// How often the watchdog looks at the engine's silence.
pub const STALL_TICK: Duration = Duration::from_secs(5);

/// What agentd knows about the engine's recent activity. The ACP
/// client feeds it: every engine line resets the clock, running tool calls are counted while they are
/// outstanding, and the longest silence between two updates is kept.
/// The stall clock runs only after the first `session/prompt` (before that nothing has asked the
/// model, so silence is an engine hang the whole-turn timeout bounds) and only while no tool call
/// is running (a long test run is not the provider being silent). The clock is tokio's, so tests can pause it.
#[derive(Debug)]
pub struct Activity {
    state: Mutex<ActivityState>,
}

#[derive(Debug)]
struct ActivityState {
    armed: bool,
    last: Instant,
    /// Every tool call id the engine named, with the furthest status it reached.
    tools: BTreeMap<String, ToolState>,
    max_gap: Duration,
    /// The silence the most recent engine line ended (zero when a tool call was running).
    last_gap: Duration,
    /// The silence the line before the most recent one ended.
    prev_gap: Duration,
    /// agentd wrote `session/cancel` during the current prompt.
    cancel_sent: bool,
    /// The cancel was a module's interrupt (the module seam), not the user's: `cancel_sent` is also true.
    interrupted: bool,
    /// A person is deciding: the clock is stopped until [`Activity::release`].
    held: bool,
    /// The tool-idle limit: how long a running tool call may go with no line from the engine. `None` is no limit.
    tool_idle: Option<Duration>,
    /// Set by [`watch`] when the tool-idle limit ended the stage: the seconds of silence. Taken by the driver.
    tool_stuck: Option<u32>,
}

/// How far a tool call got. A status never goes back: an engine may send `in_progress` before the
/// `pending` announcement of the same call.
#[derive(Debug)]
struct ToolState {
    rank: u8,
    status: String,
    /// False once the prompt that made the call was answered.
    open: bool,
}

const TOOL_PENDING: u8 = 0;
const TOOL_RUNNING: u8 = 1;
const TOOL_DONE: u8 = 2;
/// A turn with more tool calls than this stops recording new ones.
const MAX_TRACKED_TOOLS: usize = 4096;

impl ActivityState {
    /// A tool is running, or a person is deciding: silence then is not the provider's. A call that
    /// is only `pending` (announced, its input still streaming or awaiting approval) is the model
    /// still generating, which the stall budget is sized for.
    fn outstanding(&self) -> bool {
        self.held
            || self
                .tools
                .values()
                .any(|t| t.open && t.rank == TOOL_RUNNING)
    }
}

impl Activity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ActivityState {
                armed: false,
                last: Instant::now(),
                tools: BTreeMap::new(),
                max_gap: Duration::ZERO,
                last_gap: Duration::ZERO,
                prev_gap: Duration::ZERO,
                cancel_sent: false,
                interrupted: false,
                held: false,
                tool_idle: None,
                tool_stuck: None,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, ActivityState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Sets the tool-idle limit: while a tool call is running (`in_progress`) and the engine sends no line at all for this long, [`watch`] ends
    /// the stage. The provider-silence clock does not run while a tool call runs (a long test run is not the provider being silent), so
    /// this is the only bound on an engine whose tool call never returns. `None` (the default) leaves the stage to the run's deadline, as it always was.
    pub fn set_tool_idle(&self, limit: Option<Duration>) {
        self.lock().tool_idle = limit;
    }

    /// The silence so far when the tool-idle limit is set and has been reached: a prompt is open, no person is deciding, a tool call is
    /// running and the engine sent nothing for the limit.
    pub fn tool_idle_exceeded(&self) -> Option<Duration> {
        let s = self.lock();
        let limit = s.tool_idle?;
        if !s.armed || s.held || !s.tools.values().any(|t| t.open && t.rank == TOOL_RUNNING) {
            return None;
        }
        let silent = Instant::now().saturating_duration_since(s.last);
        (silent >= limit).then_some(silent)
    }

    /// [`watch`] ended the stage on the tool-idle limit after `silent`.
    pub fn mark_tool_stuck(&self, silent: Duration) {
        self.lock().tool_stuck = Some(u32::try_from(silent.as_secs()).unwrap_or(u32::MAX));
    }

    /// Takes the mark of [`mark_tool_stuck`](Self::mark_tool_stuck): the seconds of silence, once.
    pub fn take_tool_stuck(&self) -> Option<u32> {
        self.lock().tool_stuck.take()
    }

    /// A `session/prompt` is being sent: the stall clock starts (a nudge restarts it).
    pub fn prompt_sent(&self) {
        let mut s = self.lock();
        s.armed = true;
        s.cancel_sent = false;
        s.interrupted = false;
        s.tool_stuck = None;
        s.last = Instant::now();
    }

    /// agentd wrote to the engine: silence is counted from here.
    pub fn touch(&self) {
        self.lock().last = Instant::now();
    }

    /// The engine sent a line. The gap since the previous one counts toward the longest silence
    /// unless a tool call was running.
    pub fn line_read(&self) {
        let mut s = self.lock();
        let now = Instant::now();
        s.prev_gap = s.last_gap;
        s.last_gap = Duration::ZERO;
        if s.armed && !s.outstanding() {
            let gap = now.saturating_duration_since(s.last);
            s.max_gap = s.max_gap.max(gap);
            s.last_gap = gap;
        }
        s.last = now;
    }

    /// The silence the most recent engine line ended: for an error the engine sent after its own
    /// timeout, how long the engine had been silent.
    pub fn last_gap(&self) -> Duration {
        self.lock().last_gap
    }

    /// How long the engine was silent before an error it sent after its own timeout: the longer of
    /// the silence the error line ended and the silence the line before it ended, because both real
    /// engines send a notification about a millisecond before the timeout error (a
    /// `session_info_update` at 302.9342 s, the error at 302.9351 s).
    pub fn error_gap(&self) -> Duration {
        let s = self.lock();
        s.last_gap.max(s.prev_gap)
    }

    /// agentd wrote `session/cancel` for the current prompt.
    pub fn cancel_written(&self) {
        self.lock().cancel_sent = true;
    }

    /// Whether agentd cancelled the current prompt: an engine that ends a prompt `cancelled`
    /// without it did so by itself.
    pub fn cancel_sent(&self) -> bool {
        self.lock().cancel_sent
    }

    /// agentd wrote `session/cancel` because a module interrupted the prompt ([`Interrupt`]). The cancel counts as agentd's own
    /// (`cancel_sent` is true, so the engine's `cancelled` answer is the one asked for and no nudge follows).
    pub fn interrupt_written(&self) {
        let mut s = self.lock();
        s.cancel_sent = true;
        s.interrupted = true;
    }

    /// Whether the current (or just ended) prompt was cancelled by a module's interrupt. Cleared by the next prompt and by
    /// [`reset_for_stage`](Self::reset_for_stage).
    pub fn interrupted(&self) -> bool {
        self.lock().interrupted
    }

    /// Tracks tool calls from a `session/update` by id. Only a call that reached `in_progress` holds
    /// the stall clock open; `pending` (and no status, which ACP reads as pending) does not, and a
    /// status never goes back, so `completed` and `failed` close a call whatever order the updates
    /// arrive in. An update with no status changes nothing.
    pub fn observe_update(&self, update: &Value) {
        if !matches!(
            update.get("sessionUpdate").and_then(Value::as_str),
            Some("tool_call" | "tool_call_update")
        ) {
            return;
        }
        let id = match update.get("toolCallId") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => return,
        };
        let status = update.get("status").and_then(Value::as_str);
        let mut s = self.lock();
        if s.tools.len() >= MAX_TRACKED_TOOLS && !s.tools.contains_key(&id) {
            return;
        }
        let tool = s.tools.entry(id).or_insert_with(|| ToolState {
            rank: TOOL_PENDING,
            status: "pending".into(),
            open: true,
        });
        if let Some(status) = status {
            let rank = match status {
                "in_progress" => TOOL_RUNNING,
                "completed" | "failed" => TOOL_DONE,
                _ => TOOL_PENDING,
            };
            if rank >= tool.rank {
                tool.rank = rank;
                tool.status = status.to_string();
            }
        }
        if !s.outstanding() {
            s.last = Instant::now();
        }
    }

    /// A `session/prompt` was answered: any tool call it left running is closed (its record stays).
    pub fn prompt_ended(&self) {
        let mut s = self.lock();
        for t in s.tools.values_mut() {
            t.open = false;
        }
        s.last = Instant::now();
    }

    /// The furthest status any of the tool calls `ids` reached: `completed`, `failed`,
    /// `in_progress`, `pending`, or `absent` when the engine never named any of them. The record
    /// outlives the prompt, so the turn's end can read it.
    pub fn tool_result(&self, ids: &[String]) -> &'static str {
        let s = self.lock();
        let seen: Vec<&ToolState> = ids.iter().filter_map(|i| s.tools.get(i)).collect();
        if seen.iter().any(|t| t.status == "completed") {
            "completed"
        } else if seen.iter().any(|t| t.rank == TOOL_DONE) {
            "failed"
        } else if seen.iter().any(|t| t.rank == TOOL_RUNNING) {
            "in_progress"
        } else if seen.is_empty() {
            "absent"
        } else {
            "pending"
        }
    }

    /// How long the engine has been silent while nothing else explains it: zero before the first
    /// prompt, while a tool call is running and while a person is deciding ([`hold`](Self::hold)).
    pub fn silent_for(&self) -> Duration {
        let s = self.lock();
        if !s.armed || s.outstanding() {
            return Duration::ZERO;
        }
        Instant::now().saturating_duration_since(s.last)
    }

    /// The longest silence between two engine updates, the one still open included.
    pub fn max_silent_gap(&self) -> Duration {
        let open = self.silent_for();
        self.lock().max_gap.max(open)
    }

    /// [`max_silent_gap`](Self::max_silent_gap) in whole seconds; `None` before the first prompt.
    pub fn max_silent_gap_s(&self) -> Option<u32> {
        let armed = self.lock().armed;
        armed.then(|| u32::try_from(self.max_silent_gap().as_secs()).unwrap_or(u32::MAX))
    }

    /// Tool calls that are running (`in_progress`) now.
    pub fn tools_in_flight(&self) -> usize {
        self.lock()
            .tools
            .values()
            .filter(|t| t.open && t.rank == TOOL_RUNNING)
            .count()
    }

    /// A person is deciding: the silence clock stops. [`silent_for`](Self::silent_for) is zero until
    /// [`release`](Self::release), and neither the wait nor a line the engine sends during it counts toward
    /// [`max_silent_gap`](Self::max_silent_gap): the human's wait is never the provider's silence.
    pub fn hold(&self) {
        self.lock().held = true;
    }

    /// The person has decided (or the window closed): the silence clock restarts from now.
    pub fn release(&self) {
        let mut s = self.lock();
        s.held = false;
        s.last = Instant::now();
    }

    /// A new stage of the same run begins (a user's prompt, a fresh session, a model switch). Clears everything one stage must
    /// not hand to the next: the tool table (a call a stalled or dropped prompt left `in_progress` would otherwise switch the
    /// watchdog off for the whole run), `cancel_sent`, the hold, and the last two gaps (`error_gap` must not read the previous
    /// stage's silence). The prompt stays armed, the clock restarts from now, and the longest silence seen so far is kept: the
    /// driver reads [`max_silent_gap`](Self::max_silent_gap) at the end of each stage and keeps the largest.
    pub fn reset_for_stage(&self) {
        let mut s = self.lock();
        s.tools.clear();
        s.cancel_sent = false;
        s.interrupted = false;
        s.held = false;
        s.last_gap = Duration::ZERO;
        s.prev_gap = Duration::ZERO;
        s.last = Instant::now();
    }
}

/// The bounds [`watch`] enforces on one turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchLimits {
    /// The whole-turn timeout (`TurnSpec.timeoutS`).
    pub timeout: Duration,
    /// Engine silence that ends the turn as a stall (`TurnSpec.providerSilenceS`).
    pub silence: Duration,
    /// How often silence is checked.
    pub tick: Duration,
}

impl WatchLimits {
    /// The ticker is [`STALL_TICK`], or a quarter of a short budget, never below 100 ms, so a
    /// stall is noticed within one tick of its budget.
    pub fn new(timeout: Duration, silence: Duration) -> Self {
        Self {
            timeout,
            silence,
            tick: STALL_TICK.min(silence / 4).max(Duration::from_millis(100)),
        }
    }
}

/// How a watched turn ended.
#[derive(Debug)]
pub enum Watched<T> {
    /// The turn future finished.
    Done(T),
    /// The whole-turn timeout came first (the existing `timed_out` outcome).
    TimedOut,
    /// The engine was silent for `silent`, past the stall budget.
    Stalled { silent: Duration },
}

/// Runs `fut` under the turn timeout and the stall watchdog: a select of the turn, the timeout
/// and a ticker that reads `activity.silent_for()`. A finished turn wins over a due tick, and the
/// timeout over a stall.
pub async fn watch<F: Future>(
    fut: F,
    limits: WatchLimits,
    activity: &Activity,
) -> Watched<F::Output> {
    tokio::pin!(fut);
    let deadline = tokio::time::sleep(limits.timeout);
    tokio::pin!(deadline);
    let tick = limits.tick.max(Duration::from_millis(1));
    let mut ticker = tokio::time::interval_at(Instant::now() + tick, tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            out = &mut fut => return Watched::Done(out),
            () = &mut deadline => return Watched::TimedOut,
            _ = ticker.tick() => {
                let silent = activity.silent_for();
                if silent >= limits.silence {
                    return Watched::Stalled { silent };
                }
                // A running tool call and no line for the tool-idle limit (never set for an engine that has none).
                if let Some(idle) = activity.tool_idle_exceeded() {
                    activity.mark_tool_stuck(idle);
                    return Watched::Stalled { silent: idle };
                }
            }
        }
    }
}

/// Receives what the engine sends that is not the awaited response.
pub trait AcpHandler: Send {
    fn on_notification(&mut self, method: &str, params: &Value);
    /// The result for an engine-to-client request, or a JSON-RPC error `(code, message)`.
    fn on_request(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)>;
}

/// Which way a JSON-RPC message of the connection travelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Written by agentd to the engine: requests, notifications, and the answers to the engine's own requests.
    ToEngine,
    /// Read by agentd from the engine: responses, notifications and requests.
    FromEngine,
}

/// Sees every JSON-RPC message of the connection, in both directions, without being able to change it (the module seam,
/// `crate::direct::hooks`). It is called on the read and write path of the protocol, so an implementation copies and queues and never
/// waits. An outgoing message is seen after it was written and flushed (a write that fails is the run's error, not a message sent); an
/// incoming one is seen when its line has been read and parsed, before anything is done with it, and `stale` says it belongs to a
/// session the run abandoned. A stdout line that is not JSON-RPC is not a message and is not seen.
pub trait WireTap: Send + Sync {
    fn wire(&self, dir: Dir, msg: &Value, stale: bool);
}

/// The way a module cancels the `session/prompt` that is open now (the module seam, `DriverCtl::interrupt`). One handle is shared by the
/// module and the client ([`AcpClient::with_interrupt`]). It is not the run's cancel: that is a latch that is never reset (`ham stop`
/// ends the run), while an interrupt cancels one prompt and the run goes on. The client writes `session/cancel` once for the prompt (the
/// user's cancel, when it is up, comes first and is the only one), waits for the prompt's own answer for the grace and returns it. A
/// request after the interrupt starts clean.
#[derive(Clone, Debug)]
pub struct Interrupt {
    tx: Arc<watch::Sender<u64>>,
    open: Arc<AtomicBool>,
}

impl Default for Interrupt {
    fn default() -> Self {
        Self::new()
    }
}

impl Interrupt {
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(0).0),
            open: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Asks the client to cancel the prompt that is open now. `false` when none is open: the request was not made, nothing is queued, and a
    /// later prompt is not affected. `true` means the client was told; the prompt's answer (or its grace running out) is what the driver
    /// then reports at the end of the stage.
    pub fn interrupt(&self) -> bool {
        if !self.open.load(Ordering::SeqCst) {
            return false;
        }
        self.tx.send_modify(|n| *n = n.wrapping_add(1));
        true
    }

    /// Whether a `session/prompt` request is open: from just before it is written until its answer arrives or the request ends.
    pub fn prompt_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }
}

/// Marks the prompt open for as long as the request lives, whichever way it ends.
struct OpenGuard(Arc<AtomicBool>);

impl OpenGuard {
    fn open(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self(flag.clone())
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// What an engine needs done to the text of a prompt before it goes on the wire. Some engines read a prompt that starts with
/// `/` as one of their own commands, so a user's task that begins with a command word would run as that command. The guard returns the text
/// as it was when the text is safe, and the engine's own marker line before it when it is not. An engine with no guard gets the text byte for byte.
pub type PromptGuard = for<'a> fn(&'a str) -> std::borrow::Cow<'a, str>;

pub struct AcpClient<R, W> {
    lines: Lines<BufReader<R>>,
    writer: W,
    next_id: i64,
    /// Cancel signal for `session/prompt` and how long the engine gets to answer `cancelled`.
    cancel: Option<(watch::Receiver<bool>, Duration)>,
    /// What the stall watchdog reads; absent in tests that do not watch.
    activity: Option<Arc<Activity>>,
    /// The ACP session this client's traffic belongs to, once named: lines tagged with another session id come from a
    /// session the run abandoned. None: nothing is filtered.
    session: Option<String>,
    /// Lines of other sessions dropped so far.
    stale: u64,
    /// The module seam's view of the wire; absent for a run with no module.
    tap: Option<Arc<dyn WireTap>>,
    /// The module seam's interrupt of an open prompt and the grace the engine gets to answer the cancel; absent for a run with no module.
    interrupt: Option<(Interrupt, Duration)>,
    /// The guard of the engine's prompts ([`PromptGuard`]); none for an engine that reads no prompt as a command.
    prompt_guard: Option<PromptGuard>,
}

/// What a prompt wait woke up for.
enum Wake {
    Line(std::io::Result<Option<String>>),
    Cancel,
    SignalGone,
    Interrupt,
}

impl<R, W> std::fmt::Debug for AcpClient<R, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpClient")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl<R: AsyncRead + Unpin + Send, W: AsyncWrite + Unpin + Send> AcpClient<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            lines: BufReader::new(reader).lines(),
            writer,
            next_id: 1,
            cancel: None,
            activity: None,
            session: None,
            stale: 0,
            tap: None,
            interrupt: None,
            prompt_guard: None,
        }
    }

    /// Sets the guard every prompt text passes before it is written ([`PromptGuard`]). `None` leaves the text as it is.
    pub fn with_prompt_guard(mut self, guard: Option<PromptGuard>) -> Self {
        self.prompt_guard = guard;
        self
    }

    /// The text of a prompt as it goes on the wire.
    pub fn wire_text<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        match self.prompt_guard {
            Some(guard) => guard(text),
            None => std::borrow::Cow::Borrowed(text),
        }
    }

    /// Lets a module interrupt the open `session/prompt` ([`Interrupt`]); the engine gets `grace` to answer the cancel it writes.
    pub fn with_interrupt(mut self, interrupt: Interrupt, grace: Duration) -> Self {
        self.interrupt = Some((interrupt, grace));
        self
    }

    /// Shows every message of the connection to `tap` ([`WireTap`]).
    pub fn with_tap(mut self, tap: Arc<dyn WireTap>) -> Self {
        self.tap = Some(tap);
        self
    }

    /// Feeds `activity` from this connection: every engine line, every tool call and every prompt written.
    pub fn with_activity(mut self, activity: Arc<Activity>) -> Self {
        self.activity = Some(activity);
        self
    }

    /// Names the ACP session this client now serves. An abandoned session keeps
    /// running inside the same engine process until it is cancelled: its notifications keep arriving tagged with its old session
    /// id and its late `session/prompt` response with its old request id. Once a session is named, a notification or an
    /// engine request tagged with another session id is dropped (a request is refused), and it does not count as engine activity
    /// for the stall watchdog. A line with no session id passes. A late response is told apart by its request id.
    pub fn set_session(&mut self, id: &str) {
        self.session = Some(id.to_string());
    }

    /// Lines of other sessions dropped so far (for the log).
    pub fn stale_lines(&self) -> u64 {
        self.stale
    }

    /// While a `session/prompt` is pending, `true` on `signal` sends `session/cancel` (once);
    /// the engine then has `grace` to answer the prompt, else the request fails `CancelIgnored`.
    pub fn with_cancel(mut self, signal: watch::Receiver<bool>, grace: Duration) -> Self {
        self.cancel = Some((signal, grace));
        self
    }

    /// Whether a message's params name a session other than the current one (never when no session is named).
    fn is_stale(&self, params: Option<&Value>) -> bool {
        let (Some(current), Some(params)) = (self.session.as_deref(), params) else {
            return false;
        };
        params
            .get("sessionId")
            .and_then(Value::as_str)
            .is_some_and(|sid| sid != current)
    }

    /// Swaps the engine's pipes for those of a restarted engine process. The request
    /// counter, the cancel signal and the activity stay as they are; a line half-read from the old pipe is dropped with it.
    pub fn replace_io(&mut self, reader: R, writer: W) {
        self.lines = BufReader::new(reader).lines();
        self.writer = writer;
    }

    async fn write(&mut self, v: &Value) -> Result<(), AcpError> {
        self.write_line(v, true).await
    }

    /// `touch`: whether this write counts as agentd's own activity for the silence clock (a refusal sent to an abandoned
    /// session's request does not).
    async fn write_line(&mut self, v: &Value, touch: bool) -> Result<(), AcpError> {
        let mut line = v.to_string();
        line.push('\n');
        let io = |e: std::io::Error| {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                AcpError::Closed
            } else {
                AcpError::Io(e.to_string())
            }
        };
        if touch && let Some(a) = &self.activity {
            a.touch();
        }
        self.writer.write_all(line.as_bytes()).await.map_err(io)?;
        self.writer.flush().await.map_err(io)?;
        if let Some(t) = &self.tap {
            t.wire(Dir::ToEngine, v, false);
        }
        Ok(())
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), AcpError> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// Reads and serves ONE engine line while no request is pending: the pipe stays drained while a person decides, so a late
    /// notification can never block the engine. A notification goes to the handler (and to the activity, like a line during a
    /// request); an engine-to-client request is answered by the handler; a response nobody awaits and a line that is not
    /// JSON-RPC are dropped. `Err(Closed)` is the engine's output closing, `Err(Io)` a read failure. Cancel-safe while it waits
    /// for a line: dropping the future (a `select!` branch that lost, a timeout) loses no data, because the buffered line reader
    /// keeps a partial line. Once a line is read its dispatch runs to the end of this call.
    pub async fn pump_idle(&mut self, handler: &mut dyn AcpHandler) -> Result<(), AcpError> {
        let line = self
            .lines
            .next_line()
            .await
            .map_err(|e| AcpError::Io(e.to_string()))?
            .ok_or(AcpError::Closed)?;
        let msg = serde_json::from_str::<Value>(line.trim()).ok();
        let stale = msg.as_ref().is_some_and(|m| self.is_stale(m.get("params")));
        if !stale && let Some(a) = &self.activity {
            a.line_read();
        }
        if let (Some(t), Some(m)) = (&self.tap, &msg) {
            t.wire(Dir::FromEngine, m, stale);
        }
        let Some(msg) = msg else {
            return Ok(()); // engine chatter on stdout that is not JSON-RPC
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (
            msg.get("id").cloned(),
            msg.get("method").and_then(Value::as_str),
        ) {
            (id, Some(method)) => {
                self.serve_engine_message(id, method, &params, stale, handler)
                    .await
            }
            // A response nobody awaits, or an object with neither a method nor an id.
            (_, None) => Ok(()),
        }
    }

    /// Serves one message the engine sent on its own: answers a request through the handler, or passes a notification on (a
    /// `session/update` also reaches the activity). Shared by [`request`](Self::request) and [`pump_idle`](Self::pump_idle).
    /// A `stale` message (tagged with a session other than the current one) is dropped; a request among them is refused so the
    /// abandoned session is not left waiting, and that write is not agentd activity.
    async fn serve_engine_message(
        &mut self,
        id: Option<Value>,
        method: &str,
        params: &Value,
        stale: bool,
        handler: &mut dyn AcpHandler,
    ) -> Result<(), AcpError> {
        if stale {
            self.stale += 1;
            if let Some(rid) = id {
                let refusal = json!({ "jsonrpc": "2.0", "id": rid, "error": { "code": -32000, "message": "the session was abandoned by the client" } });
                return self.write_line(&refusal, false).await;
            }
            return Ok(());
        }
        match id {
            Some(rid) => {
                let reply = match handler.on_request(method, params) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": rid, "result": result }),
                    Err((code, message)) => {
                        json!({ "jsonrpc": "2.0", "id": rid, "error": { "code": code, "message": message } })
                    }
                };
                self.write(&reply).await
            }
            None => {
                if method == "session/update"
                    && let (Some(a), Some(update)) = (&self.activity, params.get("update"))
                {
                    a.observe_update(update);
                }
                handler.on_notification(method, params);
                Ok(())
            }
        }
    }

    /// Sends a request and serves engine messages until its response arrives.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        handler: &mut dyn AcpHandler,
    ) -> Result<Value, AcpError> {
        let id = self.next_id;
        self.next_id += 1;
        if method == "session/prompt"
            && let Some(a) = &self.activity
        {
            a.prompt_sent();
        }
        // A module may interrupt this prompt from just before it is written until its answer arrives; what was asked before is not
        // this prompt's business (a new receiver starts at the value now).
        let (mut intr, _open) = match (&self.interrupt, method == "session/prompt") {
            (Some((i, grace)), true) => (
                Some((i.tx.subscribe(), *grace)),
                Some(OpenGuard::open(&i.open)),
            ),
            _ => (None, None),
        };
        let mut interrupt_asked = false;
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        let session = (method == "session/prompt")
            .then(|| {
                params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();
        let mut cancel = session.as_ref().and(self.cancel.clone());
        let mut deadline: Option<Instant> = None;
        loop {
            if let (Some((signal, grace)), None, Some(sid)) = (&cancel, deadline, &session)
                && *signal.borrow()
            {
                self.notify("session/cancel", json!({ "sessionId": sid }))
                    .await?;
                if let Some(a) = &self.activity {
                    a.cancel_written();
                }
                deadline = Some(Instant::now() + *grace);
            }
            // A module's interrupt cancels the prompt once; the user's cancel, when it is up, was written above and is the only one.
            if deadline.is_none()
                && let (Some((rx, grace)), Some(sid)) = (intr.as_mut(), &session)
                && (std::mem::take(&mut interrupt_asked) || rx.has_changed().unwrap_or(false))
            {
                rx.borrow_and_update();
                self.notify("session/cancel", json!({ "sessionId": sid }))
                    .await?;
                if let Some(a) = &self.activity {
                    a.interrupt_written();
                }
                deadline = Some(Instant::now() + *grace);
            }
            let wake = match (&mut cancel, &mut intr, deadline) {
                (_, _, Some(d)) => match tokio::time::timeout_at(d, self.lines.next_line()).await {
                    Ok(r) => Wake::Line(r),
                    Err(_) => return Err(AcpError::CancelIgnored),
                },
                (Some((signal, _)), None, None) => tokio::select! {
                    r = self.lines.next_line() => Wake::Line(r),
                    c = signal.changed() => if c.is_ok() { Wake::Cancel } else { Wake::SignalGone },
                },
                (None, None, None) => Wake::Line(self.lines.next_line().await),
                (c, Some((rx, _)), None) => {
                    let cancel_changed = async {
                        match c {
                            Some((signal, _)) => signal.changed().await,
                            None => {
                                std::future::pending::<Result<(), watch::error::RecvError>>().await
                            }
                        }
                    };
                    tokio::select! {
                        r = self.lines.next_line() => Wake::Line(r),
                        c = cancel_changed => if c.is_ok() { Wake::Cancel } else { Wake::SignalGone },
                        // The client holds the sender (`self.interrupt`), so the channel cannot close while this request runs; and once an
                        // interrupt is acted on the deadline is set and this branch is not polled again.
                        _ = rx.changed() => Wake::Interrupt,
                    }
                }
            };
            let line = match wake {
                Wake::Cancel => continue,
                Wake::SignalGone => {
                    cancel = None;
                    continue;
                }
                Wake::Interrupt => {
                    interrupt_asked = true;
                    continue;
                }
                Wake::Line(r) => r
                    .map_err(|e| AcpError::Io(e.to_string()))?
                    .ok_or(AcpError::Closed)?,
            };
            let msg = serde_json::from_str::<Value>(line.trim()).ok();
            // A line of an abandoned session is not activity of the current one.
            let stale = msg.as_ref().is_some_and(|m| self.is_stale(m.get("params")));
            if !stale && let Some(a) = &self.activity {
                a.line_read();
            }
            if let (Some(t), Some(m)) = (&self.tap, &msg) {
                t.wire(Dir::FromEngine, m, stale);
            }
            let Some(msg) = msg else {
                continue; // engine chatter on stdout that is not JSON-RPC
            };
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            match (
                msg.get("id").cloned(),
                msg.get("method")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ) {
                (Some(mid), None) => {
                    if mid.as_i64() != Some(id) {
                        continue;
                    }
                    if session.is_some()
                        && let Some(a) = &self.activity
                    {
                        a.prompt_ended();
                    }
                    if let Some(err) = msg.get("error") {
                        return Err(AcpError::Rpc {
                            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                            message: err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            data: err.get("data").filter(|d| !d.is_null()).cloned(),
                        });
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
                (Some(rid), Some(m)) => {
                    self.serve_engine_message(Some(rid), &m, &params, stale, handler)
                        .await?;
                }
                (None, Some(m)) => {
                    self.serve_engine_message(None, &m, &params, stale, handler)
                        .await?;
                }
                (None, None) => {}
            }
        }
    }
}

/// The usage an engine reported in a response or an update (Converse names, snake_case, or the ACP `Usage` names).
pub fn read_usage(v: &Value) -> Option<Usage> {
    let u = v
        .pointer("/_meta/usage")
        .or_else(|| v.get("usage"))
        .filter(|u| u.is_object())?;
    // Converse names, their snake_case form, and the ACP `Usage` names (`cachedReadTokens`,
    // `cachedWriteTokens`: 1.18.32 and fork patch 0008). Input excludes cache tokens.
    let n = |names: &[&str]| {
        names
            .iter()
            .find_map(|k| u.get(*k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Some(Usage {
        input_tokens: n(&["inputTokens", "input_tokens"]),
        output_tokens: n(&["outputTokens", "output_tokens"]),
        cache_read_input_tokens: n(&[
            "cacheReadInputTokens",
            "cache_read_input_tokens",
            "cachedReadTokens",
        ]),
        cache_write_input_tokens: n(&[
            "cacheWriteInputTokens",
            "cache_write_input_tokens",
            "cachedWriteTokens",
        ]),
    })
}
