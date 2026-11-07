//! The fresh session: a new
//! session in the SAME work tree, started either by agentd (a saved conversation the provider rejects, a full context window, a dead
//! engine) or by the person's answer. The new session sees the files as they are and a harness-written prompt; it does not see the old
//! conversation.
//!
//! This module does the wire part: optionally stop the engine process and start it again ([`Respawn`], used when the engine is dead, hung,
//! or keeps its counters between sessions), `initialize` after a restart, `session/new`, the check that the engine took the user's model
//! (never `session/set_config_option`, never a route exclusion), and the `new-session` event. The prompt and the digest are built by the
//! caller ([`crate::direct::input`]); the first send of the new session is the caller's `prompt_with_retry_from`.

use crate::acp::{AcpClient, AcpError, Activity};
use crate::direct::collector::DirectCollector;
use crate::direct::driver::{
    Fail, Inner, PromptPlan, Respawn, SessionTrigger, init_params, model_option, step,
};
use crate::direct::hooks::{Event, SessionEnd, SessionEnded, SessionStarted};
use crate::time::{format_rfc3339, now_unix};
use harness_protocol::direct::{
    DIRECT_TURN, DirectFreshSession, NewSessionPayload, PROGRESS_NEW_SESSION,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

/// What a fresh session needs to know before it starts.
#[derive(Clone, Debug)]
pub struct FreshSpec {
    /// Stop the engine process and start it again first.
    pub restart: bool,
    /// The harness-written first prompt of the new session.
    pub prompt: String,
    pub trigger: SessionTrigger,
    /// The reason word of the stop that led here (`engine_stop`, `replay_rejected`, ...).
    pub reason: String,
    /// The question this answers; `None` for agentd's own fresh session.
    pub request_id: Option<String>,
    pub digest_bytes: u64,
    pub digest_sha256: String,
}

/// The `new-session` event.
pub fn new_session_event(n: u32, spec: &FreshSpec, prompt_bytes: usize) -> Value {
    let p = NewSessionPayload {
        turn: DIRECT_TURN,
        kind: PROGRESS_NEW_SESSION.to_string(),
        text: format!("Restarted from the work tree: fresh session {n}."),
        request_id: spec.request_id.clone(),
        session: n,
        trigger: spec.trigger.as_str().to_string(),
        reason: spec.reason.clone(),
        restarted_process: spec.restart,
        digest_bytes: spec.digest_bytes,
        prompt_bytes: prompt_bytes as u64,
    };
    serde_json::to_value(&p).unwrap_or(Value::Null)
}

fn could_not_start(stage: &'static str, why: impl std::fmt::Display) -> Box<Fail> {
    Box::new(Fail {
        // Not a setup failure of the run (row 1 of the stop table): the run was going, and the engine is what failed.
        setup: false,
        stage,
        err: AcpError::Protocol(format!("a fresh session could not start ({stage}): {why}")),
        fresh_start: true,
    })
}

/// Starts the new session and returns its id. `inner` learns the session, the model the engine reported, and that the usage of the
/// session before is final. On any failure the stage ends with an error that the stop table reads as a dead engine (rows 16 and 17
/// of the state machine decide what follows).
pub(crate) async fn start<R, W, S>(
    client: &mut AcpClient<R, W>,
    collector: &mut DirectCollector,
    activity: &Arc<Activity>,
    plan: &PromptPlan,
    inner: &mut Inner,
    host: &mut S,
    spec: &FreshSpec,
) -> Result<String, Box<Fail>>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
    S: Respawn<R = R, W = W>,
{
    // What the session before used is final from here on, whatever happens next.
    inner.usage.new_session(collector.usage());
    if spec.restart {
        let (reader, writer) = host
            .respawn()
            .await
            .map_err(|e| could_not_start("restart", e))?;
        client.replace_io(reader, writer);
        // The new process has run nothing: its silence is counted from now, and no tool of the old one is open.
        activity.reset_for_stage();
        step(client, collector, "initialize", "initialize", init_params())
            .await
            .map_err(|f| could_not_start(f.stage, f.err))?;
    }
    // No `_meta`, and never `session/set_config_option`: the model is the one the launch pinned.
    let new = step(
        client,
        collector,
        "session/new",
        "session/new",
        json!({ "cwd": plan.cwd, "mcpServers": [] }),
    )
    .await
    .map_err(|f| could_not_start(f.stage, f.err))?;
    let session_id = new
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| could_not_start("session/new", "no sessionId in the answer"))?
        .to_string();
    let got = model_option(&new).flatten();
    if got.as_deref() != Some(plan.model.as_str()) {
        return Err(could_not_start(
            "model pin",
            format!(
                "the engine took model {} instead of {}",
                got.as_deref().unwrap_or("none"),
                plan.model
            ),
        ));
    }
    let hooks = collector.hooks().clone();
    if let Some(old) = &inner.session_id {
        hooks.event(&Event::SessionEnded(SessionEnded {
            session_id: old,
            number: inner.sessions,
            why: SessionEnd::Replaced,
        }));
    }
    inner.sessions += 1;
    inner.session_id = Some(session_id.clone());
    // The old session may still send events (a prompt that was dropped keeps running until it is cancelled): they are ignored now.
    client.set_session(&session_id);
    collector.set_session(inner.sessions);
    inner.pinned = got;
    // The fresh session begins on the route the user chose, whatever the ladder did before.
    if !inner.failover.switches.is_empty() {
        inner.failover.final_model = Some(plan.model.clone());
    }
    inner.fresh.push(DirectFreshSession {
        n: inner.sessions,
        trigger: spec.trigger.as_str().to_string(),
        reason: spec.reason.clone(),
        request_id: spec.request_id.clone(),
        started_at: format_rfc3339(now_unix()),
        restarted_process: spec.restart,
        digest_bytes: spec.digest_bytes,
        digest_sha256: spec.digest_sha256.clone(),
        at_attempt: u32::try_from(inner.attempts.len() + 1).unwrap_or(u32::MAX),
    });
    hooks.event(&Event::SessionStarted(SessionStarted {
        session_id: &session_id,
        number: inner.sessions,
        response: &new,
        cwd: &plan.cwd,
        trigger: Some(spec.trigger.as_str()),
    }));
    Ok(session_id)
}
