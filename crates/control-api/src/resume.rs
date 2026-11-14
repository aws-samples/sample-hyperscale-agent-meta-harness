//! `POST /v1/runs/{runId}/resume`: a run that stopped delivered, `paused` at its total time or `stalled` after it lost its progress,
//! goes on as a follow-up run.
//!
//! The stopped run delivered its work and its session is over, so it cannot take a message and a second delivery under the same run id would
//! overwrite the first. What continues is a new run that starts from the commit the stopped run delivered with
//! the request the stopped run was started with and one more paragraph on its prompt that says it is a continuation.
//! The new run has its own total time, the one the request names. The stopped run keeps its status: it is the record of what was delivered.
//!
//! The client chooses the `Idempotency-Key`. `ham` sends `resume-<stopped run id>`, so a second press, a repeat of `ham resume` or a send that
//! got no answer returns the run the first one started. The claim compares a hash of the stopped run's id with the request, so the key of one
//! run never returns the run of another, and no key of a fresh start or a retry can be mistaken for it. Every refusal comes before the claim.
//!
//! A run that cannot be resumed says why in one sentence: it is still going, it ended `complete` (its label says how the engine stopped; a follow-up
//! with a prompt of the person's own continues it), it is blocked (a retry starts it again), it was cleared, or its stop asks for more than a new
//! run (a budget or a usage limit that still holds).

use crate::direct;
use crate::handler::{
    Blobs, Deps, Execution, Reply, Store, V1Request, error, not_found, owned, unavailable,
    valid_idempotency_key,
};
use crate::ids::sha256_hex;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::ContinueFrom;
use harness_protocol::goal::{GoalStatus, ResumeMode};
use harness_protocol::store::s3;
use harness_protocol::target::RunTarget;

const RETRY: &str = "retry with the same Idempotency-Key";

/// The most bytes of a stored request the route reads (the limit `retry` has).
const MAX_STORED_REQUEST_BYTES: u64 = 8 * 1024 * 1024;

/// The sentence added to the prompt of a resumed run: it says what the run is, where the earlier work is and what to do first. It is the
/// harness's text, added after the person's prompt, which stays as typed.
pub fn continuation_note(stopped_run: &str, reason: Option<&str>) -> String {
    let why = reason
        .map(|r| r.split_once(": ").map_or(r, |(_, sentence)| sentence))
        .map(|s| s.trim().trim_end_matches('.'))
        .filter(|s| !s.is_empty())
        .map(|s| format!(": {s}"))
        .unwrap_or_default();
    format!(
        "\n\n---\nThis is a continuation. An earlier run of this task (run {stopped_run}) stopped before the work was done{why}. Its work is already in this repository. Read the git log and PROGRESS.md if there is one, check what is done, and carry on until the task is complete."
    )
}

/// What the claim of a resume compares: the stopped run's id and the request, behind a tag that no fresh start's or retry's hash can equal.
fn claim_hash(stopped: &str, canonical: &[u8]) -> String {
    let mut tagged = Vec::with_capacity(canonical.len() + stopped.len() + 9);
    tagged.extend_from_slice(b"resume\0");
    tagged.extend_from_slice(stopped.as_bytes());
    tagged.push(0);
    tagged.extend_from_slice(canonical);
    sha256_hex(&tagged)
}

