//! The first slice of provider failover: the one `session/prompt` of a direct run ends in a JSON-RPC error, and
//! agentd sends ONE fixed sentence on the same engine session ([`RESUME_PROMPT`]), at most a few times, with a wait that doubles.
//! The engine keeps its conversation and the work tree stays as it is, so the model picks up where the failed request left off.
//!
//! Why on the same session, with no restart: the failures this slice exists for end the prompt, not the engine. Measured on the
//! 3.0.71 binary against a fake Bedrock: a prompt that ended
//! `-32603` leaves the session usable (the next `session/prompt` runs and keeps the history), and a helper agent (the engine's helper-agent tool)
//! whose request timed out turns the main agent's otherwise finished prompt into the same `-32603 The operation timed out.`.
//! A dead engine (a closed pipe) and a silent one (the 900 s watchdog) need a restart; that is the second slice.
//!
//! What this module decides: which failures are worth another try ([`classify_failure`]), how many ([`RetryPolicy::allowed`]) and
//! how long to wait ([`RetryPolicy::pause_before`]). The loop that applies it is [`prompt_with_retry`]; it records every try
//! ([`DirectAttempt`]), writes a `turn.progress` event and a log line for each retry, and stops at once for the user's cancel.
//! It never reads the work: whether the work is done is not its question.

use crate::acp::{AcpClient, AcpError, Activity};
use crate::direct::classify::{PromptDone, scrub_detail};
use crate::direct::collector::DirectCollector;
use crate::direct::disk::{self, DiskCheck, Limits, Reading};
use crate::direct::driver::{PromptPlan, send_prompt};
use crate::direct::engine_stop::{StopReport, Verdict};
use crate::direct::failover::{
    FRUITLESS_PROMPTS_MAX, FailoverRecord, Ladder, LadderPolicy, Next, ProbeAnswer, Route,
};
use crate::direct::local_fault::{self, Tally};
use crate::engine::config::acp_model_value;
use crate::engine::faults::is_idle_timeout;
use crate::logging::LogSink;
use crate::time::{format_rfc3339, now_unix};
use harness_protocol::common::Usage;
use harness_protocol::direct::{
    DirectAttempt, DirectAttemptKind, DirectAttemptResult, DirectSwitch, MAX_DIRECT_SWITCHES,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

/// The sentence sent after a provider failure. One fixed line: it says what happened and asks the model to look at the tree and
/// go on. It names no marker and does not ask for a verdict.
pub const RESUME_PROMPT: &str = "A previous attempt on this work tree was interrupted by a provider failure. Inspect the work tree and continue the task.";
/// The sentence sent after the engine ended the prompt `cancelled` with no cancel from agentd (its own brake: a loop guard or the mistake
/// limit): the cause is not known to be the provider, so it is not named.
pub const RESUME_PROMPT_INTERRUPTED: &str = "A previous attempt on this work tree was interrupted. Inspect the work tree and continue the task.";

/// The most retries a setting may ask for (`AGENTD_DIRECT_PROVIDER_RETRIES`): a mistake cannot loop for hours.
pub const MAX_CONFIGURED_RETRIES: u32 = 6;
/// A wait is never longer than this, whatever the provider asked for.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// What kind of failure ended a try, which decides how many more tries it gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryClass {
    /// The provider or the network failed in a way that passes: a timeout, a throttle, a 5xx, a cut stream.
    Transient,
    /// The provider refused the request or the shape is unknown. The same request would fail again, but the failure may be a helper
    /// agent's while the main agent finished, so one more try is worth its small cost.
    Permanent,
    /// The provider refuses the route itself: the model is not found, or access to it is denied. The same route cannot work, so
    /// there is no resume on it, and with failover the route is out for the session.
    Unavailable,
    /// Another try cannot help: the provider's filter blocked the answer, the context is full, the output limit was hit, or the
    /// failure is not a provider failure this slice handles.
    Never,
    /// The conversation the engine replays is what the provider refuses, not the route: a 400 that names a message of the history
    /// (`messages.7.content.0.toolUse.input is invalid`), or a context that no longer fits. Another try, another route and another model
    /// would send the same history; the remedy is a fresh session from a digest. No
    /// resume, no switch, no exclusion: agentd ends the run and raises the `notify` event (`reason: history_unusable`) for the owner of that
    /// remedy.
    HistoryUnusable,
    /// The engine ended the prompt `cancelled` itself (nobody asked): its own brake (the loop guard, the mistake limit), not a provider
    /// failure. It has its own resume count ([`PromptPlan::engine_stop_resumes`]), is never answered with a switch of route or model, and
    /// never reaches the ladder.
    EngineStop,
}

impl RetryClass {
    /// The word `summary.json` and the progress event carry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
            Self::Unavailable => "unavailable",
            Self::Never => "never",
            Self::EngineStop => "engine_stop",
            Self::HistoryUnusable => "history_unusable",
        }
    }
}

/// How the failure of the prompt is retried.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    max_transient: u32,
    max_permanent: u32,
    base_pause: Duration,
    jitter_pct: u32,
    /// One line per retry for the runtime log (CloudWatch).
    log: Option<Arc<dyn LogSink>>,
    /// The measurement of the session's two disks; `None` measures nothing.
    disk: Option<DiskCheck>,
}

