//! `summary.json`: built from the classification and the facts of the session. The builder
//! cuts every string to its limit and keeps the file under 64 KiB, so the summary is always valid against
//! `DirectSummary::validate`; a part that was not measured is absent (or `null` where the contract says null), never a
//! zero standing in for it.

use crate::direct::classify::Classified;
use harness_protocol::common::{Engine, Usage};
use harness_protocol::direct::{
    DIRECT_AGENT_ID, DIRECT_TURN, DirectAttempt, DirectFailoverNote, DirectFreshSession, DirectGit,
    DirectIntervention, DirectModelSpec, DirectNudge, DirectObservation, DirectPackages,
    DirectRequestRef, DirectRuleRef, DirectRulesPreflight, DirectSnapshots, DirectSummary,
    DirectSummaryEngine, DirectSummaryModel, DirectSwitch, DirectTiming, DirectVersions,
    HARNESS_STOP_PREFIX, MAX_CLINE_MESSAGE_CHARS, MAX_DIRECT_ATTEMPTS, MAX_DIRECT_DETAIL_CHARS,
    MAX_DIRECT_INTERVENTIONS, MAX_DIRECT_OBSERVATIONS, MAX_DIRECT_SUMMARY_BYTES,
    MAX_DIRECT_SWITCHES, SUMMARY_SCHEMA_VERSION, head_chars, tail_chars,
};

/// Everything `summary.json` records.
#[derive(Clone, Debug)]
pub struct SummaryInputs {
    pub run_id: String,
    pub classified: Classified,
    pub stop_reason: Option<String>,
    /// The final message; blank is recorded as `null`.
    pub message: Option<String>,
    pub model: DirectModelSpec,
    /// The model id the engine reported back at `session/new`.
    pub pinned: Option<String>,
    pub engine_name: String,
    pub engine_version: Option<String>,
    pub engine_binary_sha256: Option<String>,
    /// The `--thinking` level the engine was launched with, when there was one (protocol 1.47.0).
    pub reasoning_effort: Option<String>,
    /// Where that level came from: `default`, `explicit` or `off`; `None` when the launch asked nothing of the engine (protocol 1.47.0).
    pub reasoning_source: Option<String>,
    /// Who the engine ran as (`nobody`, `agentd` or the engine user's name); `None` when the run never got that far.
    pub engine_identity: Option<String>,
    pub rules: Vec<DirectRuleRef>,
    pub rules_preflight: DirectRulesPreflight,
    pub request_sha256: String,
    pub prompt_bytes: u64,
    pub nudge: DirectNudge,
    pub usage: Option<Usage>,
    pub tool_calls: u64,
    pub timing: DirectTiming,
    pub git: DirectGit,
    pub snapshots: DirectSnapshots,
    pub packages: DirectPackages,
    pub error: Option<String>,
    /// Every try of the prompt; listed only when there was more than one.
    pub attempts: Vec<DirectAttempt>,
    /// Every confirmed switch of route or model.
    pub switches: Vec<DirectSwitch>,
    /// The model the run ended on, once it switched.
    pub final_model: Option<String>,
    /// What failover used and did; `Some` for a run that had failover routes.
    pub failover: Option<DirectFailoverNote>,
    /// Seconds every route had been down when the run ended (the deadline or the user's stop came during the wait).
    pub outage_s: Option<u64>,
}

/// What the run said about the nudge file: `max_nudges` is `None` when the request carried no nudge settings. A nudge with
/// a count of 0 is requested and not honored.
pub fn nudge_note(max_nudges: Option<u32>) -> DirectNudge {
    DirectNudge {
        requested: max_nudges.is_some(),
        honored: max_nudges.is_some_and(|n| n > 0),
    }
}

fn cut(s: Option<String>) -> Option<String> {
    s.map(|t| head_chars(&t, MAX_DIRECT_DETAIL_CHARS).to_string())
        .filter(|t| !t.is_empty())
}

