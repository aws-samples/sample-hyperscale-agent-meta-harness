//! `ham watch` in line mode: the run is rehydrated from the API alone,
//! by polling the event shards into `ham_core::state::RunState`. A line is
//! printed when the view changes: the status line (plain) or the view as one JSON object
//! (`--json`), which carries only sanitized text. The watch ends on `run.completed`, `cleared`, or
//! a delivered completion; then the zip is downloaded and the final card printed. A run that is complete with its zip announced but
//! has no `run.completed` yet is read on for [`COMPLETED_SETTLE`] first, so the card carries the outcome label the harness writes
//! right after the zip. Transient read failures are reported on stderr and retried with backoff up to 30 s.

use crate::api::{Api, ApiError};
use crate::commands::now_unix;
use crate::http::ControlPlane;
use crate::pull::{PullError, Pulled, card, download, select};
use ham_core::command::PullArtifact;
use ham_core::exit::exit_code;
use ham_core::poll::PollPlan;
use ham_core::render::{final_card, status_line, wait_text};
use ham_core::state::{RunState, RunView};
use harness_protocol::api::v1::RunDetail;
use harness_protocol::goal::GoalStatus;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// Longest wait between reads while the API keeps failing.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How long the watch keeps reading once the run is complete with its zip announced and the harness's `run.completed` has not come.
/// It waits for that one record, which carries the outcome label. The harness writes `goal.status` complete, `delivery.ready` and
/// `run.completed` in that order from one step, so the label normally follows the zip within a second or two; 30 s is a sanity bound,
/// not a tuned timeout. A run that ends without
/// the record (`cleared`, `run.aborted`) is not waited for.
pub const COMPLETED_SETTLE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchOptions {
    /// 1 s for the run in focus.
    pub interval: Duration,
    pub json: bool,
    /// Where the zip goes on delivery; `None` skips the download.
    pub download_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct WatchOutcome {
    pub view: RunView,
    pub detail: Option<RunDetail>,
    pub pulled: Option<Pulled>,
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error(transparent)]
    Api(ApiError),
    #[error(transparent)]
    Pull(PullError),
    #[error("output: {0}")]
    Io(String),
}

fn out_err(e: std::io::Error) -> WatchError {
    WatchError::Io(e.to_string())
}

/// The run has stopped moving: `run.completed`, `cleared`, or `complete` with its zip announced.
/// A run that is blocked, paused or out of budget is not finished until the harness says the run completed.
pub fn finished(v: &RunView) -> bool {
    v.completed
        || v.status == Some(GoalStatus::Cleared)
        || (v.status == Some(GoalStatus::Complete) && v.zip.is_some())
}

/// The run is [`finished`] but the harness's `run.completed` (the outcome label) has not been read, and it may still come: complete with
/// its zip announced. A cleared or aborted run ends without the record, so it never waits for it.
pub fn awaiting_completed(v: &RunView) -> bool {
    finished(v) && !v.completed && v.status != Some(GoalStatus::Cleared) && v.aborted.is_none()
}

/// The process exit code with `v` on screen: the status's own code (`ham_core::exit`).
pub fn exit_of(v: &RunView) -> i32 {
    exit_code(v.status)
}

fn enum_str<T: serde::Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

