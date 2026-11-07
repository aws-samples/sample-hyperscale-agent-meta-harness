//! The tag branch of the `name-run` op: up to six topic and tech tags for a run, written by the utility model after the run exists
//! and stored once as the run's automatic tags on its search item. A tag is display and search text: it never reaches a branch, a zip, an
//! S3 key, a pull request or a process argument, and no delivery code reads it.
//!
//! The branch runs beside the name branch under `tokio::join!` and is a free function with a budget of its own, so a change of the op's
//! shape costs a re-wire and not a rewrite. It never returns an error: every failure is an outcome and a reason code in the report, the
//! name branch is never touched, and the whole branch (reads, the vocabulary, the model call and the writes) is under one limit
//! ([`TagBudget::total`]). What the report says is counts and fixed words; it never carries the prompt, the excerpt, a reply or a tag.
//!
//! Flow, in order (design 7.2 of the run-tags work):
//! 1. The catalog row must hold a passing smoke record for the `run-tags` purpose.
//! 2. One consistent read of the item's marker and the person's removals. A marker `runTagsAt` ends the branch (`kept`): a duplicate
//!    event, a redrive and a retry after the name branch failed all end here without a call.
//! 3. A follow-up (`run.continue_from`) reads its parent's lists and copies the user and off lists once, before any model call. A
//!    person's edit that came first wins (the copy's condition fails and the item is read again), and a failed read or copy ends the
//!    branch with nothing written: the automatic write bumps the revision, and a revision locks the copy out for good.
//! 4. A prompt of fewer than four words asks no model. Otherwise one eventually consistent read of the owner's newest 200 search items
//!    makes the vocabulary (it is optional: a failure means `(none yet)`), and the model is asked with the shared excerpt.
//! 5. `ham_core::runtag::judge::judge_reply` checks the whole reply and then each tag. A reply that fails as a whole keeps nothing and is
//!    never cleaned; a reply that passes is an answer even when every tag was refused.
//! 6. The automatic list is the model's kept tags, then the parent's automatic tags, up to six, none of them on an off list.
//! 7. One conditional write stores the list and the marker. The marker means "the automatic step finished"; `runTagsVer` is present only
//!    when the model answered. A model failure on a plain run writes nothing, so a later catch-up can fill the run.
//!
//! The person's removal wins on every read (`ham_core::runtag::effective`): no write here names the off list of a run it did not copy,
//! so a removal that lands between a read and the write is never undone.

use crate::error::RunSessionError;
use crate::name_run::age_seconds;
use crate::ports::{Res, Store};
use crate::utility::{UtilityCall, UtilityClient};
use ham_core::runtag::dynamo::{Answered, VOCABULARY_WINDOW};
use ham_core::runtag::edit::inherit;
use ham_core::runtag::grammar::key_of;
use ham_core::runtag::judge::{self, compose_auto, judge_reply, read_reply};
use ham_core::runtag::vocab::Vocabulary;
use ham_core::utility::{
    Excerpt, UtilityFault, UtilityModel, UtilityPurpose, UtilityReply, is_short,
};
use harness_protocol::store::RunItem;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The time the branch may spend. [`TagBudget::production`] is the limits of the purpose; a test uses units of tens of milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagBudget {
    /// The vocabulary read.
    pub vocab: Duration,
    /// One model attempt.
    pub attempt: Duration,
    /// The most model attempts (one more for a transient fault).
    pub attempts: u32,
    /// The pause between two attempts or two tries of a store call.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// The whole branch.
    pub total: Duration,
}

impl TagBudget {
    /// The limits of `UtilityPurpose::RunTags`: 2 s for the vocabulary, 6 s an attempt, 2 attempts, 200 to 400 ms between tries and 20 s
    /// for the branch, which runs inside the 30 s of the op.
    pub fn production() -> TagBudget {
        let purpose = UtilityPurpose::RunTags;
        TagBudget {
            vocab: Duration::from_secs(2),
            attempt: purpose.attempt_timeout(),
            attempts: purpose.attempts(),
            backoff_min: Duration::from_millis(200),
            backoff_max: Duration::from_millis(400),
            total: purpose.op_deadline(),
        }
    }
}