/// The tries the summary lists: none for a run with one try, at most [`MAX_DIRECT_ATTEMPTS`], each detail cut to its limit.
fn attempts_listed(mut attempts: Vec<DirectAttempt>) -> Vec<DirectAttempt> {
    if attempts.len() < 2 {
        return Vec::new();
    }
    if attempts.len() > MAX_DIRECT_ATTEMPTS {
        // The first tries and the last ones tell the story; the middle is cut.
        let tail = MAX_DIRECT_ATTEMPTS / 2;
        let head = MAX_DIRECT_ATTEMPTS - tail;
        let last = attempts.split_off(attempts.len() - tail);
        attempts.truncate(head);
        attempts.extend(last);
    }
    for a in &mut attempts {
        a.detail = cut(a.detail.take());
    }
    attempts
}

/// The switches the summary lists: at most [`MAX_DIRECT_SWITCHES`], each detail cut to its limit.
fn switches_listed(mut switches: Vec<DirectSwitch>) -> Vec<DirectSwitch> {
    switches.truncate(MAX_DIRECT_SWITCHES);
    for w in &mut switches {
        w.detail = cut(w.detail.take());
    }
    switches
}

/// How a run's last stop followed a wait for the user. Each leads the `detail` of the
/// outcome as one plain sentence, after `Ran on ...` and `Every route was down ...` and before the classifier's cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputNote {
    /// The user chose to finish.
    ChoseFinish,
    /// Nobody answered; the window ran out after `waited_s` seconds.
    NobodyAnswered { waited_s: u64 },
    /// A fresh session could not start and no question was left to ask.
    FreshSessionFailed,
}

/// The sentence of `note`: `You chose to finish.`, `Nobody answered in <n> min, so the run finished with what exists.` (n is the
/// waited time rounded to minutes, at least 1) or `A fresh session could not start.`
pub fn input_sentence(note: &InputNote) -> String {
    match note {
        InputNote::ChoseFinish => "You chose to finish.".to_string(),
        InputNote::NobodyAnswered { waited_s } => {
            let minutes = (waited_s.saturating_add(30) / 60).max(1);
            format!("Nobody answered in {minutes} min, so the run finished with what exists.")
        }
        InputNote::FreshSessionFailed => "A fresh session could not start.".to_string(),
    }
}

/// Token usage of a whole run: the sum of the usage of every prompt that answered, across all stages and all engine sessions (the
/// engine reports usage per prompt, as the pinned engine's patch series makes it: the difference of the session's accumulated usage before and after the
/// turn, so a fresh session's first prompt starts again from its own zero). A prompt that ended in an error has no response and no usage;
/// the caller may pass what the engine streamed for it instead. Each prompt is counted once: `prompt` is any number that is unique for a
/// send over the whole run (the attempt's `n`), so a late response of a prompt that was dropped is not added a second time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageSum {
    seen: std::collections::BTreeSet<u32>,
    sum: Option<Usage>,
}

impl UsageSum {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the usage of prompt `prompt` (`None`: the engine reported none for it). A prompt already added is ignored, whatever it carries.
    pub fn add_prompt(&mut self, prompt: u32, usage: Option<Usage>) {
        if !self.seen.insert(prompt) {
            return;
        }
        if let Some(u) = usage {
            self.sum = Some(self.sum.map_or(u, |so_far| add_usage(so_far, u)));
        }
    }

    /// The sum so far; `None` only when no prompt reported any: a total of zeros never stands for "not measured".
    pub fn total(&self) -> Option<Usage> {
        self.sum
    }
}

/// The sum of two usages; a counter that would overflow stops at the largest value.
pub(crate) fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens.saturating_add(b.input_tokens),
        output_tokens: a.output_tokens.saturating_add(b.output_tokens),
        cache_read_input_tokens: a
            .cache_read_input_tokens
            .saturating_add(b.cache_read_input_tokens),
        cache_write_input_tokens: a
            .cache_write_input_tokens
            .saturating_add(b.cache_write_input_tokens),
    }
}

