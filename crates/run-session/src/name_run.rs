//! The `name-run` op: the 5 to 7 word name of a run, written by the utility model after the run exists.
//!
//! The machine's first state `Name` invokes this op asynchronously and never waits for it, so a run starts the moment it is created and
//! shows a name built from its prompt (the seed, `ham_core::runname::seed_name`, computed where it is read and never stored) until this
//! op writes a model-written one. The op reads the run and its root goal, asks the utility model for a title over the redacted head and
//! tail of the prompt, lets the pure judge (`ham_core::runname::judge`) decide whether the reply may become a name, and stores the
//! result once on the run's search item (`Store::put_run_name`). Nothing else is written: not the Run item, not the delivery, not the
//! zip or the branch. A name is display text.
//!
//! The op has a second branch, the tags of the run, when `UTILITY_PURPOSES` lists `run-tags`. The two branches
//! share the reads of the run and its goal and then run side by side under `tokio::join!`. The tag branch returns a report and never an
//! error, it has its own limit, and it writes its own attributes of the same search item; so a failure or a stall of the tags never
//! touches the name, and only a store error of the name branch fails the op (Lambda runs the event again, and the tag branch then
//! answers `kept` without a call). A purpose that is not in the list does not run at all: no read, no call, no write, no log line.
//!
//! Failure rules (the design's matrix): the op never fails a run and never waits past its limit. A failure of the store is an error, so
//! Lambda runs the event again (the name is stored under a condition, so a repeat is harmless); every failure of the model side
//! (a throttle, a 5xx, a timeout, a refusal, a reply the judge rejects) is an answer, and the run keeps its seed. One JSON line per
//! op carries the outcome and counts; it never carries the prompt, the excerpt, the reply, the name or a hash of any of them.

use crate::error::RunSessionError;
use crate::ops::Deps;
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use crate::tag_run::{TagBudget, TagDeps, TagReport, tag_run};
use crate::utility::{UtilityCall, UtilityClient};
use ham_core::runname::{self, Reason};
use ham_core::utility::{
    self, UtilityFault, UtilityModel, UtilityPurpose, UtilityReply, request_body,
};
use harness_protocol::store::{GoalItem, RunItem};
use serde_json::{Map, Value, json};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use worker_api::handler::Minter;

/// Where the op's JSON line goes: the Lambda's standard output, or a collector in a test.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// A sink that prints the line, as the Lambda's log expects.
pub fn stdout_sink() -> LogSink {
    Arc::new(|line: &str| println!("{line}"))
}

/// The time the op may spend: the whole operation, one attempt, and the pause between the two attempts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameBudget {
    pub op: Duration,
    pub attempt: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

impl NameBudget {
    /// The limits of the purpose (`UtilityPurpose::RunName`): 30 s for the operation, 6 s for an attempt, and 200 to 400 ms between
    /// the attempts.
    pub fn production() -> NameBudget {
        let purpose = UtilityPurpose::RunName;
        NameBudget {
            op: purpose.op_deadline(),
            attempt: purpose.attempt_timeout(),
            backoff_min: Duration::from_millis(200),
            backoff_max: Duration::from_millis(400),
        }
    }
}

impl Default for NameBudget {
    fn default() -> Self {
        Self::production()
    }
}

/// What `Deps` carries for the op: the client (none when the Lambda has no way to call the model), the limits of the name branch and of
/// the tag branch, and the log.
#[derive(Clone)]
pub struct Namer {
    pub client: Option<Arc<dyn UtilityClient>>,
    pub budget: NameBudget,
    pub tag_budget: TagBudget,
    pub log: LogSink,
}

impl Namer {
    /// A namer that calls no model.
    pub fn off() -> Namer {
        Namer {
            client: None,
            budget: NameBudget::production(),
            tag_budget: TagBudget::production(),
            log: stdout_sink(),
        }
    }

    /// A namer over `client` with the production limits and the Lambda's log.
    pub fn new(client: Arc<dyn UtilityClient>) -> Namer {
        Namer {
            client: Some(client),
            budget: NameBudget::production(),
            tag_budget: TagBudget::production(),
            log: stdout_sink(),
        }
    }
}