impl RetryPolicy {
    /// No retry: one prompt, as before this slice.
    pub fn off() -> Self {
        Self::new(0, Duration::from_secs(5))
    }

    /// `max_retries` tries for a transient failure and at most one for a permanent one, waits from `base_pause`, no jitter.
    pub fn new(max_retries: u32, base_pause: Duration) -> Self {
        Self {
            max_transient: max_retries,
            max_permanent: max_retries.min(1),
            base_pause,
            jitter_pct: 0,
            log: None,
            disk: None,
        }
    }

    /// The policy of a real run: the waits are spread by up to a fifth so runs that failed together do not retry together.
    pub fn standard(max_retries: u32, base_pause: Duration) -> Self {
        Self {
            jitter_pct: 20,
            ..Self::new(max_retries.min(MAX_CONFIGURED_RETRIES), base_pause)
        }
    }

    /// The runtime log for the retry lines.
    pub fn with_log(mut self, log: Arc<dyn LogSink>) -> Self {
        self.log = Some(log);
        self
    }

    /// The measurement of the session's two disks: at each prompt boundary (one notice per crossing of an early mark) and, fresh,
    /// after a prompt failed with error -32603 (a short disk stops the call at once, [`prompt_with_retry_from`]).
    pub fn with_disk(mut self, check: DiskCheck) -> Self {
        self.disk = Some(check);
        self
    }

    /// How many retries a failure of this class gets.
    pub fn allowed(&self, class: RetryClass) -> u32 {
        match class {
            RetryClass::Transient => self.max_transient,
            RetryClass::Permanent => self.max_permanent,
            RetryClass::Unavailable
            | RetryClass::Never
            | RetryClass::EngineStop
            | RetryClass::HistoryUnusable => 0,
        }
    }

    /// The wait before retry number `retry_no` (1 for the first): the base, doubled each time, at most 60 s; a `retryAfterS` in the
    /// error raises it (never lowers it) up to [`MAX_RETRY_AFTER`].
    pub fn pause_before(&self, retry_no: u32, err: &AcpError) -> Duration {
        let doubled = self
            .base_pause
            .saturating_mul(2u32.saturating_pow(retry_no.saturating_sub(1)))
            .min(Duration::from_secs(60).max(self.base_pause));
        match retry_after(err) {
            Some(asked) => doubled.max(asked.min(MAX_RETRY_AFTER)),
            None => doubled,
        }
    }

    /// `d` plus up to the jitter share of itself; `d` when the policy has none.
    pub fn jittered(&self, d: Duration) -> Duration {
        if self.jitter_pct == 0 || d.is_zero() {
            return d;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |t| u64::from(t.subsec_nanos()));
        let extra_permille = nanos % (u64::from(self.jitter_pct) * 10 + 1);
        let d_nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX / 1000);
        d + Duration::from_nanos(d_nanos.saturating_mul(extra_permille) / 1000)
    }
}

/// The seconds the provider asked to wait (`data.retryAfterS` and its spellings), when it is a number.
pub fn retry_after(err: &AcpError) -> Option<Duration> {
    let AcpError::Rpc {
        data: Some(Value::Object(data)),
        ..
    } = err
    else {
        return None;
    };
    [
        "retryAfterS",
        "retryAfter",
        "retryAfterSeconds",
        "retry_after",
    ]
    .iter()
    .find_map(|k| data.get(*k))
    .and_then(Value::as_f64)
    .filter(|s| s.is_finite() && *s >= 0.0)
    .map(Duration::from_secs_f64)
}

/// Words in an error that mean another try changes nothing: the provider's filter blocked the answer, the output limit was reached.
const NEVER_WORDS: [&str; 2] = ["blocked by a content filter", "maximum output token limit"];

/// Words in an error that mean the conversation no longer fits the model's window ([`RetryClass::HistoryUnusable`]).
const HISTORY_WORDS: [&str; 4] = [
    "context window",
    "too long for requested model",
    "context length",
    "maximum context",
];