/// The last [`MAX_DIRECT_INTERVENTIONS`] of `items`, in order: what happened last tells the story of a run that went on for long.
fn last_listed<T>(mut items: Vec<T>) -> Vec<T> {
    if items.len() > MAX_DIRECT_INTERVENTIONS {
        items.drain(..items.len() - MAX_DIRECT_INTERVENTIONS);
    }
    items
}

/// What the correction path adds to `summary.json` (protocol 1.37.0): the questions the run raised, its fresh sessions and how its last
/// stop followed a wait. `SummaryExtras::default()` is a run that never waited and never restarted.
#[derive(Clone, Debug, Default)]
pub struct SummaryExtras {
    pub interventions: Vec<DirectIntervention>,
    pub fresh_sessions: Vec<DirectFreshSession>,
    /// Lead sentences of the detail, in this order, after `Ran on ...` and `Every route was down ...` and before the classifier's cause.
    pub input_notes: Vec<InputNote>,
    /// Facts the harness read at the end of the run (the last entry of the run log when it is red), at most [`MAX_DIRECT_OBSERVATIONS`].
    pub observations: Vec<DirectObservation>,
}

/// The sentence the card shows when the run switched: `Ran on <model> after <n> switch(es).` It leads the detail of the outcome.
pub fn ran_on_sentence(final_model: &str, switches: usize) -> String {
    format!(
        "Ran on {final_model} after {switches} switch{}.",
        if switches == 1 { "" } else { "es" }
    )
}

/// Gives a built summary the classification of an end that turned out to be a conflict on an end-of-run object (`classify::upload_conflict`),
/// and the final error text that goes with it. The outcome, the telemetry outcome, the detail, the label (always `label_for(detail)`, which
/// `DirectSummary::validate` checks), the fault hint and the error change; every other member stays as it was built. agentd writes this
/// summary over a foreign `summary.json`, so what run-session reads under that key names the conflict.
pub fn reclassify(summary: &mut DirectSummary, classified: &Classified, error: Option<String>) {
    summary.outcome = classified.outcome;
    summary.turn_outcome = classified.turn_outcome;
    summary.detail = cut(classified.detail.clone());
    let engine = Engine::new(&summary.engine.name).unwrap_or(Engine::DEFAULT);
    summary.label = classified
        .outcome
        .label_for_engine(engine, summary.detail.as_deref());
    summary.fault_hint = classified.fault_hint.clone();
    summary.error = cut(error);
}

/// The summary of a finished (or failed) session that never waited for the user and never restarted.
pub fn build_summary(i: SummaryInputs) -> DirectSummary {
    build_summary_with(i, SummaryExtras::default())
}