impl Default for TagBudget {
    fn default() -> Self {
        Self::production()
    }
}

/// What the branch needs: the store, the model client and the catalog row, and the time of the op.
pub struct TagDeps<'a, S> {
    pub store: &'a S,
    pub client: &'a dyn UtilityClient,
    pub model: &'a UtilityModel,
    pub now_ms: u64,
}

impl<S> std::fmt::Debug for TagDeps<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TagDeps")
            .field("client", &self.client)
            .field("model", &self.model.id)
            .field("now_ms", &self.now_ms)
            .finish_non_exhaustive()
    }
}

/// The outcome words of the report.
mod outcome {
    pub const WRITTEN: &str = "written";
    pub const INHERITED: &str = "inherited";
    pub const NONE: &str = "none";
    pub const KEPT: &str = "kept";
    pub const SKIPPED: &str = "skipped";
    pub const OFF: &str = "off";
}

/// The reason codes the branch itself decides. The model's faults use `UtilityFault::code`, the judge's refusals
/// `ham_core::runtag::grammar::Reason::as_str`.
mod why {
    /// A tag list was written.
    pub const WRITTEN: &str = "-";
    pub const SHORT_PROMPT: &str = "short_prompt";
    pub const DISABLED: &str = "disabled";
    pub const NO_RUN: &str = "no_run";
    pub const NOT_SMOKED: &str = "not_smoked";
    pub const KEPT: &str = "kept";
    pub const STORE_ERROR: &str = "store_error";
    pub const VOCAB_UNAVAILABLE: &str = "vocab_unavailable";
    pub const NO_TAGS: &str = "no_tags";
    pub const OP_DEADLINE: &str = "op_deadline";
}

/// The facts about the vocabulary read.
#[derive(Clone, Copy, Debug, Default)]
struct VocabFacts {
    lines: usize,
    window_runs: usize,
    top_share_bp: u32,
    skipped: usize,
}

/// The facts about the judged reply.
#[derive(Clone, Debug, Default)]
struct JudgeFacts {
    proposed: usize,
    kept: usize,
    reused: usize,
    new: usize,
    rejected: BTreeMap<&'static str, usize>,
}

/// What happened, for the op's answer and for the log line.
#[derive(Clone, Debug)]
pub struct TagReport {
    pub outcome: &'static str,
    pub reason: &'static str,
    route: Option<String>,
    attempts: u32,
    call_ms: Option<u64>,
    reply: Option<(String, u64, u64)>,
    prompt: Option<(usize, usize)>,
    age_s: Option<u64>,
    vocab: Option<VocabFacts>,
    judged: Option<JudgeFacts>,
    /// The copy of the parent's lists was stored, so the follow-up holds something even when the rest of the branch fails.
    wrote_inherit: bool,
}

impl TagReport {
    fn blank(outcome: &'static str, reason: &'static str) -> TagReport {
        TagReport {
            outcome,
            reason,
            route: None,
            attempts: 0,
            call_ms: None,
            reply: None,
            prompt: None,
            age_s: None,
            vocab: None,
            judged: None,
            wrote_inherit: false,
        }
    }

    /// The setting is off, or the function has no way to call the model: nothing was read.
    pub fn off() -> TagReport {
        TagReport::blank(outcome::OFF, why::DISABLED)
    }

    /// The branch did not run for want of a run or a goal.
    pub fn no_run() -> TagReport {
        TagReport::blank(outcome::SKIPPED, why::NO_RUN)
    }

    /// The run could not be read at all (a store error before the branch started).
    pub fn store_error() -> TagReport {
        TagReport::blank(outcome::NONE, why::STORE_ERROR)
    }

    fn start(run: &RunItem, excerpt: &Excerpt, now_ms: u64) -> TagReport {
        // The default stands if the limit cuts the flow before it ends: nothing is known to be written.
        let mut r = TagReport::blank(outcome::NONE, why::OP_DEADLINE);
        r.prompt = Some((excerpt.words, excerpt.escaped.chars().count()));
        r.age_s = age_seconds(&run.created_at, now_ms);
        r
    }