/// True when `text` (lower case) names a message of the replayed history, as the provider does when one is malformed:
/// `messages.7.content.0.toolUse.input is invalid`.
fn names_a_message(text: &str) -> bool {
    text.match_indices("messages.").any(|(i, m)| {
        text[i + m.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
}

/// AWS exception names (without the `Exception` suffix, lower case) and engine error names that pass.
const TRANSIENT_NAMES: [&str; 15] = [
    "econnreset",
    "epipe",
    "etimedout",
    "econnrefused",
    "throttling",
    "toomanyrequests",
    "serviceunavailable",
    "internalserver",
    "modelstreamerror",
    "modeltimeout",
    "modelnotready",
    "timeouterror",
    "connectionclosed",
    // The session credentials are renewed through agentd; a request signed just before the renewal is refused once.
    "expiredtoken",
    "requestexpired",
];

/// Names of an error that mean the route itself is refused (lower case, without the `Exception` suffix).
const UNAVAILABLE_NAMES: [&str; 3] = ["accessdenied", "resourcenotfound", "modelnotfound"];

/// Words that mean the route itself is refused: the model id is not valid for this account, Region or call.
const UNAVAILABLE_WORDS: [&str; 5] = [
    "model identifier is invalid",
    "on-demand throughput",
    "is not authorized to perform: bedrock",
    "don't have access to the model",
    "could not resolve the foundation model",
];

/// The class of a failure that ended the prompt. Only a JSON-RPC internal error (-32603), the code the engine uses for a failed model
/// request, is retried here; every other error is [`RetryClass::Never`]. A replayed history the provider refuses is
/// [`RetryClass::HistoryUnusable`] whatever the retry flag says.
pub fn classify_failure(err: &AcpError) -> RetryClass {
    let AcpError::Rpc {
        code: -32603,
        message,
        data,
    } = err
    else {
        return RetryClass::Never;
    };
    let text = message
        .strip_prefix("Internal error: ")
        .unwrap_or(message)
        .to_lowercase();
    if NEVER_WORDS.iter().any(|w| text.contains(w)) {
        return RetryClass::Never;
    }
    if HISTORY_WORDS.iter().any(|w| text.contains(w)) || names_a_message(&text) {
        return RetryClass::HistoryUnusable;
    }
    if is_idle_timeout(message) {
        return RetryClass::Transient;
    }
    let data = data.as_ref().and_then(Value::as_object);
    let field = |k: &str| data.and_then(|d| d.get(k));
    let flag = field("isRetryable").and_then(Value::as_bool);
    let status = field("statusCode")
        .or_else(|| field("status"))
        .and_then(Value::as_u64);
    let name = ["errorName", "errorType", "name", "code"]
        .iter()
        .find_map(|k| field(k).and_then(Value::as_str))
        .map(|n| n.to_lowercase().trim_end_matches("exception").to_string());
    if flag == Some(true)
        || name
            .as_deref()
            .is_some_and(|n| TRANSIENT_NAMES.contains(&n))
        || status.is_some_and(|s| s >= 500 || matches!(s, 408 | 409 | 424 | 425 | 429))
    {
        return RetryClass::Transient;
    }
    if status == Some(404)
        || name
            .as_deref()
            .is_some_and(|n| UNAVAILABLE_NAMES.contains(&n))
        || UNAVAILABLE_WORDS.iter().any(|w| text.contains(w))
    {
        return RetryClass::Unavailable;
    }
    RetryClass::Permanent
}

/// The progress event of a retry and the log line share these words.
fn retry_note(attempt: u32, of: u32, class: RetryClass, backoff: Duration, detail: &str) -> String {
    format!(
        "provider failure: try {attempt} of {of} follows in {} s ({}): {detail}",
        backoff.as_secs(),
        class.as_str()
    )
}

/// Waits `d`, or less when the user's cancel arrives; `true` when the wait ended because of it.
async fn pause(d: Duration, cancel: Option<&watch::Receiver<bool>>) -> bool {
    let Some(cancel) = cancel else {
        tokio::time::sleep(d).await;
        return false;
    };
    let mut cancel = cancel.clone();
    let sleep = tokio::time::sleep(d);
    tokio::pin!(sleep);
    loop {
        if *cancel.borrow() {
            return true;
        }
        tokio::select! {
            () = &mut sleep => return false,
            changed = cancel.changed() => if changed.is_err() {
                sleep.await;
                return false;
            },
        }
    }
}

/// Waits `total` in pieces of at most `slice`, marking the engine's silence clock between them (agentd is the one waiting, so the
/// provider-silence watchdog must not count it), or less when the user's cancel arrives; `true` when the wait ended because of it.
async fn wait_marking(
    total: Duration,
    slice: Duration,
    cancel: Option<&watch::Receiver<bool>>,
    activity: &Activity,
) -> bool {
    let mut left = total;
    while !left.is_zero() {
        let piece = left.min(slice.max(Duration::from_millis(1)));
        if pause(piece, cancel).await {
            return true;
        }
        activity.touch();
        left = left.saturating_sub(piece);
    }
    false
}

/// The user stopped the run while it waited: there is no prompt in flight to cancel, so the cancel is recorded and the answer is the
/// engine's `cancelled`, which the classifier reads as the user's stop.
async fn cancelled_while_waiting<R, W>(
    client: &mut AcpClient<R, W>,
    activity: &Activity,
    session_id: &str,
) -> Result<(PromptDone, Option<Usage>), AcpError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    activity.cancel_written();
    let _ = client
        .notify("session/cancel", json!({ "sessionId": session_id }))
        .await;
    Ok((
        PromptDone {
            stop_reason: "cancelled".into(),
            usage: None,
        },
        None,
    ))
}

/// Asks the engine to run the next prompt on `route` (`session/set_config_option`, `model`) and checks what it reports back. The
/// engine applies the model to the NEXT prompt (measured 2026-10-15), which is why a switch happens only between two prompts.
pub(crate) async fn set_model<R, W>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    session_id: &str,
    engine: &str,
    route: &Route,
    timeout: Duration,
) -> Result<(), String>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    // The engine takes the model in its own form (`acp_model_value`); the route keeps the catalog id.
    let value = acp_model_value(engine, &route.id);
    let answer = tokio::time::timeout(
        timeout,
        client.request(
            "session/set_config_option",
            json!({ "sessionId": session_id, "configId": "model", "value": value }),
            collector,
        ),
    )
    .await
    .map_err(|_| format!("no answer within {} s", timeout.as_secs()))?
    .map_err(|e| scrub_detail(&e.to_string()))?;
    let reported = answer
        .get("configOptions")
        .and_then(Value::as_array)
        .and_then(|opts| {
            opts.iter()
                .find(|o| o.get("id").and_then(Value::as_str) == Some("model"))
        })
        .and_then(|o| o.get("currentValue"))
        .and_then(Value::as_str);
    if reported == Some(value.as_str()) {
        Ok(())
    } else {
        Err(format!(
            "the engine reported {} instead of the model asked for",
            reported.unwrap_or("none")
        ))
    }
}

