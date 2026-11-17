//! What the end of a run shares with the wire: the closing `turn.ended` payload, the `/internal/turn-complete` body, the
//! `result.json` write, the engine's reaping and the note on an engine whose output closed. The direct driver
//! (`crate::direct`) is the only caller.

use crate::engine::{Exit, StderrCapture, describe_exit, exit_of, kill_group};
use harness_core::engine_diagnostic;
use harness_protocol::api::internal::{
    MAX_TURN_SUMMARY_BYTES, MAX_TURN_SUMMARY_CHARS, TurnCompleteRequest,
};
use harness_protocol::common::Usage;
use harness_protocol::direct::{message_preview, tail_chars};
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

fn outcome_word(o: TurnOutcome) -> &'static str {
    match o {
        TurnOutcome::Completed => "completed",
        TurnOutcome::Failed => "failed",
        TurnOutcome::Cancelled => "cancelled",
        TurnOutcome::TimedOut => "timed_out",
    }
}

/// The payload of a turn's closing `turn.ended` event: the turn, its outcome, the stop reason and the last commit.
pub fn turn_ended_payload(
    turn: u32,
    word: &str,
    stop_reason: Option<&str>,
    last_commit_sha: Option<&str>,
) -> serde_json::Value {
    json!({
        "turn": turn, "outcome": word, "stopReason": stop_reason, "lastCommitSha": last_commit_sha,
    })
}

/// A turn's `summary`: the agent's text with control characters removed (newlines and tabs kept), its last `MAX_TURN_SUMMARY_CHARS` characters and, when
/// those are over `MAX_TURN_SUMMARY_BYTES` bytes of JSON, a preview of them (a start, a marker and an end); `None` when nothing but whitespace is left.
pub fn summary_of(text: &str) -> Option<String> {
    let clean: String = text
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    let last = tail_chars(&clean, MAX_TURN_SUMMARY_CHARS);
    (!last.trim().is_empty()).then(|| message_preview(last, MAX_TURN_SUMMARY_BYTES, ""))
}

/// The `/internal/turn-complete` body for a finished turn; `finishReason` falls back to the outcome.
pub fn turn_complete_request(resp: &InvocationResponse) -> TurnCompleteRequest {
    // The request goes whole into SendTaskSuccess (256 KiB), so the restored paths and the
    // refusals are capped.
    let capped = harness_protocol::api::internal::cap_reverted(&resp.reverted);
    let refused = harness_protocol::api::internal::cap_refused(&resp.refused);
    TurnCompleteRequest {
        summary: resp.summary.clone(),
        run_id: resp.run_id.clone(),
        agent_id: resp.agent_id.clone(),
        turn: resp.turn,
        finish_reason: resp
            .stop_reason
            .clone()
            .unwrap_or_else(|| outcome_word(resp.outcome).to_string()),
        last_commit_sha: resp.last_commit_sha.clone(),
        bundle_uri: resp.bundle_uri.clone(),
        snapshot_uri: None,
        usage: resp.usage,
        outcome: Some(resp.outcome),
        error: resp.error.clone(),
        rules_digest: resp.rules_digest.clone(),
        rules_manifest: resp.rules_manifest.clone(),
        reverted: capped.0,
        reverted_truncated: capped.1,
        refused: refused.0,
        refused_truncated: refused.1,
        fault_hint: resp.fault_hint.clone(),
        max_silent_gap_s: resp.max_silent_gap_s,
        transcript: None,
        handoff: None,
        output_pins: None,
    }
}