impl fmt::Debug for Namer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Namer")
            .field("client", &self.client)
            .field("budget", &self.budget)
            .field("tag_budget", &self.tag_budget)
            .finish_non_exhaustive()
    }
}

/// The outcome words of the log line and of the op's answer.
mod named {
    pub const AI: &str = "ai";
    pub const AI_SQUEEZED: &str = "ai_squeezed";
    pub const SEED: &str = "seed";
    pub const KEPT: &str = "kept";
    pub const OFF: &str = "off";
    pub const SKIPPED: &str = "skipped";
}

/// The `reason` of a name that was written.
const WRITTEN: &str = "-";
/// The source word stored beside the name: the model wrote it. (`user` is reserved for a rename feature; nothing writes it.)
const SOURCE_AI: &str = "ai";

/// Whole seconds from the creation of the run (`created_at`, RFC 3339) to `now_ms`; zero when the clock is behind. `None` when the
/// time cannot be read.
pub fn age_seconds(created_at: &str, now_ms: u64) -> Option<u64> {
    let created = u64::try_from(harness_protocol::common::parse_utc_unix(created_at)?).ok()?;
    Some((now_ms / 1000).saturating_sub(created))
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What happened, for the answer and for the log line.
#[derive(Debug)]
struct Report {
    outcome: &'static str,
    reason: &'static str,
    route: Option<String>,
    attempts: u32,
    call_ms: Option<u64>,
    reply: Option<(String, u64, u64)>,
    prompt: Option<(usize, usize, usize)>,
    name: Option<(usize, usize)>,
    age_s: Option<u64>,
}

impl Report {
    fn new() -> Report {
        Report {
            outcome: named::SKIPPED,
            reason: Reason::NoRun.as_str(),
            route: None,
            attempts: 0,
            call_ms: None,
            reply: None,
            prompt: None,
            name: None,
            age_s: None,
        }
    }

    fn end(&mut self, outcome: &'static str, reason: &'static str) {
        self.outcome = outcome;
        self.reason = reason;
    }

    fn answer(&self) -> Value {
        json!({"named": self.outcome, "reason": self.reason})
    }

    /// The op's one line. Counts and fixed words only: never the prompt, the excerpt, the reply, the name, or a hash of them.
    fn line(&self, run_id: &str) -> String {
        let mut m = Map::new();
        m.insert("function".into(), json!("run-session"));
        m.insert("op".into(), json!("name-run"));
        m.insert("purpose".into(), json!(UtilityPurpose::RunName.as_str()));
        m.insert("promptVersion".into(), json!(runname::PROMPT_VERSION));
        m.insert("runId".into(), json!(run_id));
        m.insert("outcome".into(), json!(self.outcome));
        m.insert("reason".into(), json!(self.reason));
        if let Some(route) = &self.route {
            m.insert("route".into(), json!(route));
        }
        if self.attempts > 0 {
            m.insert("attempts".into(), json!(self.attempts));
        }
        if let Some(ms) = self.call_ms {
            m.insert("callMs".into(), json!(ms));
        }
        if let Some((stop, input, output)) = &self.reply {
            m.insert("stopReason".into(), json!(stop));
            m.insert("inputTokens".into(), json!(input));
            m.insert("outputTokens".into(), json!(output));
        }
        if let Some((chars, excerpt, words)) = self.prompt {
            m.insert("promptChars".into(), json!(chars));
            m.insert("excerptChars".into(), json!(excerpt));
            m.insert("promptWords".into(), json!(words));
        }
        if let Some((words, cols)) = self.name {
            m.insert("nameWords".into(), json!(words));
            m.insert("nameCols".into(), json!(cols));
        }
        if let Some(age) = self.age_s {
            m.insert("ageS".into(), json!(age));
        }
        Value::Object(m).to_string()
    }
}

/// What the attempts left behind, kept outside the future that makes them so the operation's limit can cut it off and the numbers it had
/// reached are still there.
#[derive(Debug, Default)]
struct Attempts {
    made: u32,
    last_call_ms: Option<u64>,
}

/// The pause before the second attempt: between the budget's two bounds, spread by the clock's nanoseconds.
fn backoff(b: &NameBudget) -> Duration {
    let span = millis(b.backoff_max.saturating_sub(b.backoff_min));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    b.backoff_min + Duration::from_millis(nanos % (span + 1))
}

/// Up to the purpose's attempts at the one route: a transient fault (a timeout, a throttle, a 5xx) is worth another, a refusal or an empty
/// answer is not.
async fn ask(
    client: &dyn UtilityClient,
    model: &UtilityModel,
    body: &Value,
    budget: NameBudget,
    seen: &mut Attempts,
) -> Result<UtilityReply, UtilityFault> {
    let max = UtilityPurpose::RunName.attempts();
    let mut attempt = 1;
    loop {
        seen.made = attempt;
        let started = Instant::now();
        let call = UtilityCall {
            route: &model.id,
            region: &model.region,
            body,
            timeout: budget.attempt,
        };
        let result = match tokio::time::timeout(budget.attempt, client.converse(&call)).await {
            Ok(r) => r,
            Err(_) => Err(UtilityFault::Timeout),
        };
        seen.last_call_ms = Some(millis(started.elapsed()));
        match result {
            Ok(reply) => return Ok(reply),
            Err(fault) if fault.retryable() && attempt < max => {
                tokio::time::sleep(backoff(&budget)).await;
                attempt += 1;
            }
            Err(fault) => return Err(fault),
        }
    }
}

/// Runs the op for `run_id` and answers `{"named": <outcome>, "reason": <code>}` and, when the tag branch is on, `"tagged"` and
/// `"tagReason"`; writes one log line per branch through the namer's sink.
pub async fn name_run<S, T, M, R, B, P>(deps: &Deps<S, T, M, R, B, P>, run_id: &str) -> Res<Value>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let mut report = Report::new();
    let mut tags: Option<TagReport> = None;
    let result = flow(deps, run_id, &mut report, &mut tags).await;
    // The name's line is printed when the name branch ended without a store error (the per-invocation line carries the error); the tag
    // branch's line is printed whatever happened to the name, because the branch did its own work.
    if result.is_ok() && deps.cfg.serves(UtilityPurpose::RunName) {
        (deps.namer.log)(&report.line(run_id));
    }
    if let Some(tags) = &tags {
        (deps.namer.log)(&tags.line(run_id));
    }
    result?;
    let mut answer = report.answer();
    if let Some(tags) = &tags {
        answer["tagged"] = json!(tags.outcome);
        answer["tagReason"] = json!(tags.reason);
    }
    Ok(answer)
}