    fn end(&mut self, outcome: &'static str, reason: &'static str) {
        self.outcome = outcome;
        self.reason = reason;
    }

    /// The line of the op for the log. Counts and fixed words only: never the prompt, the excerpt, the reply, a tag or a hash of them.
    pub fn line(&self, run_id: &str) -> String {
        let mut m = Map::new();
        m.insert("function".into(), json!("run-session"));
        m.insert("op".into(), json!("name-run"));
        m.insert("purpose".into(), json!(UtilityPurpose::RunTags.as_str()));
        m.insert("promptVersion".into(), json!(judge::PROMPT_VERSION));
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
        if let Some((words, chars)) = self.prompt {
            m.insert("promptWords".into(), json!(words));
            m.insert("excerptChars".into(), json!(chars));
        }
        if let Some(age) = self.age_s {
            m.insert("ageS".into(), json!(age));
        }
        if let Some(v) = &self.vocab {
            m.insert("vocabLines".into(), json!(v.lines));
            m.insert("windowRuns".into(), json!(v.window_runs));
            m.insert("topShareBp".into(), json!(v.top_share_bp));
            if v.skipped > 0 {
                m.insert("vocabSkipped".into(), json!(v.skipped));
            }
        }
        if let Some(j) = &self.judged {
            m.insert("proposed".into(), json!(j.proposed));
            m.insert("keptN".into(), json!(j.kept));
            m.insert("reused".into(), json!(j.reused));
            m.insert("new".into(), json!(j.new));
            m.insert("rejected".into(), json!(j.rejected));
        }
        Value::Object(m).to_string()
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// A pause between `min` and `max`, spread by the clock's nanoseconds.
fn jitter(min: Duration, max: Duration) -> Duration {
    let span = millis(max.saturating_sub(min));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    min + Duration::from_millis(nanos % (span + 1))
}

/// One try again of a store call that failed for a reason a second try can change. A call that can never work (`Invalid`) is not tried
/// again.
async fn retry_once<T, F, Fut>(budget: &TagBudget, mut call: F) -> Res<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Res<T>>,
{
    match call().await {
        Err(RunSessionError::Conflict(_) | RunSessionError::Unavailable(_)) => {
            tokio::time::sleep(jitter(budget.backoff_min, budget.backoff_max)).await;
            call().await
        }
        other => other,
    }
}

/// The union of two lists of tags, the first one's order first, the same text once.
fn union(first: &[String], second: &[String]) -> Vec<String> {
    let mut out: Vec<String> = first.to_vec();
    for tag in second {
        if !out.contains(tag) {
            out.push(tag.clone());
        }
    }
    out
}

/// Up to the purpose's attempts at the one route: a transient fault (a timeout, a throttle, a 5xx) is worth another, a refusal or an
/// empty answer is not. The counts go into the report as they are reached, so the branch's limit can cut this off and still leave them.
async fn ask(
    client: &dyn UtilityClient,
    model: &UtilityModel,
    body: &Value,
    budget: &TagBudget,
    rep: &mut TagReport,
) -> Result<UtilityReply, UtilityFault> {
    let mut attempt = 1;
    loop {
        rep.attempts = attempt;
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
        rep.call_ms = Some(millis(started.elapsed()));
        match result {
            Ok(reply) => return Ok(reply),
            Err(fault) if fault.retryable() && attempt < budget.attempts => {
                tokio::time::sleep(jitter(budget.backoff_min, budget.backoff_max)).await;
                attempt += 1;
            }
            Err(fault) => return Err(fault),
        }
    }
}

/// Runs the tag branch for `run` and says what happened. Never fails, never waits past [`TagBudget::total`].
pub async fn tag_run<S: Store>(
    deps: &TagDeps<'_, S>,
    run: &RunItem,
    excerpt: &Excerpt,
    budget: &TagBudget,
) -> TagReport {
    let mut rep = TagReport::start(run, excerpt, deps.now_ms);
    // A model is used for a purpose only with a passing smoke record for that purpose (root invariant 6).
    if !deps.model.selectable_for(UtilityPurpose::RunTags) {
        rep.end(outcome::SKIPPED, why::NOT_SMOKED);
        return rep;
    }
    if tokio::time::timeout(budget.total, flow(deps, run, excerpt, budget, &mut rep))
        .await
        .is_err()
    {
        // The limit cut the flow. Whatever it wrote stays; the copy of a follow-up is the one write that is known.
        let word = if rep.wrote_inherit {
            outcome::INHERITED
        } else {
            outcome::NONE
        };
        rep.end(word, why::OP_DEADLINE);
    }
    rep
}

async fn flow<S: Store>(
    deps: &TagDeps<'_, S>,
    run: &RunItem,
    excerpt: &Excerpt,
    budget: &TagBudget,
    rep: &mut TagReport,
) {
    let store = deps.store;

    // The marker and the person's removals. A marker ends the branch.
    let marks = match retry_once(budget, || store.tag_state(run)).await {
        Ok(marks) => marks.unwrap_or_default(),
        Err(_) => return rep.end(outcome::NONE, why::STORE_ERROR),
    };
    if marks.at.is_some() {
        return rep.end(outcome::KEPT, why::KEPT);
    }
    let mut off = marks.off;

    // A follow-up: the parent's lists. A parent that cannot be read leaves the person's removals on it unknown, so nothing is written.
    let follow_up = run.continue_from.as_deref();
    let parent = match follow_up {
        None => None,
        Some(_) => match retry_once(budget, || store.parent_tags(run)).await {
            Ok(parent) => parent,
            Err(_) => return rep.end(outcome::NONE, why::STORE_ERROR),
        },
    };
    let inherited = parent.as_ref().map(inherit);
    if let (Some(parent_id), Some(inh)) = (follow_up, &inherited)
        && (!inh.user.is_empty() || !inh.off.is_empty())
    {
        let copy = retry_once(budget, || {
            store.put_run_tags_inherit(run, parent_id, &inh.user, &inh.off)
        })
        .await;
        match copy {
            Ok(true) => rep.wrote_inherit = true,
            Ok(false) => {
                // A person's edit or an earlier copy stands. Read the item again: the person may have removed a tag since the
                // first read, and a removed tag must not take one of the six places.
                match retry_once(budget, || store.tag_state(run)).await {
                    Ok(marks) => {
                        let marks = marks.unwrap_or_default();
                        if marks.at.is_some() {
                            return rep.end(outcome::KEPT, why::KEPT);
                        }
                        off = marks.off;
                    }
                    Err(_) => return rep.end(outcome::NONE, why::STORE_ERROR),
                }
            }
            // The automatic write would bump the revision, and a revision makes the copy impossible for good.
            Err(_) => return rep.end(outcome::NONE, why::STORE_ERROR),
        }
    }
    if let Some(inh) = &inherited {
        off = union(&off, &inh.off);
    }
    let parent_auto: Vec<String> = inherited.map(|i| i.auto).unwrap_or_default();

    // The model: why there is no answer is a reason code; an answer is the judged tags.
    let mut no_answer: &'static str = why::SHORT_PROMPT;
    let mut answer: Option<Vec<String>> = None;
    let mut vocab_ok = true;
    if !is_short(excerpt) {
        let (vocab, ok, facts) = read_vocabulary(deps, run, budget).await;
        vocab_ok = ok;
        rep.vocab = Some(facts);
        let extra = deps
            .model
            .additional_model_request_fields
            .clone()
            .filter(|v| !v.is_null() && v.as_object().is_none_or(|o| !o.is_empty()))
            .unwrap_or(Value::Null);
        match judge::request(&extra, &vocab, &excerpt.escaped) {
            Err(reason) => no_answer = reason.as_str(),
            Ok(body) => {
                rep.route = Some(deps.model.id.clone());
                match ask(deps.client, deps.model, &body, budget, rep).await {
                    Err(fault) => no_answer = fault.code(),
                    Ok(reply) => {
                        rep.reply = Some((
                            reply.stop_reason.clone(),
                            reply.input_tokens,
                            reply.output_tokens,
                        ));
                        match read_reply(&reply.stop_reason, &reply.text) {
                            Err(reason) => no_answer = reason.as_str(),
                            Ok(proposed) => {
                                let judged =
                                    judge_reply(&reply.stop_reason, &reply.text, &vocab, &off);
                                let reused = judged
                                    .kept
                                    .iter()
                                    .filter(|t| vocab.keys.contains_key(&key_of(t)))
                                    .count();
                                let mut rejected: BTreeMap<&'static str, usize> = BTreeMap::new();
                                for reason in &judged.rejects {
                                    *rejected.entry(reason.as_str()).or_default() += 1;
                                }
                                rep.judged = Some(JudgeFacts {
                                    proposed: proposed.len(),
                                    kept: judged.kept.len(),
                                    reused,
                                    new: judged.kept.len() - reused,
                                    rejected,
                                });
                                answer = Some(judged.kept);
                            }
                        }
                    }
                }
            }
        }
    }

    let at = dispatcher::items::rfc3339(deps.now_ms / 1000);
    if let Some(model_tags) = answer {
        // The model answered, even with an empty list: the marker, the route and the version are written.
        let tags = compose_auto(&model_tags, &parent_auto, &off);
        let answered = Answered {
            route: &deps.model.id,
            version: judge::PROMPT_VERSION,
        };
        let stored = retry_once(budget, || {
            store.put_run_tags_auto(run, &tags, &at, Some(answered))
        })
        .await;
        match stored {
            Ok(true) => {
                let reason = if !vocab_ok {
                    why::VOCAB_UNAVAILABLE
                } else if tags.is_empty() {
                    why::NO_TAGS
                } else {
                    why::WRITTEN
                };
                rep.end(outcome::WRITTEN, reason);
            }
            Ok(false) => rep.end(outcome::KEPT, why::KEPT),
            Err(_) => rep.end(outcome::NONE, why::STORE_ERROR),
        }
        return;
    }

    // No model answer. A follow-up still holds its parent's automatic tags; a plain run is left for a later catch-up.
    let tags = compose_auto(&[], &parent_auto, &off);
    if follow_up.is_some() && !tags.is_empty() {
        let stored = retry_once(budget, || store.put_run_tags_auto(run, &tags, &at, None)).await;
        match stored {
            Ok(true) => rep.end(outcome::INHERITED, no_answer),
            Ok(false) => rep.end(outcome::KEPT, why::KEPT),
            Err(_) => rep.end(outcome::NONE, why::STORE_ERROR),
        }
    } else if rep.wrote_inherit {
        rep.end(outcome::INHERITED, no_answer);
    } else if no_answer == why::SHORT_PROMPT {
        rep.end(outcome::SKIPPED, why::SHORT_PROMPT);
    } else {
        rep.end(outcome::NONE, no_answer);
    }
}

/// The vocabulary of the owner's newest runs, within its own limit. A failure or a stall is not an error: the model is asked with
/// `(none yet)` and the report says so.
async fn read_vocabulary<S: Store>(
    deps: &TagDeps<'_, S>,
    run: &RunItem,
    budget: &TagBudget,
) -> (Vocabulary, bool, VocabFacts) {
    match tokio::time::timeout(
        budget.vocab,
        deps.store.tag_vocabulary(run, VOCABULARY_WINDOW),
    )
    .await
    {
        Ok(Ok(read)) => {
            let vocab = Vocabulary::from_items(&read.items);
            let facts = VocabFacts {
                lines: vocab.lines.len(),
                window_runs: vocab.window_runs,
                top_share_bp: vocab.top_share_bp,
                skipped: read.skipped,
            };
            (vocab, true, facts)
        }
        _ => (Vocabulary::default(), false, VocabFacts::default()),
    }
}
