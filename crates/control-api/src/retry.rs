//! `POST /v1/runs/{runId}/retry`: a run that ended `blocked` is started again with the request it was started with.
//!
//! A blocked run ended before it delivered anything, so it has no bundle to follow up from (`continueFrom` answers 409 `no-delivered-head`),
//! and the run rows hold no prompt, rules, switches or attachments for the client to resend. The one full copy of the request is the object
//! `runs/<id>/input/request.json`, so this route reads it, holds it to what a fresh start is held to ([`crate::direct::prepare`]: the model list,
//! the rules, the limits, and for a follow-up its base run and head), and starts a new run whose stored request is the same request: the
//! canonical form of the stored one, which is the very bytes for a request this service stored.
//!
//! The new run's Run item names the blocked run in `continueFrom`, which the run rows and the detail return. That is a link between two runs and
//! nothing more: the stored request keeps its own `continueFrom` (with its resolved head) when the blocked run was itself a follow-up, and
//! carries none when it was an ordinary run, so agentd starts the new run exactly as it started the blocked one. Run-session's tag step reads the
//! Run item's link, so the retry also inherits the tags the person put on the blocked run.
//!
//! The client chooses the `Idempotency-Key`. `ham` sends `retry-<blocked run id>`, so a repeat or a second press returns the run the first retry
//! started, and the terminal app starts one retry per blocked run; if that retry blocks, the retry of the new run is the next. The service holds
//! each key to one run and does not limit the keys a caller may use. The claim compares a hash of the blocked run's id with the request, so the key
//! of one blocked run never returns the run of another, and a key used for a fresh start never returns a retry.
//! Every refusal comes before the claim, so a refused retry leaves nothing behind and the same key works once the cause is gone. A repeat of a
//! retry that succeeded is checked again like a fresh start before its claim is read, so a repeat after the stage dropped the model answers the
//! refusal; the first retry's run stays in the run list.

use crate::direct;
use crate::handler::{
    Blobs, Deps, Execution, Reply, Store, V1Request, error, not_found, owned, unavailable,
    valid_idempotency_key,
};
use crate::ids::sha256_hex;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::s3;

const RETRY: &str = "retry with the same Idempotency-Key";

/// The most bytes of a stored request the route reads. A request this service accepted is at most `MAX_REQUEST_BYTES` (6,225,920) long, the size
/// it has in the Lambda event, which is never less than the size of the stored JSON.
const MAX_STORED_REQUEST_BYTES: u64 = 8 * 1024 * 1024;

/// The status as the API spells it (`usage_limited`).
fn word(s: GoalStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// 422 for a request that is missing, too large, or not a request this build reads. The answer names the run and nothing of the content; the
/// cause (its kind and place, never the text) goes to the log.
fn unreadable(run_id: &str, cause: &str) -> Reply {
    let (reply, _) = error(
        422,
        "stored-request-unreadable",
        &format!(
            "the stored request of run {run_id} cannot be read, so the run cannot be started again; start a new run with the same prompt instead"
        ),
    );
    (reply, vec![format!("retry {run_id}: {cause}")])
}

/// What the claim of a retry compares: the blocked run's id and the request, behind a tag that no fresh start's hash (the request alone) can equal.
fn claim_hash(blocked: &str, canonical: &[u8]) -> String {
    let mut tagged = Vec::with_capacity(canonical.len() + blocked.len() + 8);
    tagged.extend_from_slice(b"retry\0");
    tagged.extend_from_slice(blocked.as_bytes());
    tagged.push(0);
    tagged.extend_from_slice(canonical);
    sha256_hex(&tagged)
}

/// `POST /v1/runs/{runId}/retry`: 201 with the new run (`StartRunResponse`), the same answer to every repeat of the key.
pub(crate) async fn retry<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let Some(key) = req.headers.get("idempotency-key") else {
        return error(
            400,
            "idempotency-key-required",
            "POST /v1/runs/{runId}/retry needs an Idempotency-Key header",
        );
    };
    if !valid_idempotency_key(key) {
        return error(
            400,
            "bad-request",
            "Idempotency-Key must be 1-128 visible ASCII characters",
        );
    }
    // The caller's own run, or "no such run": another caller's run and a missing one answer alike, and nothing is read before this is known.
    let source = match owned(&deps.store, run_id, caller).await {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(&format!("no run {run_id} for this caller")),
        Err(e) => return unavailable(&e, RETRY),
    };
    match source.status {
        GoalStatus::Blocked => {}
        GoalStatus::Active => {
            return error(
                409,
                "run-active",
                &format!("run {run_id} has not ended; a retry starts a blocked run again"),
            );
        }
        other => {
            let hint = match other {
                GoalStatus::Complete => "; a follow-up continues from the code it delivered",
                GoalStatus::Paused | GoalStatus::Stalled => {
                    "; resume continues it from the code it delivered"
                }
                _ => "",
            };
            return error(
                409,
                "run-not-blocked",
                &format!(
                    "run {run_id} is {}; only a blocked run can be retried{hint}",
                    word(other)
                ),
            );
        }
    }
    let object = match deps
        .blobs
        .read_prefix(&s3::request_key(run_id), MAX_STORED_REQUEST_BYTES)
        .await
    {
        Ok(Some(o)) => o,
        Ok(None) => return unreadable(run_id, "the object is absent"),
        // The role's grant, or the store: the caller retries with the same key, which nothing has claimed yet.
        Err(e) => return unavailable(&e, RETRY),
    };
    if object.size > object.bytes.len() as u64 {
        return unreadable(run_id, "the object is larger than the route reads");
    }
    let stored: StartDirectRunRequest = match serde_json::from_slice(&object.bytes) {
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
    // Held to what a fresh start is held to: a model the stage no longer lists, a rule that no longer passes, or a base run that lost its
    // delivered head refuses the retry with the words of a fresh start.
    let prepared = match direct::prepare(stored, caller, deps).await {
        Ok(p) => p,
        Err(reply) => return reply,
    };
    let claim = claim_hash(run_id, &prepared.canonical);
    direct::start_prepared(&prepared, key, claim, Some(run_id), caller, deps).await
}