/// The reads both branches share fail: the name branch (if it is on) makes the op fail, as it always did; a function that serves only the
/// tags never fails the op for the tags.
fn read_failed(
    error: RunSessionError,
    want_name: bool,
    want_tags: bool,
    report: &mut Report,
    tags: &mut Option<TagReport>,
) -> Result<(), RunSessionError> {
    if want_name {
        return Err(error);
    }
    report.end(named::OFF, Reason::Disabled.as_str());
    *tags = want_tags.then(TagReport::store_error);
    Ok(())
}

async fn flow<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    report: &mut Report,
    tags: &mut Option<TagReport>,
) -> Result<(), RunSessionError>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let want_name = deps.cfg.serves(UtilityPurpose::RunName);
    let want_tags = deps.cfg.serves(UtilityPurpose::RunTags);
    // The setting is off, or this Lambda has no way to call the model: no read, no call.
    let (Some(model), Some(client)) = (deps.cfg.utility.as_ref(), deps.namer.client.as_ref())
    else {
        report.end(named::OFF, Reason::Disabled.as_str());
        *tags = want_tags.then(TagReport::off);
        return Ok(());
    };
    let run = match deps.store.get_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            report.end(named::SKIPPED, Reason::NoRun.as_str());
            *tags = want_tags.then(TagReport::no_run);
            return Ok(());
        }
        Err(e) => return read_failed(e, want_name, want_tags, report, tags),
    };
    report.age_s = age_seconds(&run.created_at, deps.cfg.now_ms());
    let goal = match deps.store.get_root_goal(run_id).await {
        Ok(Some(goal)) => goal,
        Ok(None) => {
            report.end(named::SKIPPED, Reason::NoRun.as_str());
            *tags = want_tags.then(TagReport::no_run);
            return Ok(());
        }
        Err(e) => return read_failed(e, want_name, want_tags, report, tags),
    };

    // The two branches run side by side; each ends when its own work is done. The tag branch never errors.
    let name_side = async {
        if want_name {
            name_branch(deps, &run, &goal, model, client.as_ref(), report).await
        } else {
            report.end(named::OFF, Reason::Disabled.as_str());
            Ok(())
        }
    };
    let tag_side = async {
        if !want_tags {
            return None;
        }
        let excerpt = utility::excerpt(goal.objective.as_str());
        let tag_deps = TagDeps {
            store: &deps.store,
            client: client.as_ref(),
            model,
            now_ms: deps.cfg.now_ms(),
        };
        Some(tag_run(&tag_deps, &run, &excerpt, &deps.namer.tag_budget).await)
    };
    let (named_result, tagged) = tokio::join!(name_side, tag_side);
    *tags = tagged;
    named_result
}