/// The wake-up spread of an outage wait: up to a tenth above the plain wait, and at least 5 s.
fn outage_wait(plain: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| u64::from(t.subsec_nanos()));
    let extra = Duration::from_secs_f64(plain.as_secs_f64() * (nanos % 101) as f64 / 1000.0);
    (plain + extra).max(Duration::from_secs(5))
}

/// How long a stretch with every route down lasts before the run says so once (the signed ladder's `alarmAfterS`).
const OUTAGE_ALARM_AFTER: Duration = Duration::from_secs(900);

/// A prompt boundary: measures the session's two disks and announces each crossing of an early mark once (`notify`, reason
/// `disk_low`). It deletes nothing. A run with no disk check measures nothing.
pub(crate) async fn disk_boundary(
    policy: &RetryPolicy,
    collector: &DirectCollector,
    record: &mut FailoverRecord,
) {
    let Some(check) = &policy.disk else {
        return;
    };
    let reading = check.probe.measure().await;
    for note in record.disk.crossings(&check.limits, &reading) {
        collector.record_notify("disk_low", None, &note);
        if let Some(log) = &policy.log {
            log.line(&format!("direct turn disk low: {note}"));
        }
    }
}

/// The error is the engine's -32603, the code it uses for a failed model request.
fn is_internal_error(err: &AcpError) -> bool {
    matches!(err, AcpError::Rpc { code: -32603, .. })
}

/// Sends `text` as the prompt of the session and, when it ends in a provider failure that deserves another try, the fixed
/// [`RESUME_PROMPT`] on the same session after a wait, up to the policy's count for the failure's class. With failover routes
/// ([`PromptPlan::failover`]) a spent route is followed by a switch of the engine to the next route (the other profile of the same
/// model, then the other models), and when every route has failed the loop waits, probes and begins again at the preferred route
/// instead of failing ([`Ladder`] decides each step).
///
/// `attempts` gets one entry per try and `record` one entry per confirmed switch. A cancel from the user during a wait ends the run
/// as the user's stop: a `cancelled` answer is returned without another prompt. Any error that is not retried, and the last error
/// after the last retry, is returned unchanged.
///
/// A call is a stage of the run: its attempts continue the numbers of
/// `attempts`, its budgets (resumes, the engine-stop resume) are its own, and `record.stage` holds what it learned
/// ([`FailoverRecord::begin_stage`] starts a stage over). The first send is an attempt of kind `Prompt`; use
/// [`prompt_with_retry_from`] for another kind.
#[allow(clippy::too_many_arguments)]
pub async fn prompt_with_retry<R, W>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    activity: &Arc<Activity>,
    session_id: &str,
    text: &str,
    plan: &PromptPlan,
    attempts: &mut Vec<DirectAttempt>,
    record: &mut FailoverRecord,
) -> Result<(PromptDone, Option<Usage>), AcpError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    prompt_with_retry_from(
        client,
        collector,
        activity,
        session_id,
        text,
        plan,
        attempts,
        record,
        DirectAttemptKind::Prompt,
    )
    .await
}