/// Writes the result by rename, so a reader sees no file or the whole file.
pub(crate) fn write_result(path: &Path, resp: &InvocationResponse) -> Result<(), String> {
    let dir = path.parent().ok_or("result path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(".result.json.tmp");
    let body = serde_json::to_vec_pretty(resp).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Waits up to `grace` seconds for the engine to exit, then stops its process group (at once when `grace` is 0, so
/// nothing its tools started outlives the turn). What agentd saw of the process.
pub(crate) async fn reap_engine(child: &mut tokio::process::Child, grace: u64) -> Exit {
    let exit = match tokio::time::timeout(Duration::from_secs(grace), child.wait()).await {
        Ok(Ok(status)) => exit_of(status),
        Ok(Err(e)) => Exit::Unknown(e.to_string()),
        Err(_) => Exit::StoppedByAgentd { after_s: grace },
    };
    if grace == 0 || matches!(exit, Exit::StoppedByAgentd { .. }) {
        kill_group(child).await;
    }
    exit
}

/// The two bodies of the diagnostic of an engine whose output closed: for the error text (the exit and one line of its
/// stderr, `engine_diagnostic::error_body`) and for the runtime log line (the exit and the scrubbed tail,
/// `engine_diagnostic::log_body`). `None` when there is nothing to add: the engine exited 0 and said nothing on stderr,
/// which keeps the plain "the engine closed its output" that a clean exit has always had. A log agentd cannot read is
/// said so; it never replaces the turn's error.
fn closed_diagnostic(exit: &Exit, capture: &StderrCapture) -> Option<(String, String)> {
    let words = describe_exit(exit);
    let tail = match capture.tail() {
        // The line the harness's own plugin writes when its hook is in place is not the engine's last words.
        Ok(raw) => engine_diagnostic::scrub(
            &crate::engine::without_lines_starting(
                &raw.bytes,
                crate::engine::config::HARNESS_LINE_PREFIX,
            ),
            raw.cut,
        ),
        Err(e) => {
            let unreadable = engine_diagnostic::unreadable_body(&words, &e.to_string());
            return Some((unreadable.clone(), unreadable));
        }
    };
    if *exit == Exit::Code(0) && tail.is_empty() && !tail.long_line {
        return None;
    }
    Some((
        engine_diagnostic::error_body(&words, &tail),
        engine_diagnostic::log_body(&words, &tail),
    ))
}

/// The short scrubbed note on how an engine whose output closed ended (its exit and one line of its stderr), for a direct
/// run's detail; `None` when there is nothing to add.
pub(crate) fn closed_note(exit: &Exit, capture: &StderrCapture) -> Option<String> {
    closed_diagnostic(exit, capture).map(|(short, _)| short)
}

/// Usage after a `session/prompt` (the first one or a nudge): what the response reported added to the run so far,
/// else what the engine streamed, else still absent (never zeros for an absent report).
pub fn prompt_usage(
    so_far: Option<Usage>,
    reported: Option<Usage>,
    streamed: Option<Usage>,
) -> Option<Usage> {
    match reported {
        Some(u) => Some(add_usage(so_far.unwrap_or_default(), u)),
        None => streamed,
    }
}

fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens + b.input_tokens,
        output_tokens: a.output_tokens + b.output_tokens,
        cache_read_input_tokens: a.cache_read_input_tokens + b.cache_read_input_tokens,
        cache_write_input_tokens: a.cache_write_input_tokens + b.cache_write_input_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::closed_note;
    use crate::engine::{Exit, capture_of};

    fn note(log: &str, exit: &Exit) -> Option<String> {
        // The folder `TMPDIR` names (every run of this project's tests sets it), else the working directory: agentd's sources make no file
        // in a directory that other processes share, and its unit tests keep to that (`tests/sandbox_shared_tmp.rs`).
        let dir = std::env::var_os("TMPDIR")
            .map_or_else(|| std::path::PathBuf::from("."), std::path::PathBuf::from)
            .join(format!(
                "b800-turn-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
        std::fs::create_dir_all(&dir).expect("scratch folder");
        let path = dir.join("engine-stderr.log");
        std::fs::write(&path, log).expect("log");
        closed_note(exit, &capture_of(&path).expect("handle"))
    }

    /// The line the harness's own plugin writes when its hook is in place is not the engine's last words. A log that holds nothing
    /// else reads as the empty log it was before the plugin existed; an engine warning beside it is what the detail shows.
    #[test]
    fn b800_the_ready_line_is_not_the_engines_last_words() {
        let ready = "hamh.shell-limit: ready 600000\n";
        assert_eq!(note(ready, &Exit::Code(0)), None);
        assert_eq!(note(&format!("{ready}{ready}"), &Exit::Code(0)), None);
        let failed = note(ready, &Exit::Code(1)).expect("an exit other than 0 has a note");
        assert!(failed.contains("stderr empty"), "{failed}");
        assert!(!failed.contains("hamh.shell-limit"), "{failed}");
        let warned = note(
            &format!("level=WARN message=\"failed to load plugin\"\n{ready}"),
            &Exit::Code(1),
        )
        .expect("a note");
        assert!(warned.contains("failed to load plugin"), "{warned}");
        assert!(!warned.contains("ready 600000"), "{warned}");
    }
}