/// The summary of a finished (or failed) session, with what the correction path adds (see [`SummaryExtras`]).
pub fn build_summary_with(i: SummaryInputs, extra: SummaryExtras) -> DirectSummary {
    let outcome = i.classified.outcome;
    // The label is the sentence of the engine that ran; the name comes from the run's own request.
    let engine = Engine::new(&i.engine_name).unwrap_or(Engine::DEFAULT);
    let switched = match (&i.final_model, i.switches.len()) {
        (Some(model), n) if n > 0 => Some(ran_on_sentence(model, n)),
        _ => None,
    };
    let down = i
        .outage_s
        .map(|s| format!("Every route was down for the last {s} s."));
    let waited = extra.input_notes.iter().map(input_sentence);
    let mut detail = [switched, down]
        .into_iter()
        .flatten()
        .chain(waited)
        .collect::<Vec<_>>();
    // A stop by the harness leads its detail, so that the label (`DirectOutcome::label_for`) can name it whatever else the run went through.
    match i.classified.detail.clone() {
        Some(d) if d.starts_with(HARNESS_STOP_PREFIX) => detail.insert(0, d),
        Some(d) => detail.push(d),
        None => {}
    }
    let detail = (!detail.is_empty()).then(|| detail.join(" "));
    let message = i
        .message
        .filter(|m| !m.trim().is_empty())
        .map(|m| tail_chars(&m, MAX_CLINE_MESSAGE_CHARS).to_string());
    let mut summary = DirectSummary {
        schema_version: SUMMARY_SCHEMA_VERSION,
        run_id: i.run_id,
        agent_id: DIRECT_AGENT_ID.to_string(),
        turn: DIRECT_TURN,
        outcome,
        label: outcome.label_for_engine(engine, cut(detail.clone()).as_deref()),
        detail: cut(detail),
        stop_reason: i.stop_reason,
        turn_outcome: i.classified.turn_outcome,
        cline_message: message,
        error: cut(i.error),
        model: DirectSummaryModel {
            id: i.model.id,
            region: i.model.region,
            pinned: i.pinned,
        },
        engine: DirectSummaryEngine {
            name: i.engine_name,
            version: i.engine_version,
            binary_sha256: i.engine_binary_sha256,
            reasoning_effort: i.reasoning_effort,
            reasoning_source: i.reasoning_source,
            identity: i.engine_identity,
        },
        rules: i.rules,
        rules_preflight: i.rules_preflight,
        request: DirectRequestRef {
            sha256: i.request_sha256,
            prompt_bytes: i.prompt_bytes,
        },
        nudge: i.nudge,
        usage: i.usage,
        tool_calls: i.tool_calls,
        timing: i.timing,
        git: i.git,
        snapshots: i.snapshots,
        packages: i.packages,
        fault_hint: i.classified.fault_hint,
        versions: DirectVersions {
            agentd: env!("CARGO_PKG_VERSION").to_string(),
            protocol: harness_protocol::PROTOCOL_VERSION.to_string(),
        },
        attempts: attempts_listed(i.attempts),
        final_model: if i.switches.is_empty() {
            None
        } else {
            i.final_model
        },
        switches: switches_listed(i.switches),
        failover: i.failover,
        interventions: last_listed(extra.interventions),
        fresh_sessions: last_listed(extra.fresh_sessions),
        skills: vec![],
        observations: extra
            .observations
            .into_iter()
            .filter(DirectObservation::is_valid)
            .take(MAX_DIRECT_OBSERVATIONS)
            .collect(),
        research: None,
        // The module that places the user's engine settings fills it through the `on_summary` hook; a run with none has none.
        engine_config: None,
        // The tools module fills it through the `on_summary` hook; a run on a stage or a profile without the layer has none.
        tools: None,
        // The driver's record of the review round is set by the end sequence (`direct/mod.rs`) once the summary is built.
        review: None,
        // The flow the run declared is read from the commits by the end sequence (`direct/mod.rs`) once the summary is built.
        flow: None,
    };
    fit_summary(&mut summary);
    summary
}

/// Cuts `summary` until the file is at most [`MAX_DIRECT_SUMMARY_BYTES`]. The message is the one part with no size of its own: it is halved first.
/// If the file is still too large, the two lists that grow with what the user sent are cut from their ends, skills before rules.
/// `build_summary_with` calls it, and the end sequence calls it again after the modules added their members.
pub fn fit_summary(summary: &mut DirectSummary) {
    let too_big =
        |s: &DirectSummary| serde_json::to_vec(s).is_ok_and(|b| b.len() > MAX_DIRECT_SUMMARY_BYTES);
    while too_big(summary) {
        let Some(m) = summary.cline_message.take() else {
            break;
        };
        let keep = m.chars().count() / 2;
        summary.cline_message = (keep > 0).then(|| tail_chars(&m, keep).to_string());
    }
    while too_big(summary) && summary.skills.pop().is_some() {}
    while too_big(summary) && summary.rules.pop().is_some() {}
}
