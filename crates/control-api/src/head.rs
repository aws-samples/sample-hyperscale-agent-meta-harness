//! `GET /v1/runs/{runId}/head`: the commit a finished run delivered, for the follow-up's box title and
//! confirmation, its start key and its digest. The wire type is `ham_core::head::DeliveredHead` (a field in `harness-protocol` would change
//! the worker image key).
//!
//! The head is the one thing the route must find, and it comes from one place: the first 8 KiB of `runs/<run>/deliverable/verified.bundle`,
//! the bundle the validator wrote, whose header names `refs/heads/harness/<runId>`. `continueFrom` takes the same fact from the same bytes.
//! Whether the bundle exists is asked by a listing by its key (the role may list the bucket for that one key; a GET of an absent key is
//! 403 for it and would look like an outage). Everything else is optional and never hides the head:
//!
//! - the engine's last message, out of `evidence/direct.json` (run-session `plan-delivery` writes it, and the message is its `summary.clineMessage`:
//!   `ham_core::head::cline_message_in_evidence`; the message is the engine's text, so it is cleaned and cut again here, and it is never logged);
//! - the validator's record of what the delivery changed, `deliverable/changes.json`, which only counts when it names this run and this
//!   head. Any trouble with it (no object, a refused read, a record of another head, a malformed one) makes the state `unavailable`, and a
//!   diagnostic names the cause without the content.
//!
//! The route reads kilobytes, never the bundle, and writes nothing.

use crate::handler::{
    Blobs, Deps, Execution, Reply, Store, error, json, not_found, owned, unavailable,
};
use ham_core::head::{
    ChangeList, Changes, DeliveredHead, MAX_CHANGES_RECORD_BYTES, changes_key,
    cline_message_in_evidence,
};
use ham_core::redact::redact_tokens;
use ham_core::sanitize::display_text;
use ham_core::snapshot::{HEADER_BYTES, bundle_head_of};
use harness_protocol::direct::{MAX_CLINE_MESSAGE_CHARS, s3 as direct_s3};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::s3;

/// The most bytes of `evidence/direct.json` read: the whole `summary.json` it holds (at most 1 MiB, the engine's message of at most 200,000
/// characters among it) and the rest of the envelope fit inside. The route reads the object into a Lambda function with 256 MB.
const MAX_EVIDENCE_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) async fn get_head<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let run = match owned(&deps.store, run_id, caller).await {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(&format!("no run {run_id} for this caller")),
        Err(e) => return unavailable(&e, "retry"),
    };
    if run.status == GoalStatus::Active {
        return error(
            409,
            "run-active",
            &format!("run {run_id} has not finished, so it has delivered no commit yet"),
        );
    }
    let key = s3::verified_bundle_key(run_id);
    let no_head = || {
        error(
            409,
            "no-delivered-head",
            &format!("run {run_id} delivered no code to continue from"),
        )
    };
    match deps.blobs.exists(&key).await {
        Ok(true) => {}
        Ok(false) => return no_head(),
        Err(e) => return unavailable(&e, "retry"),
    }
    let object = match deps.blobs.read_prefix(&key, HEADER_BYTES).await {
        Ok(Some(o)) => o,
        // Removed between the listing and the read.
        Ok(None) => return no_head(),
        Err(e) => return unavailable(&e, "retry"),
    };
    let head = match bundle_head_of(&object.bytes, &format!("refs/heads/harness/{run_id}")) {
        Ok(h) => h,
        Err(e) => {
            // The cause is for the log; the answer says only that the bundle cannot be read as a head.
            let (reply, _) = error(
                422,
                "delivered-head-unreadable",
                &format!("the delivered bundle of run {run_id} cannot be read as a head"),
            );
            return (reply, vec![format!("head of run {run_id}: {e}")]);
        }
    };
    let mut notes: Vec<String> = Vec::new();
    let cline_message = cline_message(run_id, deps, &mut notes).await;
    let changes = changes(run_id, &head, deps, &mut notes).await;
    let (reply, _) = json(
        200,
        &DeliveredHead {
            run_id: run_id.to_string(),
            head_sha: head,
            run_status: run.status,
            cline_message,
            changes,
        },
    );
    (reply, notes)
}

/// The engine's last message from the evidence the harness wrote, cleaned and cut. `None` when there is none, and a note says why when the
/// evidence could not be used (never a byte of it).
async fn cline_message<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    deps: &Deps<S, B, E>,
    notes: &mut Vec<String>,
) -> Option<String> {
    let key = direct_s3::evidence_key(run_id);
    let object = match deps.blobs.read_prefix(&key, MAX_EVIDENCE_BYTES).await {
        Ok(Some(o)) => o,
        Ok(None) => return None,
        Err(e) => {
            notes.push(format!("head of run {run_id}: evidence not read: {e}"));
            return None;
        }
    };
    if object.size > MAX_EVIDENCE_BYTES {
        notes.push(format!(
            "head of run {run_id}: evidence is {} bytes; the limit is {MAX_EVIDENCE_BYTES}",
            object.size
        ));
        return None;
    }
    let value: serde_json::Value = match serde_json::from_slice(&object.bytes) {
        Ok(v) => v,
        Err(_) => {
            notes.push(format!("head of run {run_id}: evidence is not JSON"));
            return None;
        }
    };
    let text = cline_message_in_evidence(&value)?;
    let cut: String = text.chars().take(MAX_CLINE_MESSAGE_CHARS).collect();
    let clean = redact_tokens(&display_text(&cut));
    (!clean.trim().is_empty()).then_some(clean)
}

/// The validator's record of what the delivery changed, when it is the record of this head.
async fn changes<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    head: &str,
    deps: &Deps<S, B, E>,
    notes: &mut Vec<String>,
) -> Changes {
    let key = changes_key(run_id);
    let object = match deps
        .blobs
        .read_prefix(&key, MAX_CHANGES_RECORD_BYTES as u64 + 1)
        .await
    {
        Ok(Some(o)) => o,
        // A delivery that predates the record, or a role that cannot tell absent from refused (403): either way there is no list to show.
        Ok(None) => return Changes::Unavailable,
        Err(e) => {
            notes.push(format!("head of run {run_id}: change record not read: {e}"));
            return Changes::Unavailable;
        }
    };
    match ChangeList::from_record(&object.bytes, run_id, head) {
        Ok(list) => Changes::Recorded(list),
        Err(e) => {
            notes.push(format!("head of run {run_id}: change record not used: {e}"));
            Changes::Unavailable
        }
    }
}