fn word(s: GoalStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The refusal for a status that is not a stop the run made delivered, or `None` for `paused` and `stalled`.
fn status_refusal(run_id: &str, status: GoalStatus) -> Option<Reply> {
    let (code, text) = match status {
        GoalStatus::Paused | GoalStatus::Stalled => return None,
        GoalStatus::Active => (
            "run-active",
            format!("run {run_id} has not ended; there is nothing to resume yet"),
        ),
        GoalStatus::Blocked => (
            "run-not-resumable",
            // A blocked run delivered no zip and its work is kept; the refusal says where.
            format!(
                "run {run_id} is blocked and delivered no zip; a retry starts it again, and `ham pull {run_id} --snapshot` downloads the latest saved copy of its work"
            ),
        ),
        GoalStatus::Complete => (
            "run-not-resumable",
            format!(
                "run {run_id} is complete: the engine stopped it, and its label says how; a follow-up with a prompt of your own continues from the code it delivered"
            ),
        ),
        GoalStatus::Cleared => (
            "run-not-resumable",
            format!(
                "run {run_id} was cleared; it cannot be resumed. `ham pull {run_id} --snapshot` downloads the latest saved copy of its work"
            ),
        ),
        other @ (GoalStatus::UsageLimited | GoalStatus::BudgetLimited) => (
            "run-not-resumable",
            format!(
                "run {run_id} is {}; the limit that stopped it still holds, and a new run would meet it again",
                word(other)
            ),
        ),
    };
    Some(error(409, code, &text))
}

/// `POST /v1/runs/{runId}/resume`: 201 with the new run (`StartRunResponse`), the same answer to every repeat of the key.
pub(crate) async fn resume<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let Some(key) = req.headers.get("idempotency-key") else {
        return error(
            400,
            "idempotency-key-required",
            "POST /v1/runs/{runId}/resume needs an Idempotency-Key header",
        );
    };
    if !valid_idempotency_key(key) {
        return error(
            400,
            "bad-request",
            "Idempotency-Key must be 1-128 visible ASCII characters",
        );
    }
    let source = match owned(&deps.store, run_id, caller).await {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(&format!("no run {run_id} for this caller")),
        Err(e) => return unavailable(&e, RETRY),
    };
    if let Some(refused) = status_refusal(run_id, source.status) {
        return refused;
    }
    if source.resume_mode == Some(ResumeMode::None) {
        return error(
            409,
            "run-not-resumable",
            &format!("run {run_id} stopped without code to continue from"),
        );
    }
    let object = match deps
        .blobs
        .read_prefix(&s3::request_key(run_id), MAX_STORED_REQUEST_BYTES)
        .await
    {
        Ok(Some(o)) => o,
        Ok(None) => return unreadable(run_id, "the object is absent"),
        Err(e) => return unavailable(&e, RETRY),
    };
    if object.size > object.bytes.len() as u64 {
        return unreadable(run_id, "the object is larger than the route reads");
    }
    let mut stored: StartDirectRunRequest = match serde_json::from_slice(&object.bytes) {
        Ok(r) => r,
        Err(e) => {
            return unreadable(
                run_id,
                &format!(
                    "{:?} at line {} column {}",
                    e.classify(),
                    e.line(),
                    e.column()
                ),
            );
        }
    };
    // The continuation: the delivered head of the stopped run is the base, so a branch the person named or a repository the run created are
    // the stopped run's business, not this one's. `control-api` resolves the head (and refuses a run that delivered none) in `prepare`.
    stored.continue_from = Some(ContinueFrom {
        run_id: run_id.to_string(),
        head_sha: None,
    });
    stored.base_ref = None;
    if let RunTarget::NewRepo { repo } = stored.target.clone() {
        stored.target = RunTarget::Repo { repo };
    }
    stored
        .prompt
        .push_str(&continuation_note(run_id, source.status_reason.as_deref()));
    let prepared = match direct::prepare(stored, caller, deps).await {
        Ok(p) => p,
        Err(reply) => return reply,
    };
    let claim = claim_hash(run_id, &prepared.canonical);
    direct::start_prepared(&prepared, key, claim, Some(run_id), caller, deps).await
}

/// 422 for a request that is missing, too large, or not a request this build reads; the answer names the run and nothing of the content.
fn unreadable(run_id: &str, cause: &str) -> Reply {
    let (reply, _) = error(
        422,
        "stored-request-unreadable",
        &format!(
            "the stored request of run {run_id} cannot be read, so the run cannot be resumed; start a new run with the same prompt instead"
        ),
    );
    (reply, vec![format!("resume {run_id}: {cause}")])
}