/// The name branch: the op's work before the tags existed, from the stored name on.
async fn name_branch<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run: &RunItem,
    goal: &GoalItem,
    model: &UtilityModel,
    client: &dyn UtilityClient,
    report: &mut Report,
) -> Result<(), RunSessionError>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    // A name that is stored stays: a duplicate event, a redrive and a retry after a lost answer end here without a call.
    if deps.store.run_name(run).await?.is_some() {
        report.end(named::KEPT, Reason::Kept.as_str());
        return Ok(());
    }

    let prompt = goal.objective.as_str();
    let redacted = runname::redacted_prompt(prompt);
    let plan = runname::plan(prompt);
    report.prompt = Some((
        prompt.chars().count(),
        plan.user_text.chars().count(),
        runname::word_count(&redacted),
    ));
    if !plan.ask {
        report.end(named::SEED, Reason::ShortPrompt.as_str());
        return Ok(());
    }

    let budget = deps.namer.budget;
    let body = request_body(
        model,
        runname::system_text(),
        &plan.user_text,
        runname::MAX_TOKENS,
        &[runname::STOP],
    );
    report.route = Some(model.id.clone());
    let mut seen = Attempts::default();
    let asked = tokio::time::timeout(budget.op, ask(client, model, &body, budget, &mut seen)).await;
    report.attempts = seen.made;
    report.call_ms = seen.last_call_ms;
    let reply = match asked {
        Err(_) => {
            report.end(named::SEED, Reason::OpDeadline.as_str());
            return Ok(());
        }
        Ok(Err(fault)) => {
            report.end(named::SEED, Reason::from(fault).as_str());
            return Ok(());
        }
        Ok(Ok(reply)) => reply,
    };
    report.reply = Some((
        reply.stop_reason.clone(),
        reply.input_tokens,
        reply.output_tokens,
    ));

    let judged = match runname::judge(&reply, &redacted) {
        Ok(j) => j,
        Err(reason) => {
            report.end(named::SEED, reason.as_str());
            return Ok(());
        }
    };
    report.name = Some((
        runname::word_count(&judged.name),
        runname::display_width(&judged.name),
    ));
    // One conditional write: if another invocation stored a name first, that one stands.
    if deps
        .store
        .put_run_name(run, &judged.name, SOURCE_AI, &model.id)
        .await?
    {
        let word = if judged.squeezed {
            named::AI_SQUEEZED
        } else {
            named::AI
        };
        report.end(word, WRITTEN);
    } else {
        report.end(named::KEPT, Reason::Kept.as_str());
    }
    Ok(())
}