/// The view as JSON. Every string in it is harness data or text `RunState` already sanitized.
pub fn view_json(v: &RunView) -> Value {
    let agents: Vec<Value> = v
        .agents
        .values()
        .map(|a| {
            let mut agent = json!({"agentId": ham_core::sanitize::sanitize_line(&a.agent_id), "engine": a.engine,
                   "modelId": a.model_id, "turns": a.turns, "lastSummary": a.last_summary});
            // Only a session that reports its window carries it, so other views are unchanged.
            if let (Some(t), Some(o)) = (a.context_tokens, agent.as_object_mut()) {
                o.insert("contextTokens".into(), json!(t));
            }
            agent
        })
        .collect();
    let mut out = json!({
        "runId": v.run_id,
        "status": v.status.as_ref().map(enum_str),
        "phase": v.phase.as_ref().map(enum_str),
        "prUrl": v.pr_url,
        "zip": v.zip.as_ref().map(|z| json!({"artifactId": z.artifact_id, "sha256": z.sha256, "sizeBytes": z.size_bytes})),
        "agents": agents,
        "blocked": v.blocked.as_ref().map(|b| json!({"reason": b.reason})),
        "untrusted": v.untrusted,
        "completed": v.completed,
    });
    // Only a run the harness has finished carries its label: the harness's own words, as delivered.
    if let Some(o) = out.as_object_mut() {
        insert_result(o, v);
    }
    // Only a run that asked a question or restarted carries them, so every other view is unchanged.
    if let Some(o) = out.as_object_mut() {
        insert_input(o, v);
    }
    // Only a run that ended early carries it, so every other view is unchanged.
    if let (Some(a), Some(o)) = (&v.aborted, out.as_object_mut()) {
        o.insert(
            "aborted".into(),
            json!({"status": a.status, "error": a.error, "cause": a.cause, "stoppedAt": a.stopped_at}),
        );
    }
    // Only a run that was given skills lists them: name, digest, files and bytes, and whether the engine's own list showed them.
    if let (Some(s), Some(o)) = (&v.skills, out.as_object_mut()) {
        let rows: Vec<Value> = s
            .rows
            .iter()
            .map(
                |r| json!({"name": r.name, "sha256": r.digest, "files": r.files, "bytes": r.bytes}),
            )
            .collect();
        o.insert("skills".into(), Value::Array(rows));
        o.insert(
            "skillsPreflight".into(),
            json!({"ran": s.preflight_ms.is_some(), "ms": s.preflight_ms}),
        );
    }
    // Only a run that was given engine settings lists them (the configuration program): the engine, the settings hash and each placed key.
    if let (Some(c), Some(o)) = (&v.engine_config, out.as_object_mut()) {
        let rows: Vec<Value> = c
            .rows
            .iter()
            .map(|r| json!({"key": r.key, "value": r.value}))
            .collect();
        o.insert(
            "engineConfig".into(),
            json!({"engine": c.engine, "sha256": c.sha256, "keys": c.keys, "settings": rows}),
        );
    }
    // Each model's effective window, only while a session reports one.
    let windows: serde_json::Map<String, Value> = v
        .windows()
        .into_iter()
        .map(|(m, t)| (m, json!(t)))
        .collect();
    if let (false, Some(o)) = (windows.is_empty(), out.as_object_mut()) {
        o.insert("windows".into(), Value::Object(windows));
    }
    out
}

/// The question a run waits on, the last that ended, and the sessions of the run, for a view that has any.
fn insert_input(o: &mut serde_json::Map<String, Value>, v: &RunView) {
    if !v.input.is_empty() {
        let open = v.input.open.as_ref().map(|q| {
            json!({"requestId": q.request_id, "n": q.n, "reason": q.reason, "title": q.title, "blocking": q.blocking,
                   "options": q.options, "models": q.models, "canContinue": q.can_continue, "waitS": q.wait_s,
                   "expiresAt": q.expires_at, "lastMessage": q.last_message,
                   "files": q.files, "filesTotal": q.files_total, "evidence": q.evidence})
        });
        let history: Vec<Value> = v
            .input
            .history
            .iter()
            .map(|h| json!({"requestId": h.request_id, "n": h.n, "reason": h.reason, "how": h.how, "ts": h.ts}))
            .collect();
        o.insert("input".into(), json!({"open": open, "history": history}));
    }
    if v.sessions > 1 {
        o.insert("sessions".into(), json!(v.sessions));
    }
}

/// A direct run's own fields: `model` and `modelId` once the session has started (the name the model list gives the id, and the id as
/// the harness wrote it), and `outcome`, `label` and `detail` once its `run.completed` is read, as the harness wrote them; nothing
/// for any other run.
pub fn insert_result(o: &mut serde_json::Map<String, Value>, v: &RunView) {
    if let Some(id) = v.direct_model_id() {
        o.insert("model".into(), json!(ham_core::render::model_name(id)));
        o.insert("modelId".into(), json!(id));
    }
    if let Some(r) = &v.result {
        o.insert("outcome".into(), json!(r.outcome));
        o.insert("label".into(), json!(r.label));
        o.insert("detail".into(), json!(r.detail));
    }
}

/// A downloaded artifact as JSON (P16: path, SHA-256, URL and its true expiry).
pub fn pulled_json(p: &Pulled) -> Value {
    json!({
        "path": p.path.display().to_string(),
        "artifactId": p.artifact.artifact_id,
        "sha256": p.artifact.sha256,
        "sizeBytes": p.artifact.size_bytes,
        "url": p.url.url,
        "expiresAt": p.url.expires_at,
        "resumedFrom": p.resumed_from,
        "ranges": p.ranges,
    })
}

/// The zip `delivery.ready` announced, downloaded into `dir`. Its SHA-256 must equal the one in
/// the harness event as well as the artifact item.
pub async fn download_delivery<P: ControlPlane>(
    api: &Api<P>,
    view: &RunView,
    dir: &std::path::Path,
) -> Result<Option<Pulled>, PullError> {
    let Some(zip) = &view.zip else {
        return Ok(None);
    };
    let list = api.list_artifacts(&view.run_id).await?;
    let artifact = match list.iter().find(|a| a.artifact_id == zip.artifact_id) {
        Some(a) => a.clone(),
        None => select(&list, &PullArtifact::Zip)?,
    };
    if artifact.sha256 != zip.sha256 {
        return Err(PullError::NoArtifact(format!(
            "artifact {} has SHA-256 {}, but delivery.ready announced {}",
            artifact.artifact_id, artifact.sha256, zip.sha256
        )));
    }
    download(api, &view.run_id, &artifact, dir).await.map(Some)
}