/// [`prompt_with_retry`] with the kind of the attempt that sends `text` (a later send of the call is always `Resume`).
#[allow(clippy::too_many_arguments)]
pub async fn prompt_with_retry_from<R, W>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    activity: &Arc<Activity>,
    session_id: &str,
    text: &str,
    plan: &PromptPlan,
    attempts: &mut Vec<DirectAttempt>,
    record: &mut FailoverRecord,
    first_kind: DirectAttemptKind,
) -> Result<(PromptDone, Option<Usage>), AcpError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let policy = &plan.retry;
    let failover = plan.failover.as_ref().filter(|f| f.routes.len() > 1);
    let mut ladder = match failover {
        Some(f) => f.ladder(policy.clone()),
        None => Ladder::new(
            vec![Route {
                id: plan.model.clone(),
                region: String::new(),
                context_tokens: 0,
                max_output_tokens: 0,
            }],
            LadderPolicy::new(policy.clone()),
        ),
    };
    let mut sending = text.to_string();
    // The numbers continue those of the earlier calls of the run.
    let mut n = u32::try_from(attempts.len()).unwrap_or(u32::MAX);
    let mut first_send = true;
    // Resumes already sent for a stop the engine chose itself (a run-wide count: the engine's brake is not a route's budget).
    let mut engine_stop_used = 0u32;
    // The outage in progress: how long the run has waited with every route down, across passes that failed again at once, and whether it
    // has said so.
    let mut outage_waited = Duration::ZERO;
    let mut alarmed = false;
    let mut noticed = false;
    // The request id of the outage notice while it is open: a prompt that succeeds again closes it `recovered`.
    let mut notice_id: Option<String> = None;
    // The machine's own report is logged once, at the first -32603 that names no cause.
    let mut diagnosed = false;
    loop {
        n += 1;
        // Each prompt is a boundary at which the session's disks are looked at.
        disk_boundary(policy, collector, record).await;
        let started = now_unix();
        let began = tokio::time::Instant::now();
        let kind = if std::mem::take(&mut first_send) {
            first_kind
        } else {
            DirectAttemptKind::Resume
        };
        let result = send_prompt(client, collector, session_id, &sending).await;
        let mut attempt = DirectAttempt {
            n,
            kind,
            started_at: format_rfc3339(started),
            ended_at: format_rfc3339(now_unix()),
            result: DirectAttemptResult::Ok,
            retry_class: None,
            detail: None,
            backoff_s: None,
            model_id: failover.map(|_| ladder.route(ladder.current()).id.clone()),
        };
        // What ended the try, when it is a reason for another: a JSON-RPC error (its class decides), or the engine ending the
        // prompt `cancelled` with nobody having asked. That second one is the engine's own brake (its loop guard or its mistake limit;
        // a really cut model stream is a -32603), not a provider failure: it has its own class and its own resumes and never moves the
        // ladder.
        let facts = collector.attempt_facts();
        let ran = began.elapsed();
        let (mut class, why, err_for_wait, resume, report) = match &result {
            Ok((done, _)) if self_cancelled(done, activity, plan) => (
                RetryClass::EngineStop,
                DirectAttemptResult::EngineCancelled,
                AcpError::Closed,
                RESUME_PROMPT_INTERRUPTED,
                Some(StopReport::new(facts, ran)),
            ),
            Ok(_) => {
                attempts.push(attempt);
                // A prompt succeeded: the route answers, so the notice of an outage is over.
                if let Some(id) = notice_id.take() {
                    collector.record_outage_recovered(&id, outage_waited.as_secs());
                }
                return result;
            }
            Err(e) => (
                classify_failure(e),
                DirectAttemptResult::ProviderError,
                e.clone(),
                RESUME_PROMPT,
                None,
            ),
        };
        let model_now = ladder.route(ladder.current()).id.clone();
        let mut detail = match (&result, &report) {
            (Err(e), _) => scrub_detail(&e.to_string()),
            (Ok(_), Some(r)) => format!(
                "the engine ended the prompt cancelled with no cancel request ({}); the model was not changed",
                r.describe()
            ),
            (Ok(_), None) => {
                "the engine ended the prompt cancelled with no cancel request".to_string()
            }
        };
        // Error -32603 while the session disk is short is the disk, not the provider: the checkout mount holds 1 GiB and reports no
        // use, and a full mount made the engine answer -32603 on every turn, which route failover took for a provider fault and cycled every
        // route for. The measurement is fresh (never the reading of the prompt boundary) and comes before any bookkeeping of the routes. An
        // error that names its own cause (a content filter, a full context window, a refused route) is not measured.
        let disk_full = match (&result, &policy.disk) {
            (Err(e), Some(check)) if is_internal_error(e) && !activity.cancel_sent() => {
                // The engine's own text can name the disk (the managed storage turns read-only once it is full), whatever class the
                // error has; without a text the measurement decides, for the classes a provider fault can have.
                let signal = disk::signal_of_error(e);
                // At the first bare -32603 (the text names neither the disk nor a provider) the log gets what the machine reports:
                // which folder refused a test write, space and inodes, the mounts and the kernel's last lines. It costs one look,
                // once per call.
                if signal.is_none()
                    && !diagnosed
                    && local_fault::bare_internal_error(&e.to_string())
                {
                    diagnosed = true;
                    if let Some(log) = &policy.log {
                        log.line(&format!(
                            "direct turn engine error -32603 names no cause; the machine reports:\n{}",
                            check.probe.diagnose().await
                        ));
                    }
                }
                if signal.is_some()
                    || matches!(class, RetryClass::Transient | RetryClass::Permanent)
                {
                    check.limits.full_with(&check.probe.measure().await, signal)
                } else {
                    None
                }
            }
            _ => None,
        };
        // Nothing explains the stop, no tool call completed and it came soon: it could be a provider fault that showed no output. The
        // route is asked once with a small test request; only a route that does not answer makes the stop a provider failure.
        if !activity.cancel_sent()
            && let Some(r) = &report
            && r.needs_probe()
            && let Some(probe) = failover.and_then(|f| f.probe.as_ref())
        {
            record.probes += 1;
            let answer = probe.probe(&ladder.route(ladder.current()).clone()).await;
            if let Some(log) = &policy.log {
                log.line(&format!(
                    "direct turn probe of {model_now} after an unexplained stop: {answer:?}"
                ));
            }
            let down = Some(matches!(answer, ProbeAnswer::Down(_)));
            if r.verdict(down) == Verdict::ProviderFailure
                && let ProbeAnswer::Down(why_down) = answer
            {
                class = RetryClass::Transient;
                detail = scrub_detail(&format!(
                    "the engine ended the prompt cancelled with no cancel request, and the route did not answer a small test request ({why_down})"
                ));
            }
        }
        attempt.result = why;
        attempt.retry_class = Some(
            disk_full
                .as_ref()
                .map_or_else(|| class.as_str(), disk::DiskFull::reason_word)
                .to_string(),
        );
        attempt.detail = Some(match &disk_full {
            Some(full) => scrub_detail(&format!("{detail} ({})", full.line())),
            None => detail.clone(),
        });
        attempts.push(attempt);
        // A failure after the user's own cancel is not retried.
        if activity.cancel_sent() {
            return result;
        }
        // The cause is written down first, then announced; nothing below it runs: no resume, no wait, no switch, no probe of a route, no
        // later prompt. The person decides (the question of the input machine, or the end of the run).
        if let Some(full) = disk_full {
            record.stage.disk_full = Some(full.clone());
            collector.record_notify(full.reason_word(), None, &full.notice());
            if let Some(log) = &policy.log {
                log.line(&format!(
                    "direct turn disk full: {}; no resume, no switch of route",
                    full.line()
                ));
            }
            return result;
        }
        ladder.healthy_for(began.elapsed());
        // A submission that ran as long as the longest wait ended the outage: the next one starts the count again.
        if failover.is_some_and(|f| began.elapsed() >= f.wait_cap) {
            if let Some(id) = notice_id.take() {
                collector.record_outage_recovered(&id, outage_waited.as_secs());
            }
            outage_waited = Duration::ZERO;
            alarmed = false;
            noticed = false;
        }
        // The engine's own stop: resume on the same route and model while the count allows, then end. Never a switch, never an
        // exclusion, never a fruitless-switch count.
        if class == RetryClass::EngineStop {
            ladder.engine_stopped();
            record.stage.stage_engine_stops += 1;
            record.stage.last_shape = report.as_ref().map(|r| r.shape);
            record.stage.identical_calls = Some(facts.identical_tail);
            record.stage.failed_turns = Some(facts.failed_tail);
            if engine_stop_used >= plan.engine_stop_resumes {
                return result;
            }
            engine_stop_used += 1;
            let wait = policy.jittered(policy.pause_before(engine_stop_used, &AcpError::Closed));
            if let Some(a) = attempts.last_mut() {
                a.backoff_s = Some(wait.as_secs());
            }
            let report = report.unwrap_or_else(|| StopReport::new(facts, ran));
            let hint = report.hint();
            let note = format!(
                "the engine stopped the run itself ({}); the model stays {model_now}; one more try follows in {} s",
                report.describe(),
                wait.as_secs()
            );
            collector.record_engine_stop(
                hint,
                engine_stop_used,
                plan.engine_stop_resumes,
                wait.as_secs(),
                &note,
            );
            if let Some(log) = &policy.log {
                log.line(&format!(
                    "direct turn engine stop {engine_stop_used} of {}: {hint}: {note}",
                    plan.engine_stop_resumes
                ));
            }
            let slice = failover.map_or(wait, |f| f.wait_slice);
            if wait_marking(wait, slice, plan.cancel.as_ref(), activity).await {
                return cancelled_while_waiting(client, activity, session_id).await;
            }
            sending = resume.to_string();
            continue;
        }
        // The history the engine replays is what the provider refuses: another try, another
        // route or another model would send the same history, and a route must not be excluded for it. The run ends here; the notify
        // event tells the owner of the fresh-session remedy what happened.
        if class == RetryClass::HistoryUnusable {
            let note = format!(
                "the provider refuses the conversation the engine replays, not the route {model_now}: {detail}"
            );
            collector.record_notify("history_unusable", None, &note);
            if let Some(log) = &policy.log {
                log.line(&format!("direct turn history unusable: {note}"));
            }
            return result;
        }
        // A provider failure: a switch whose first attempt did nothing counts toward the hold, and a run that has made as many
        // switches as the summary can list stops switching.
        ladder.attempt_ended(facts.completed, ran);
        if ladder.fruitless_prompts() >= FRUITLESS_PROMPTS_MAX {
            record.stage.cap_hit = true;
            // A run that can ask its user does so now; the others hold and wait for the deadline.
            if failover.is_some_and(|f| f.stop_at_cap) {
                if let Some(log) = &policy.log {
                    log.line(&format!(
                        "direct turn stops at the cap: {} prompts in a row completed no tool call",
                        ladder.fruitless_prompts()
                    ));
                }
                return result;
            }
        }
        if record.switches.len() >= MAX_DIRECT_SWITCHES {
            ladder.force_hold();
        }
        let mut decision =
            ladder.failed(class, &err_for_wait, tokio::time::Instant::now().into_std());
        // What the walk over the routes meets after this failure: a pass in which every route answers its small test request and then
        // refuses the switch with the engine's bare error is the machine failing, not a provider.
        let mut tally = Tally::default();
        'decide: loop {
            match decision {
                Next::Stop(_) => return result,
                Next::Retry { wait } => {
                    let wait = policy.jittered(wait);
                    if let Some(a) = attempts.last_mut() {
                        a.backoff_s = Some(wait.as_secs());
                    }
                    // "Try 2 of 4" counts the tries on this route: the resumes sent here so far, and the most it gets.
                    let (used, allowed) = ladder.retry_progress();
                    let note = retry_note(used + 1, allowed + 1, class, wait, &detail);
                    collector.record_provider_retry(
                        used + 1,
                        allowed + 1,
                        class.as_str(),
                        wait.as_secs(),
                        &detail,
                    );
                    if let Some(log) = &policy.log {
                        log.line(&format!("direct turn {note}"));
                    }
                    let slice = failover.map_or(wait, |f| f.wait_slice);
                    if wait_marking(wait, slice, plan.cancel.as_ref(), activity).await {
                        return cancelled_while_waiting(client, activity, session_id).await;
                    }
                    break 'decide;
                }
                Next::Switch { to, kind } => {
                    let Some(f) = failover else {
                        return result;
                    };
                    let from = ladder.route(ladder.current()).id.clone();
                    let target = ladder.route(to).clone();
                    // The small test request first: the whole conversation is sent to a route only when the
                    // route answered a question of 16 tokens. A route that does not answer is used up for the pass, one the provider
                    // refuses is out for the session; neither costs a prompt.
                    if let Some(probe) = f.probe.as_ref() {
                        record.probes += 1;
                        let answer = probe.probe(&target).await;
                        if let Some(log) = &policy.log {
                            log.line(&format!(
                                "direct turn probe of {} before a switch: {answer:?}",
                                target.id
                            ));
                        }
                        match answer {
                            ProbeAnswer::Healthy => {}
                            ProbeAnswer::Down(_) => {
                                tally.other();
                                decision =
                                    ladder.unanswered(to, tokio::time::Instant::now().into_std());
                                continue 'decide;
                            }
                            ProbeAnswer::Refused(_) => {
                                tally.other();
                                decision =
                                    ladder.refused(to, tokio::time::Instant::now().into_std());
                                continue 'decide;
                            }
                        }
                    } else {
                        // No small test request to ask: nothing shows that the routes answer.
                        tally.other();
                    }
                    match set_model(
                        client,
                        collector,
                        session_id,
                        &plan.engine,
                        &target,
                        f.switch_timeout,
                    )
                    .await
                    {
                        Ok(()) => {
                            tally.other();
                            let switch = DirectSwitch {
                                n: u32::try_from(record.switches.len() + 1).unwrap_or(u32::MAX),
                                at: format_rfc3339(now_unix()),
                                from,
                                to: target.id.clone(),
                                kind,
                                class: ladder.last_class().to_string(),
                                detail: Some(detail.clone()).filter(|d| !d.is_empty()),
                            };
                            ladder.switched(to);
                            record.final_model = Some(target.id.clone());
                            collector.record_provider_switch(&switch);
                            if let Some(log) = &policy.log {
                                log.line(&format!(
                                    "direct turn provider switch {}: {} -> {} ({:?}, {})",
                                    switch.n, switch.from, switch.to, kind, switch.class
                                ));
                            }
                            record.switches.push(switch);
                            break 'decide;
                        }
                        Err(why) => {
                            if let Some(log) = &policy.log {
                                log.line(&format!(
                                    "direct turn provider switch to {} refused: {why}",
                                    target.id
                                ));
                            }
                            if f.probe.is_some() && local_fault::bare_internal_error(&why) {
                                tally.local_refusal();
                            } else {
                                tally.other();
                            }
                            decision = ladder.refused(to, tokio::time::Instant::now().into_std());
                            continue 'decide;
                        }
                    }
                }
                Next::AllDown { mut wait, pass } => {
                    let Some(f) = failover else {
                        return result;
                    };
                    // Every usable route was walked once and each answered its small test request and then refused the switch with the
                    // bare error that began the walk: the engine is failing on the machine. More passes and waits cannot help.
                    if tally.is_local_fault() && local_fault::bare_internal_error(&detail) {
                        let reading = match &policy.disk {
                            Some(check) => check.probe.measure().await,
                            None => Reading::default(),
                        };
                        let limits = policy
                            .disk
                            .as_ref()
                            .map_or_else(Limits::default, |c| c.limits);
                        let full = limits.engine_fault(&reading);
                        if let Some(a) = attempts.last_mut() {
                            a.retry_class = Some(full.reason_word().to_string());
                            a.detail = Some(scrub_detail(&format!("{detail} ({})", full.line())));
                        }
                        record.stage.disk_full = Some(full.clone());
                        collector.record_notify(full.reason_word(), None, &full.notice());
                        if let Some(log) = &policy.log {
                            log.line(&format!(
                                "direct turn local fault: {} routes answered a test request and refused the switch with the engine's bare error; {}; no wait, no second pass",
                                tally.routes(),
                                full.line()
                            ));
                        }
                        return result;
                    }
                    record.passes = pass;
                    record
                        .outage_since
                        .get_or_insert_with(tokio::time::Instant::now);
                    if let Some(a) = attempts.last_mut() {
                        a.backoff_s = Some(wait.as_secs());
                    }
                    loop {
                        let w = outage_wait(wait);
                        let why_waiting = if ladder.holding() {
                            format!(
                                "switching is on hold after {} prompts and {} switches that achieved nothing, so only the route you chose is asked, once per answered test request; {detail}",
                                ladder.fruitless_prompts(),
                                ladder.fruitless()
                            )
                        } else {
                            detail.clone()
                        };
                        collector.record_provider_wait(pass, w.as_secs(), &why_waiting);
                        if let Some(log) = &policy.log {
                            log.line(&format!(
                                "direct turn every route failed: waiting {} s before pass {}",
                                w.as_secs(),
                                pass + 1
                            ));
                        }
                        if wait_marking(w, f.wait_slice, plan.cancel.as_ref(), activity).await {
                            return cancelled_while_waiting(client, activity, session_id).await;
                        }
                        outage_waited += w;
                        record.waited_s += w.as_secs();
                        if !alarmed && outage_waited >= OUTAGE_ALARM_AFTER {
                            alarmed = true;
                            collector.record_provider_outage(outage_waited.as_secs());
                        }
                        // The hook of the recovery ladder's "needs your input": after this long with every route down the run
                        // says so once and goes on waiting and probing until its deadline or the user's cancel.
                        if !noticed && outage_waited >= f.notice_after {
                            noticed = true;
                            notice_id =
                                Some(collector.record_outage_notice(outage_waited.as_secs()));
                            if let Some(log) = &policy.log {
                                log.line(&format!(
                                    "direct turn outage notice: every route down for {} minutes",
                                    outage_waited.as_secs() / 60
                                ));
                            }
                        }
                        // Where the next pass begins: the first route whose small test request is answered, or the preferred
                        // route when there is no probe.
                        if ladder.usable().is_empty() {
                            return result;
                        }
                        let targets = ladder.probe_targets();
                        let start = match &f.probe {
                            None => Some(targets[0]),
                            Some(probe) => {
                                let mut healthy = None;
                                for i in targets {
                                    record.probes += 1;
                                    let answer = probe.probe(ladder.route(i)).await;
                                    if let Some(log) = &policy.log {
                                        log.line(&format!(
                                            "direct turn probe of {}: {answer:?}",
                                            ladder.route(i).id
                                        ));
                                    }
                                    match answer {
                                        ProbeAnswer::Healthy => {
                                            healthy = Some(i);
                                            break;
                                        }
                                        ProbeAnswer::Refused(_) => ladder.exclude(i),
                                        ProbeAnswer::Down(_) => {}
                                    }
                                }
                                healthy
                            }
                        };
                        let Some(start) = start else {
                            if ladder.usable().is_empty() {
                                return result;
                            }
                            wait = ladder.next_wait();
                            continue;
                        };
                        // The engine moves when the pass begins elsewhere than where it is.
                        match ladder.begin_pass(start, tokio::time::Instant::now().into_std()) {
                            None => break,
                            Some((to, kind)) => {
                                let from = ladder.route(ladder.current()).id.clone();
                                let target = ladder.route(to).clone();
                                match set_model(
                                    client,
                                    collector,
                                    session_id,
                                    &plan.engine,
                                    &target,
                                    f.switch_timeout,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        let switch = DirectSwitch {
                                            n: u32::try_from(record.switches.len() + 1)
                                                .unwrap_or(u32::MAX),
                                            at: format_rfc3339(now_unix()),
                                            from,
                                            to: target.id.clone(),
                                            kind,
                                            class: "unavailable".to_string(),
                                            detail: Some(
                                                "every route had failed; this one answered".into(),
                                            ),
                                        };
                                        ladder.switched(to);
                                        record.final_model = Some(target.id.clone());
                                        collector.record_provider_switch(&switch);
                                        record.switches.push(switch);
                                        break;
                                    }
                                    Err(why) => {
                                        if let Some(log) = &policy.log {
                                            log.line(&format!(
                                                "direct turn provider switch to {} refused: {why}",
                                                target.id
                                            ));
                                        }
                                        ladder.exclude(to);
                                        wait = ladder.next_wait();
                                    }
                                }
                            }
                        }
                    }
                    record.outage_since = None;
                    break 'decide;
                }
            }
        }
        sending = resume.to_string();
    }
}

/// The engine ended the prompt `cancelled` and neither agentd nor the user asked for it.
fn self_cancelled(done: &PromptDone, activity: &Activity, plan: &PromptPlan) -> bool {
    done.stop_reason == "cancelled"
        && !activity.cancel_sent()
        && !plan.cancel.as_ref().is_some_and(|c| *c.borrow())
}