/// What `ham watch` has already told the user about the run's questions and sessions (line mode).
#[derive(Debug, Default)]
pub struct InputMemo {
    shown: std::collections::BTreeSet<String>,
    closed: std::collections::BTreeSet<String>,
    sessions: u32,
}

impl InputMemo {
    pub fn new() -> Self {
        Self {
            sessions: 1,
            ..Self::default()
        }
    }

    /// The lines to print now for what is new in `v`: a question that opened (its card and the commands that answer it), a question
    /// this watch saw open that has ended, and a fresh session.
    pub fn lines(&mut self, v: &RunView) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(q) = &v.input.open
            && self.shown.insert(q.request_id.clone())
        {
            let head = if q.blocking {
                "Cline needs you:"
            } else {
                "Notice:"
            };
            out.push(format!("{head} {}", q.title.trim_end_matches('.')));
            out.extend(q.card_lines().into_iter().skip(1).map(|l| format!("  {l}")));
            if let Some(left) = q.left_text() {
                out.push(format!("  {left}"));
            }
            if q.blocking {
                out.push("  Answer with one of:".to_string());
                out.extend(
                    q.commands(&v.run_id)
                        .into_iter()
                        .map(|c| format!("    {c}")),
                );
            } else {
                out.push(format!(
                    "  No answer is needed. ham cancel {} stops the run and delivers what exists.",
                    v.run_id
                ));
            }
        }
        // The card of a goal-loop brake: the run is over, so it names the follow-up command and not a question to answer.
        if let Some(b) = &v.goal_brake
            && self.shown.insert(b.request_id.clone())
        {
            out.push("Goal loop stopped by the harness:".to_string());
            out.extend(b.lines(&v.run_id, 76).into_iter().map(|l| format!("  {l}")));
        }
        // The skills the run was given: their names, short digests and sizes, once.
        if let Some(sk) = &v.skills
            && self.shown.insert(sk.request_id.clone())
        {
            out.push("Skills placed for this run:".to_string());
            out.extend(sk.lines(76).into_iter().map(|l| format!("  {l}")));
        }
        // The engine settings placed for the run (the configuration program): the hash and each key, once.
        if let Some(c) = &v.engine_config
            && self.shown.insert(c.request_id.clone())
        {
            out.push(format!(
                "Engine settings placed for this run ({}, settings hash {}):",
                c.engine,
                c.short_hash()
            ));
            out.extend(c.lines(76).into_iter().map(|l| format!("  {l}")));
        }
        for h in &v.input.history {
            if self.shown.contains(&h.request_id) && self.closed.insert(h.request_id.clone()) {
                out.push(format!("Question {} closed: {}.", h.n, h.phrase));
            }
        }
        if v.sessions > self.sessions {
            self.sessions = v.sessions;
            if let Some(r) = &v.restarted {
                out.push(r.watch_line());
            }
        }
        out
    }
}

/// The line on stderr when the wait for `run.completed` ran out.
fn note_label_missing<E: Write>(err: &mut E, settle: Duration) -> Result<(), WatchError> {
    writeln!(
        err,
        "ham: run.completed did not arrive within {}; the card has no outcome label",
        wait_text(settle)
    )
    .map_err(out_err)
}

/// Watches `run_id` until it is [`finished`] and has its label (or [`COMPLETED_SETTLE`] has passed without it), printing to `out`,
/// reporting retries on `err`.
pub async fn watch<P: ControlPlane, O: Write, E: Write>(
    api: &Api<P>,
    run_id: &str,
    opts: &WatchOptions,
    out: &mut O,
    err: &mut E,
) -> Result<WatchOutcome, WatchError> {
    watch_settled(api, run_id, opts, COMPLETED_SETTLE, out, err).await
}

/// [`watch`] with the bound of the wait for `run.completed` given: the time from the first read that shows the run complete with its zip
/// announced. When it passes without the record, the watch ends, `err` says so and the card has an Outcome line that says so.
pub async fn watch_settled<P: ControlPlane, O: Write, E: Write>(
    api: &Api<P>,
    run_id: &str,
    opts: &WatchOptions,
    settle: Duration,
    out: &mut O,
    err: &mut E,
) -> Result<WatchOutcome, WatchError> {
    // The owner check: another caller's run, or no run, is a 404 here.
    api.get_run(run_id).await.map_err(WatchError::Api)?;
    let mut state = RunState::new(run_id);
    let mut last: Option<String> = None;
    let mut last_blocked: Option<String> = None;
    let mut input_memo = InputMemo::new();
    let mut aborted_shown = false;
    let mut failures: u32 = 0;
    // Adaptive polling (client polish B2): every shard on the first cycle, then the shards that matter, and a longer wait while
    // nothing arrives.
    let mut plan = PollPlan::new(opts.interval);
    let began = std::time::Instant::now();
    // Set at the first read that shows the run complete with its zip announced and no `run.completed`.
    let mut settling: Option<std::time::Instant> = None;
    // Set when the wait for `run.completed` ran out.
    let mut label_missing_after: Option<Duration> = None;
    let view = loop {
        // After a failed read during the wait for `run.completed` the loop goes on with the view it has, so the bound still counts.
        let mut pause = None;
        let polled = match settling {
            None => api.poll_with(&mut state, &mut plan, began.elapsed()).await,
            // During the wait the read itself is held to the bound too: a slow read cannot stretch it.
            Some(since) => {
                let left = settle.saturating_sub(since.elapsed());
                let read = api.poll_with(&mut state, &mut plan, began.elapsed());
                match tokio::time::timeout(left, read).await {
                    Ok(r) => r,
                    Err(_) => {
                        label_missing_after = Some(settle);
                        note_label_missing(err, settle)?;
                        break state.view();
                    }
                }
            }
        };
        match polled {
            Ok(_) => failures = 0,
            Err(e) if e.is_retryable() => {
                failures += 1;
                let wait = opts
                    .interval
                    .saturating_mul(2u32.saturating_pow(failures.min(8)))
                    .min(MAX_BACKOFF);
                writeln!(
                    err,
                    "ham: reading events: {e}; retrying in {} s",
                    wait.as_secs_f32()
                )
                .map_err(out_err)?;
                if settling.is_none() {
                    tokio::time::sleep(wait).await;
                    continue;
                }
                pause = Some(wait);
            }
            Err(e) => return Err(WatchError::Api(e)),
        }
        let view = state.view_at(Some(now_unix()));
        let line = if opts.json {
            json!({ "run": view_json(&view) }).to_string()
        } else {
            status_line(&view)
        };
        if last.as_deref() != Some(line.as_str()) {
            writeln!(out, "{line}").map_err(out_err)?;
            last = Some(line);
        }
        if !opts.json
            && let Some(b) = &view.blocked
            && last_blocked.as_deref() != Some(b.reason.as_str())
        {
            writeln!(out, "blocked: {}", b.reason).map_err(out_err)?;
            last_blocked = Some(b.reason.clone());
        }
        if !opts.json {
            for l in input_memo.lines(&view) {
                writeln!(out, "{}", l).map_err(out_err)?;
            }
        }
        if !opts.json
            && !aborted_shown
            && let Some(a) = &view.aborted
        {
            writeln!(out, "ended early: {}", a.line()).map_err(out_err)?;
            aborted_shown = true;
        }
        if finished(&view) {
            if !awaiting_completed(&view) {
                break view;
            }
            let since = *settling.get_or_insert_with(std::time::Instant::now);
            let left = settle.saturating_sub(since.elapsed());
            if left.is_zero() {
                label_missing_after = Some(settle);
                note_label_missing(err, settle)?;
                break view;
            }
            // The harness is about to write the label: read at the base interval, not the grown one, and never past the bound.
            tokio::time::sleep(pause.unwrap_or(opts.interval).min(left)).await;
            continue;
        }
        tokio::time::sleep(plan.interval()).await;
    };
    let pulled = match &opts.download_dir {
        Some(dir) => download_delivery(api, &view, dir)
            .await
            .map_err(WatchError::Pull)?,
        None => None,
    };
    let detail = api.get_run(run_id).await.ok();
    // The `Tests` line of the clean-checkout test report in the zip that was just downloaded.
    let mut c = crate::report::with_tests(card(detail.as_ref(), pulled.as_ref()), pulled.as_ref());
    // The flow the run declared, from the same zip.
    c = crate::flow::with_flow(c, pulled.as_ref());
    c.label_missing_after = label_missing_after;
    if opts.json {
        let mut last = json!({
            "run": view_json(&view),
            "exitCode": exit_of(&view),
            "download": pulled.as_ref().map(pulled_json),
        });
        // The usage the stage reported; a run it reported none for has neither member (never a fake 0).
        if let (Some(u), Some(o)) = (c.usage, last.as_object_mut()) {
            o.insert("tokens".into(), json!(u.tokens));
            o.insert("usd".into(), json!(u.usd));
        }
        writeln!(out, "{last}").map_err(out_err)?;
    } else if view.status == Some(GoalStatus::Complete)
        || view.pr_url.is_some()
        || view.zip.is_some()
    {
        writeln!(out, "{}", final_card(&view, &c)).map_err(out_err)?;
    }
    Ok(WatchOutcome {
        view,
        detail,
        pulled,
    })
}
